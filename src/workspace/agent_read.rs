//! The agent's own connections, and the reads that run on them.
//!
//! An agent never shares the user's connection: its statements would queue
//! behind the user's, ride inside a transaction the user has open, and see
//! session settings the user changed. So each profile an agent reads gets a
//! [`Lane`], a connection opened from the profile's config for the agent alone,
//! held read-only whatever the profile's mode, and kept for the next call. The
//! user's profile is never connected, reconnected or marked by it.
//!
//! The statement is sent exactly as the agent wrote it, after
//! [`crate::sql::agent_gate`] judged that text. Nothing is appended to it,
//! wrapped around it or rewritten, so the gate's verdict is about what the
//! server receives.

use std::{
    collections::VecDeque,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use super::agent::{AuditVerdict, CallFacts, elapsed_ms};
use super::*;
use crate::{
    agent::{CallId, Reply, Status, result, result::Total, tools::Tool},
    db::{
        CancelToken, Catalog, Connection, ConnectionConfig, DbError, Fetch, QueryResult,
        RelationKind,
    },
};

// ponytail: an agent is told the first hundred rows, and the tool takes no
// count, so what a call can cost the model's context does not depend on what
// the model asks for. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_ROWS: usize = 100;

// ponytail: the longest an agent's statement runs when the profile sets no
// timeout of its own, so no agent connection is ever unbounded. A profile's own
// shorter timeout wins. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_TIMEOUT: u32 = 30;

// ponytail: the longest any one call lives, from the moment it starts to its
// answer: the open, a tunnel, a keychain prompt nobody answers and one
// statement at its [`AGENT_TIMEOUT`]. Time spent waiting behind other calls is
// not counted; it is bounded by their deadlines and by the call cap. The
// upgrade is a deadline per tool.
pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(60);

/// Said for a call that ran past [`CALL_DEADLINE`]. Names the same 60 seconds;
/// a test holds the two together, since a constant cannot be built from one.
const DEADLINE_PASSED: &str = "The call did not finish within 60 seconds.";

/// What opens the agent's connection to a profile and says what an error from
/// it must not name. `None` in [`AgentState::opener`](super::agent::AgentState::opener)
/// is [`open_agent`]; a test puts one here that never returns.
pub(crate) type Opener = fn(
    &gpui::BackgroundExecutor,
    ConnectionConfig,
    String,
) -> gpui::Task<Result<(Connection, Vec<String>), DbError>>;

/// The statement timeout in seconds an agent connection carries, given the
/// profile's own: the smaller of the two, with 0 (none) meaning the agent's.
pub(crate) fn agent_timeout(user: u32) -> u32 {
    match user {
        0 => AGENT_TIMEOUT,
        user => user.min(AGENT_TIMEOUT),
    }
}

/// The config an agent connection is opened from: the profile's, bounded by
/// [`agent_timeout`]. [`Lane::opened_from`] stays the profile's own config, the
/// value it is compared with.
pub(crate) fn agent_config(config: &ConnectionConfig) -> ConnectionConfig {
    config
        .clone()
        .with_statement_timeout(agent_timeout(config.statement_timeout()))
}

/// Said when the agent's connection could not be made. Names nothing about
/// where the database is: the reason is the server's and may carry a host, a
/// user or a path.
pub(crate) const OPEN_FAILED: &str =
    "DBDelve could not open the agent's connection to this database.";

// ponytail: Claude Code trims an error result past about 11,000 characters, so
// a statement's error is cut well before that. The upgrade is a setting.
pub(crate) const ERROR_CAP: usize = 4_000;

/// What replaces a hidden string in an error an agent reads.
const HIDDEN: &str = "[hidden]";

/// What an error cut at [`ERROR_CAP`] ends with.
const CLIPPED: &str = " [clipped]";

/// A character of a name: what a hidden string must not be glued to.
fn in_a_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `message` with every string of `hidden` replaced by `[hidden]` wherever it
/// stands whole, then cut to [`ERROR_CAP`] characters.
///
/// A string stands whole when the character before and after it, if its own
/// edge is a name character, is not one: `sa` goes from `by sa.` and `'sa'`
/// and stays in `usage`. There is no length below which a string is let
/// through. Case is ignored in ASCII, because a server re-cases what it echoes
/// (Snowflake upper-cases identifiers). Longer strings are tried first, so
/// `db.internal:5432` goes as one marker rather than a marker and a port. A
/// MySQL account, `'user'@'host'`, goes as one, whatever it is made of.
pub(crate) fn scrubbed(message: &str, hidden: &[String]) -> String {
    let mut hidden: Vec<&str> = hidden
        .iter()
        .map(String::as_str)
        .filter(|text| !text.is_empty())
        .collect();
    hidden.sort_by_key(|text| std::cmp::Reverse(text.len()));
    let bytes = message.as_bytes();
    let mut out = String::with_capacity(message.len());
    let mut at = 0;
    while at < message.len() {
        let end = account_end(message, at).or_else(|| {
            hidden.iter().find_map(|text| {
                let end = at + text.len();
                let first = text.as_bytes()[0];
                let last = text.as_bytes()[text.len() - 1];
                (bytes.get(at..end)?.eq_ignore_ascii_case(text.as_bytes())
                    && !(in_a_name(first) && at > 0 && in_a_name(bytes[at - 1]))
                    && !(in_a_name(last) && bytes.get(end).is_some_and(|&next| in_a_name(next))))
                .then_some(end)
            })
        });
        match end {
            Some(end) => {
                out.push_str(HIDDEN);
                at = end;
            }
            None => {
                let character = message[at..].chars().next().unwrap_or_default();
                out.push(character);
                at += character.len_utf8().max(1);
            }
        }
    }
    match out.char_indices().nth(ERROR_CAP) {
        Some((cut, _)) => {
            out.truncate(cut);
            out.push_str(CLIPPED);
            out
        }
        None => out,
    }
}

/// Where the account spelled `'user'@'host'` at `at` ends, if one starts there.
fn account_end(message: &str, at: usize) -> Option<usize> {
    let rest = message[at..].strip_prefix('\'')?;
    // The user ends at the next quote, which must open the host: a quote that
    // closes something else is not an account.
    let (_, after_user) = rest.split_once('\'')?;
    let host = after_user.strip_prefix("@'")?;
    let (_, after_host) = host.split_once('\'')?;
    Some(message.len() - after_host.len())
}

/// Said for a statement that needs the user's approval: it is not run for an
/// agent, and no connection is opened for it.
pub(crate) const NEEDS_APPROVAL: &str =
    "statements that need the user's approval are not run for agents";

/// The agent's calls on one profile, and the connection of its own they share.
///
/// One call runs at a time: the connection runs one statement, and a call that
/// arrived while another ran would otherwise be answered with the first's rows
/// or cancel the first's statement. Later calls wait here, in the order they
/// came, and never in [`AgentState::calls`](super::agent::AgentState::calls),
/// which holds only what a person is asked about: a read draws no dialog and
/// is not a waiting call.
pub(crate) struct Lane {
    /// The config `connection` was opened from, its password slot as the
    /// profile holds it (blank when the keychain has it), so a profile whose
    /// config has since changed is told apart from one that has not. Without a
    /// connection it is the config the next read opens from.
    pub(crate) opened_from: ConnectionConfig,
    /// `None` until a read opens it, and again after one that ended early or
    /// failed.
    pub(crate) connection: Option<Connection>,
    /// What an error that came back through this connection must not say
    /// ([`ConnectionConfig::identifying_text`] of the config it opened with,
    /// its password included). It lives exactly as long as the connection,
    /// which holds the same strings.
    pub(crate) hidden: Vec<String>,
    pub(crate) running: Option<Running>,
    pub(crate) waiting: VecDeque<Waiting>,
}

/// The call a lane is working on. Dropping it drops the work, which is what
/// keeps a result that comes back late from landing.
pub(crate) struct Running {
    pub(crate) id: CallId,
    tool: Tool,
    profile_name: String,
    reply: SyncSender<Reply>,
    /// The statement as the agent sent it, for the audit. A catalog call has
    /// none.
    sql: Option<String>,
    /// When the call reached the window, for the audit's elapsed time.
    arrived: Instant,
    /// What stops the statement on the server when the call ends early.
    cancel: CancelToken,
    /// The lane's connection the statement runs on, when the lane had one. A
    /// read that opens its own has none to hand until it lands.
    connection: Option<Connection>,
    _work: gpui::Task<()>,
    /// Ends the call at [`CALL_DEADLINE`]. Dropped with the call, whatever
    /// ended it, so it never fires for one that is over.
    _deadline: gpui::Task<()>,
}

/// A call that has not started: it holds its caller's reply and what to do.
pub(crate) struct Waiting {
    pub(crate) id: CallId,
    tool: Tool,
    profile_name: String,
    reply: SyncSender<Reply>,
    arrived: Instant,
    work: AgentWork,
}

/// What a waiting call will do once its lane is free.
pub(crate) enum AgentWork {
    Query {
        sql: String,
    },
    /// The agent connection's catalog: its schemas, or with `schema` the
    /// relations of that one. DBDelve reads it through the driver and sends no
    /// statement of its own for it.
    Catalog {
        schema: Option<String>,
    },
}

impl AgentWork {
    fn tool(&self) -> Tool {
        match self {
            AgentWork::Query { .. } => Tool::ExecuteSql,
            AgentWork::Catalog { schema: None } => Tool::ListSchemas,
            AgentWork::Catalog { schema: Some(_) } => Tool::ListTables,
        }
    }

    /// The statement the audit records, for the work that sends one.
    fn sql(&self) -> Option<String> {
        match self {
            AgentWork::Query { sql } => Some(sql.clone()),
            AgentWork::Catalog { .. } => None,
        }
    }
}

/// Said for a `list_tables` call whose schema the catalog does not hold.
pub(crate) const NO_SUCH_SCHEMA: &str = "there is no schema of that name";

/// What the audit learns of a read from how it ended, beyond what it sent.
#[derive(Default)]
pub(super) struct Ended {
    /// How many rows went back and how many there were, when it was answered
    /// with a result.
    pub(super) rows: Option<(usize, Total)>,
    /// The full error of a read that failed, which the agent was not given.
    pub(super) error: Option<String>,
}

/// What a read that ended records of itself: for a statement, that the gate
/// let it run and what it sent; for a catalog call, which has no statement
/// and so no verdict, nothing of either. Then how long it took, and what
/// `ended` adds.
fn facts_of(sql: Option<String>, arrived: Instant, ended: Ended) -> CallFacts {
    CallFacts {
        verdict: sql.is_some().then_some(AuditVerdict::Run),
        sql,
        shown: ended.rows.map(|(shown, _)| shown),
        total: ended.rows.map(|(_, total)| total),
        elapsed_ms: elapsed_ms(arrived),
        error: ended.error,
    }
}

/// What the blocking half of a read came back with.
#[allow(
    clippy::large_enum_variant,
    reason = "one value per read, moved straight into its landing"
)]
enum Ran {
    /// The connection could not be made, or could not be held read-only, for
    /// this reason.
    NotOpened(DbError),
    Done {
        connection: Connection,
        hidden: Vec<String>,
        result: Result<QueryResult, DbError>,
    },
    /// A catalog call came back: `None` is a schema the catalog does not hold.
    Listed {
        connection: Connection,
        hidden: Vec<String>,
        listed: Result<Option<Listing>, DbError>,
    },
}

/// A catalog reply and how many rows it lists.
struct Listing {
    text: String,
    rows: usize,
}

/// One result set as the agent reads it. A Postgres spatial cell is rendered
/// first, as an export renders it, so hex EWKB never spends the agent's tokens.
fn table_of(engine: Engine, result: &QueryResult) -> result::Table {
    let mut shown = result.rows.clone();
    for (row, column, _, value) in db::render(db::unrendered(engine, result)) {
        shown[row].replace(&[(column, Some(value.as_str()))]);
    }
    let rows: Vec<Vec<Option<String>>> = shown
        .iter()
        .map(|row| row.iter().map(|cell| cell.map(str::to_string)).collect())
        .collect();
    // A statement stopped at the limit has no known count: the server was not
    // asked for the rest.
    let total = if result.stopped.is_some() {
        result::Total::AtLeast(rows.len())
    } else {
        result::Total::Exact(result.capped_from.unwrap_or(rows.len()))
    };
    result::Table {
        label: "columns",
        heads: result
            .columns
            .iter()
            .map(|column| result::Head {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
            })
            .collect(),
        rows,
        total,
    }
}

impl Workspace {
    /// Take one statement the gate let through for `profile_id`: it runs on
    /// the agent's own connection to that profile when the lane is free, and
    /// after the calls ahead of it when not. Answers with its first rows.
    pub(crate) fn run_agent_sql(
        &mut self,
        id: CallId,
        profile_id: String,
        sql: String,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_work(id, profile_id, AgentWork::Query { sql }, reply, arrived, cx);
    }

    /// Take work for `profile_id`'s lane, on the same terms as
    /// [`Workspace::run_agent_sql`]: the profile must be one an agent may read,
    /// and the work waits its turn on the agent's own connection.
    pub(crate) fn run_agent_work(
        &mut self,
        id: CallId,
        profile_id: String,
        work: AgentWork,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
        else {
            // The profile went between the gate and here.
            self.agent_answered(
                id,
                work.tool(),
                None,
                None,
                &reply,
                Reply {
                    status: Status::RefusedByGate,
                    text: super::agent::AgentLookup::NotAvailable.to_string(),
                },
                CallFacts {
                    elapsed_ms: elapsed_ms(arrived),
                    ..CallFacts::default()
                },
            );
            return;
        };
        if let Some(refusal) = profile.config.agent_refusal() {
            // Before a lane exists: nothing is opened for a profile an agent
            // cannot read.
            let name = profile.name.clone();
            self.agent_answered(
                id,
                work.tool(),
                Some(name),
                None,
                &reply,
                Reply {
                    status: Status::RefusedByGate,
                    text: refusal.to_string(),
                },
                CallFacts {
                    // A catalog call has no statement to judge, so no verdict.
                    verdict: work
                        .sql()
                        .map(|_| AuditVerdict::Refuse(refusal.to_string())),
                    sql: work.sql(),
                    elapsed_ms: elapsed_ms(arrived),
                    ..CallFacts::default()
                },
            );
            return;
        }
        let (profile_name, config) = (profile.name.clone(), profile.config.clone());
        self.agent
            .lanes
            .entry(profile_id.clone())
            .or_insert_with(|| Lane {
                opened_from: config,
                connection: None,
                hidden: Vec::new(),
                running: None,
                waiting: VecDeque::new(),
            })
            .waiting
            .push_back(Waiting {
                id,
                tool: work.tool(),
                profile_name,
                reply,
                arrived,
                work,
            });
        self.start_next(&profile_id, cx);
    }

    /// Start the oldest waiting call of `profile_id`'s lane if nothing runs
    /// there, opening the lane's connection first when it has none or when the
    /// profile's config is no longer the one it opened from. A lane left with
    /// nothing to do and no connection is removed.
    pub(crate) fn start_next(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.running.is_some() {
            return;
        }
        if lane.waiting.is_empty() {
            if lane.connection.is_none() {
                self.agent.lanes.remove(profile_id);
            }
            return;
        }
        let Some(config) = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .map(|profile| profile.config.clone())
        else {
            // The profile went with calls still waiting: no sweep is meant to
            // leave one behind, and none is run for a profile that is gone.
            self.end_lane(
                profile_id,
                Status::Rejected,
                super::agent::NO_LONGER_AVAILABLE,
                cx,
            );
            return;
        };
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.opened_from != config {
            // ponytail: a changed connection is reopened on the next read
            // rather than on the change, since nothing is read until then.
            lane.connection = None;
            lane.hidden.clear();
            lane.opened_from = config.clone();
        }
        let Some(Waiting {
            id,
            tool,
            profile_name,
            reply,
            arrived,
            work,
        }) = lane.waiting.pop_front()
        else {
            return;
        };
        let cached = lane
            .connection
            .clone()
            .map(|connection| (connection, lane.hidden.clone()));
        let engine = config.engine();
        let cancel = CancelToken::default();
        let sent = work.sql();

        // Called here and not in the task, so what opens is decided when the
        // call starts.
        let opening = match cached {
            Some(cached) => gpui::Task::ready(Ok(cached)),
            None => {
                let open = self.agent.opener.unwrap_or(open_agent);
                open(cx.background_executor(), config, profile_id.to_string())
            }
        };
        let task = cx.spawn({
            let profile_id = profile_id.to_string();
            let cancel = cancel.clone();
            async move |workspace, cx| {
                let ran = match opening.await {
                    Ok((connection, hidden)) => {
                        spawn_blocking(cx.background_executor(), move || match work {
                            AgentWork::Query { sql } => {
                                read_on(connection, hidden, &sql, &cancel, engine)
                            }
                            AgentWork::Catalog { schema } => list_on(
                                connection,
                                hidden,
                                schema.as_deref(),
                                engine.agent_catalog_note(),
                            ),
                        })
                        .await
                    }
                    Err(error) => Ran::NotOpened(error),
                };
                // The window may have closed meanwhile, and then nobody is
                // left to answer.
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.land_agent_read(id, &profile_id, engine, ran, cx);
                });
            }
        });
        let deadline = cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(CALL_DEADLINE).await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_call(id, Status::Failed, DEADLINE_PASSED.to_string(), cx)
            });
        });
        if let Some(lane) = self.agent.lanes.get_mut(profile_id) {
            lane.running = Some(Running {
                id,
                tool,
                profile_name,
                reply,
                sql: sent,
                arrived,
                cancel,
                connection: lane.connection.clone(),
                _work: task,
                _deadline: deadline,
            });
        }
    }

    /// Take what a read came back with: keep its connection and answer. A read
    /// whose call ended meanwhile finds nothing running under its id, and what
    /// it came back with is dropped.
    fn land_agent_read(
        &mut self,
        id: CallId,
        profile_id: &str,
        engine: Engine,
        ran: Ran,
        cx: &mut Context<Self>,
    ) {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.running.as_ref().is_none_or(|running| running.id != id) {
            return;
        }
        let mut ended = Ended::default();
        let (status, text) = match ran {
            Ran::NotOpened(error) => {
                lane.connection = None;
                ended.error = Some(error.message);
                (Status::Failed, OPEN_FAILED.to_string())
            }
            Ran::Done {
                connection,
                hidden,
                result: Ok(result),
            } => {
                // A DuckDB connection is let go here, so the next call opens
                // afresh and sees what the user has committed since.
                if !lane.opened_from.agent_opens_per_call() {
                    lane.connection = Some(connection);
                    lane.hidden = hidden;
                }
                let table = table_of(engine, &result);
                ended.rows = Some((table.rows.len(), table.total));
                (
                    Status::Executed,
                    result::text(&[table], result.rest.len(), None),
                )
            }
            Ran::Listed {
                connection,
                hidden,
                listed: Ok(listed),
            } => {
                // Kept on the same terms as a read's: the catalog call ended
                // cleanly, and so did the connection it ran on.
                if !lane.opened_from.agent_opens_per_call() {
                    lane.connection = Some(connection);
                    lane.hidden = hidden;
                }
                match listed {
                    Some(Listing { text, rows }) => {
                        ended.rows = Some((rows, Total::Exact(rows)));
                        (Status::Executed, text)
                    }
                    None => (Status::Failed, NO_SUCH_SCHEMA.to_string()),
                }
            }
            Ran::Done {
                hidden,
                result: Err(error),
                ..
            }
            | Ran::Listed {
                hidden,
                listed: Err(error),
                ..
            } => {
                // ponytail: every failure ends the connection rather than
                // asking which ones end a session, since a Redis timeout
                // closes it without `is_lost` knowing (D-02). The cost is one
                // reopen after a failed statement.
                lane.connection = None;
                let text = scrubbed(&error.message, &hidden);
                ended.error = Some(error.message);
                (Status::Failed, text)
            }
        };
        // An answer that is not an error leaves the connection, and what an
        // error through it must not say, as they were.
        if status == Status::Failed && lane.connection.is_none() {
            lane.hidden.clear();
        }
        self.finish_read(id, status, text, false, ended, cx);
    }

    /// End the read `id`, running or waiting, once: it is removed first, so a
    /// call that two things end at once is answered and recorded by whichever
    /// comes first. `early` is an end that is not the statement's own answer:
    /// the statement is cancelled on the server and the lane's connection is
    /// dropped. `ended` is what the audit learns from the end: the rows that went
    /// back, or the full error. Then the lane's next call starts. Answers whether there was
    /// such a call.
    pub(super) fn finish_read(
        &mut self,
        id: CallId,
        status: Status,
        text: String,
        early: bool,
        mut ended: Ended,
        cx: &mut Context<Self>,
    ) -> bool {
        let found = self.agent.lanes.iter_mut().find_map(|(profile_id, lane)| {
            if lane
                .running
                .as_ref()
                .is_some_and(|running| running.id == id)
            {
                let running = lane.running.take()?;
                if early {
                    // ponytail: the connection is dropped rather than reused,
                    // because a cancelled statement may still be finishing on
                    // it and a cancel stops whatever the connection is running,
                    // which would reach the next call. The cost is one reopen.
                    lane.connection = None;
                    lane.hidden.clear();
                }
                let stop = running
                    .connection
                    .map(|connection| (connection, running.cancel));
                return Some((
                    profile_id.clone(),
                    running.tool,
                    running.profile_name,
                    running.reply,
                    facts_of(running.sql, running.arrived, std::mem::take(&mut ended)),
                    stop.filter(|_| early),
                ));
            }
            let place = lane.waiting.iter().position(|waiting| waiting.id == id)?;
            let waiting = lane.waiting.remove(place)?;
            Some((
                profile_id.clone(),
                waiting.tool,
                waiting.profile_name,
                waiting.reply,
                facts_of(
                    waiting.work.sql(),
                    waiting.arrived,
                    std::mem::take(&mut ended),
                ),
                None,
            ))
        });
        let Some((profile_id, tool, name, reply, facts, stop)) = found else {
            return false;
        };
        if let Some((connection, cancel)) = stop {
            // A thread of its own: the pool may be full of the very statements
            // this stops.
            spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel)).detach();
        }
        if status != Status::Cancelled {
            // The caller may have hung up, and then nobody reads this.
            let _ = reply.send(Reply { status, text });
        }
        self.agent.record(id, tool, Some(name), None, status, facts);
        self.start_next(&profile_id, cx);
        cx.notify();
        true
    }

    /// End every read of `profile_id`'s lane, the running one first and then
    /// the waiting ones in the order they came, each with `status` and `text`,
    /// and drop the lane with its connection. Nothing of the lane starts again.
    pub(crate) fn end_lane(
        &mut self,
        profile_id: &str,
        status: Status,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(mut lane) = self.agent.lanes.remove(profile_id) else {
            return;
        };
        let running = lane.running.take();
        if let Some(running) = &running
            && let Some(connection) = running.connection.clone()
        {
            let cancel = running.cancel.clone();
            spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel)).detach();
        }
        let ended = running
            .map(|running| {
                (
                    running.id,
                    running.tool,
                    running.profile_name,
                    running.reply,
                    facts_of(running.sql, running.arrived, Ended::default()),
                )
            })
            .into_iter()
            .chain(lane.waiting.drain(..).map(|waiting| {
                (
                    waiting.id,
                    waiting.tool,
                    waiting.profile_name,
                    waiting.reply,
                    facts_of(waiting.work.sql(), waiting.arrived, Ended::default()),
                )
            }));
        for (id, tool, name, reply, facts) in ended {
            let _ = reply.send(Reply {
                status,
                text: text.to_string(),
            });
            self.agent.record(id, tool, Some(name), None, status, facts);
        }
        cx.notify();
    }
}

/// Open the agent's connection to a profile and hold it read-only, with the
/// strings an error through it must not name.
fn open_agent(
    executor: &gpui::BackgroundExecutor,
    config: ConnectionConfig,
    profile_id: String,
) -> gpui::Task<Result<(Connection, Vec<String>), DbError>> {
    spawn_blocking(executor, move || {
        let mut config = agent_config(&config);
        // A file engine has nothing to authenticate to, so it never reaches
        // the keychain.
        if let Some(slot) = config.password_mut()
            && slot.is_empty()
        {
            match store::password(&profile_id) {
                Ok(Some(password)) => *slot = password,
                Ok(None) => {}
                Err(error) => {
                    return Err(DbError {
                        message: error,
                        position: None,
                    });
                }
            }
        }
        // Taken after the password is in: it is one of the strings a server's
        // error may echo.
        let hidden = config.identifying_text();
        // Opened through `open_for_agent`: the hold is what makes a read the gate
        // misjudged harmless, so a connection that cannot take it is not one the
        // agent may use.
        let connection = Connection::open_for_agent(config)?;
        Ok((connection, hidden))
    })
}

/// The blocking half of a read: run the statement on the agent's connection as
/// written.
fn read_on(
    connection: Connection,
    hidden: Vec<String>,
    sql: &str,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    let result = connection.query(
        sql,
        cancel,
        Fetch {
            limit: Some(AGENT_ROWS),
            reads_only: crate::sql::stoppable(engine, sql),
            ..Fetch::default()
        },
    );
    Ran::Done {
        connection,
        hidden,
        result,
    }
}

/// The blocking half of a catalog call: read the catalog on the agent's
/// connection and write it as the agent reads it. `schema` is `None` for the
/// schemas and a name for the relations of that one.
fn list_on(
    connection: Connection,
    hidden: Vec<String>,
    schema: Option<&str>,
    note: Option<&str>,
) -> Ran {
    let listed = list(&connection, schema, note);
    Ran::Listed {
        connection,
        hidden,
        listed,
    }
}

fn list(
    connection: &Connection,
    schema: Option<&str>,
    note: Option<&str>,
) -> Result<Option<Listing>, DbError> {
    let mut catalog = connection.catalog()?;
    let Some(schema) = schema else {
        let rows = catalog.schemas.len();
        let text = schemas_text(&catalog, note);
        return Ok(Some(Listing { text, rows }));
    };
    let Some(rows) = catalog
        .schemas
        .iter()
        .find(|held| held.name == schema)
        .map(|held| held.relations.len())
    else {
        // Nothing is asked of the server for a schema that is not there.
        return Ok(None);
    };
    // The estimate is optional, and the query behind it can need privileges
    // the catalog does not, so a failure here leaves the rows `null`.
    if let Ok(sizes) = connection.sizes() {
        catalog.set_sizes(&sizes);
    }
    Ok(tables_text(&catalog, schema, note).map(|text| Listing { text, rows }))
}

fn table_of_names(heads: &[&str], rows: Vec<Vec<Option<String>>>) -> result::Table {
    result::Table {
        label: "columns",
        heads: heads
            .iter()
            .map(|name| result::Head {
                name: name.to_string(),
                data_type: None,
            })
            .collect(),
        total: Total::Exact(rows.len()),
        rows,
    }
}

/// The reply for `list_schemas`: one row per schema, in name order. The names
/// are the database's own and go out as they are; `note` is DBDelve's.
fn schemas_text(catalog: &Catalog, note: Option<&str>) -> String {
    let rows = catalog
        .by_name()
        .into_iter()
        .map(|(_, schema)| vec![Some(schema.name.clone())])
        .collect();
    result::text(&[table_of_names(&["name"], rows)], 0, note)
}

/// The reply for `list_tables`: one row per relation of `schema`, with its
/// kind, the relation it is a partition of and the row estimate, `null` where
/// there is none. `None` is a schema the catalog does not hold.
fn tables_text(catalog: &Catalog, schema: &str, note: Option<&str>) -> Option<String> {
    let schema = catalog.schemas.iter().find(|held| held.name == schema)?;
    let rows = schema
        .relations
        .iter()
        .map(|relation| {
            vec![
                Some(relation.name.clone()),
                Some(kind_name(relation.kind).to_string()),
                relation.partition_of.clone(),
                relation.rows.map(|rows| rows.to_string()),
            ]
        })
        .collect();
    let heads = ["name", "kind", "partition_of", "rows"];
    Some(result::text(&[table_of_names(&heads, rows)], 0, note))
}

fn kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "table",
        RelationKind::PartitionedTable => "partitioned table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
        RelationKind::Dictionary => "dictionary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation(
        name: &str,
        kind: RelationKind,
        parent: Option<&str>,
        rows: Option<u64>,
    ) -> crate::db::Relation {
        crate::db::Relation {
            name: name.to_string(),
            kind,
            partition_of: parent.map(str::to_string),
            size: None,
            rows,
        }
    }

    fn schema(name: &str, relations: Vec<crate::db::Relation>) -> crate::db::Schema {
        crate::db::Schema {
            name: name.to_string(),
            relations,
            routines: Vec::new(),
        }
    }

    #[test]
    fn tables_are_listed_with_their_kind_parent_and_estimate() {
        let catalog = Catalog {
            schemas: vec![schema(
                "public",
                vec![
                    relation("orders", RelationKind::Table, None, Some(42)),
                    relation("orders_2024", RelationKind::Table, Some("orders"), None),
                    relation("v", RelationKind::View, None, None),
                ],
            )],
            database: None,
        };
        let text = tables_text(&catalog, "public", None).expect("the schema is there");
        assert_eq!(
            text,
            "truncated: none (3 rows)\n\
             columns: [{\"name\":\"name\",\"type\":null},{\"name\":\"kind\",\"type\":null},\
             {\"name\":\"partition_of\",\"type\":null},{\"name\":\"rows\",\"type\":null}]\n\
             [\"orders\",\"table\",null,\"42\"]\n\
             [\"orders_2024\",\"table\",\"orders\",null]\n\
             [\"v\",\"view\",null,null]\n"
        );
        assert_eq!(tables_text(&catalog, "Public", None), None);
    }

    #[test]
    fn a_schema_with_no_relations_lists_none() {
        let catalog = Catalog {
            schemas: vec![schema("empty", Vec::new())],
            database: None,
        };
        let text = tables_text(&catalog, "empty", None).expect("the schema is there");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("truncated: none (0 rows)"));
        assert!(
            lines
                .next()
                .is_some_and(|line| line.starts_with("columns: "))
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn a_redis_catalog_reply_carries_the_preview_note() {
        let catalog = Catalog {
            schemas: vec![schema(
                "db0",
                vec![relation("keys", RelationKind::Table, None, Some(7))],
            )],
            database: None,
        };
        let note = Engine::Redis.agent_catalog_note();
        let note_line = format!("note: {}\n", note.expect("Redis has a note"));
        let schemas = schemas_text(&catalog, note);
        assert!(
            schemas.starts_with(&format!("truncated: none (1 row)\n{note_line}columns: ")),
            "{schemas}"
        );
        let tables = tables_text(&catalog, "db0", note).expect("the schema is there");
        assert!(
            tables.starts_with(&format!("truncated: none (1 row)\n{note_line}columns: ")),
            "{tables}"
        );
        assert!(
            tables.ends_with("[\"keys\",\"table\",null,\"7\"]\n"),
            "{tables}"
        );
        // Another engine's reply has no such line.
        assert!(!schemas_text(&catalog, Engine::Sqlite.agent_catalog_note()).contains("note: "));
    }

    #[test]
    fn schemas_are_listed_in_name_order() {
        let catalog = Catalog {
            schemas: vec![schema("b", Vec::new()), schema("a", Vec::new())],
            database: None,
        };
        assert_eq!(
            schemas_text(&catalog, None),
            "truncated: none (2 rows)\n\
             columns: [{\"name\":\"name\",\"type\":null}]\n\
             [\"a\"]\n\
             [\"b\"]\n"
        );
    }

    #[test]
    fn every_kind_of_relation_has_its_own_name() {
        let kinds = [
            RelationKind::Table,
            RelationKind::PartitionedTable,
            RelationKind::View,
            RelationKind::MaterializedView,
            RelationKind::ForeignTable,
            RelationKind::Dictionary,
        ];
        let names: std::collections::HashSet<_> = kinds.into_iter().map(kind_name).collect();
        assert_eq!(names.len(), kinds.len());
    }

    #[test]
    fn the_deadline_text_names_the_deadline() {
        assert!(DEADLINE_PASSED.contains(&format!("{} seconds", CALL_DEADLINE.as_secs())));
    }

    #[test]
    fn the_agent_timeout_is_the_users_capped_or_the_agents_own() {
        assert_eq!(agent_timeout(0), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(5), 5);
        assert_eq!(agent_timeout(AGENT_TIMEOUT), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(AGENT_TIMEOUT + 1), AGENT_TIMEOUT);
        assert_eq!(agent_timeout(u32::MAX), AGENT_TIMEOUT);
    }

    #[test]
    fn the_agent_config_carries_the_capped_timeout() {
        for config in crate::db::every_shape() {
            for user in [0, 5, AGENT_TIMEOUT, AGENT_TIMEOUT + 1] {
                let profile = config.clone().with_statement_timeout(user);
                let agent = agent_config(&profile);
                let engine = profile.engine();
                assert_eq!(
                    agent.statement_timeout(),
                    agent_timeout(user),
                    "{engine:?} at {user}"
                );
                // Only the timeout differs from what the profile holds.
                assert_eq!(agent.with_statement_timeout(user), profile, "{engine:?}");
            }
        }
    }

    /// Whether `text` holds `name` bounded by anything but a name character,
    /// in any ASCII case.
    fn stands_whole(text: &str, name: &str) -> bool {
        let (text, name) = (text.to_ascii_lowercase(), name.to_ascii_lowercase());
        let in_a_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(&name).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].chars().next();
            let first = name.chars().next().is_some_and(in_a_name);
            let last = name.chars().next_back().is_some_and(in_a_name);
            !(first && before.is_some_and(in_a_name)) && !(last && after.is_some_and(in_a_name))
        })
    }

    #[test]
    fn an_error_naming_the_profile_is_scrubbed_before_the_agent_reads_it() {
        let configs = restored(include_str!("../testdata/every-engine.toml"));
        assert!(!configs.is_empty());
        for (id, config) in &configs {
            let hidden = config.identifying_text();
            for name in &hidden {
                for message in [
                    format!("could not reach {name}."),
                    format!("user '{name}' denied"),
                    format!("\"{name}\""),
                    format!("{name}@host"),
                    name.to_ascii_uppercase(),
                ] {
                    let clean = scrubbed(&message, &hidden);
                    assert!(
                        !stands_whole(&clean, name),
                        "{id}: {name:?} survives in {clean:?} from {message:?}"
                    );
                }
            }
        }
        assert_eq!(
            scrubbed(
                "SELECT command denied to user 'app'@'10.0.0.5' for table 't'",
                &[]
            ),
            "SELECT command denied to user [hidden] for table 't'"
        );
        let long = "x".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&long, &[]);
        assert_eq!(clipped.chars().count(), ERROR_CAP + CLIPPED.chars().count());
        assert!(clipped.ends_with(" [clipped]"));
        let wide = "é".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&wide, &["é".to_string()]);
        assert!(clipped.chars().count() <= ERROR_CAP + CLIPPED.chars().count());
        let exact = "y".repeat(ERROR_CAP);
        assert_eq!(scrubbed(&exact, &[]), exact);
        let multibyte = "é".repeat(ERROR_CAP + 500);
        let clipped = scrubbed(&multibyte, &[]);
        assert!(clipped.starts_with("éé") && clipped.ends_with(" [clipped]"));
    }

    #[test]
    fn a_name_is_scrubbed_only_where_it_stands_whole() {
        let hidden: Vec<String> = ["sa", "db.internal", "db.internal:5432"]
            .map(String::from)
            .into();
        for (message, scrubbed_to) in [
            ("usage of sa_role by sa.", "usage of sa_role by [hidden]."),
            ("cannot reach db.internal.", "cannot reach [hidden]."),
            ("at db.internal:5432,", "at [hidden],"),
            ("subdb.internal", "subdb.internal"),
            (
                "SA 'sa' \"sa\" sa@host",
                "[hidden] '[hidden]' \"[hidden]\" [hidden]@host",
            ),
        ] {
            assert_eq!(scrubbed(message, &hidden), scrubbed_to, "{message}");
        }
    }

    #[test]
    fn a_quote_that_closes_something_else_is_not_an_account() {
        assert_eq!(
            scrubbed("user 'x' near 'app'@'host' denied", &[]),
            "user 'x' near [hidden] denied"
        );
        assert_eq!(scrubbed("it's 'a' and b@c", &[]), "it's 'a' and b@c");
    }

    #[test]
    fn a_spatial_cell_reaches_the_agent_rendered_not_as_hex() {
        use crate::db::{Column, Engine, Row};
        let point = "0101000000000000000000F03F0000000000000040";
        let result = QueryResult {
            columns: vec![
                Column {
                    name: "geom".into(),
                    data_type: Some("geometry".into()),
                },
                Column {
                    name: "note".into(),
                    data_type: Some("text".into()),
                },
            ],
            rows: vec![Row::new([Some(point), Some(point)]).expect("a row")],
            ..QueryResult::default()
        };
        let table = table_of(Engine::Postgres, &result);
        let geometry = table.rows[0][0].as_deref().expect("a cell");
        assert!(geometry.starts_with("POINT"), "{geometry}");
        assert_eq!(table.rows[0][1].as_deref(), Some(point));
        // Another engine's cells are not read as EWKB.
        let other = table_of(Engine::MySql, &result);
        assert_eq!(other.rows[0][0].as_deref(), Some(point));
    }
}
