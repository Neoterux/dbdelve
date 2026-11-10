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
//! server receives. The one exception is `explain_query`, which sends the
//! engine's EXPLAIN of the statement: [`explain_statement`] composes it first
//! and the gate judges that composed text, so the verdict is again about what
//! the server receives.

use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use super::agent::{AuditVerdict, CallFacts, elapsed_ms};
use super::*;
use crate::{
    agent::{CallId, Reply, Status, result, result::Total, tools::Tool},
    db::{
        CancelToken, Catalog, Connection, ConnectionConfig, DbError, ExplainMode, Fetch, IndexKind,
        QueryResult, RelationKind, Structure,
    },
    explain::compact,
    sql::{AgentVerdict, agent_gate},
};

// ponytail: an agent is told the first hundred rows, and the tool takes no
// count, so what a call can cost the model's context does not depend on what
// the model asks for. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_ROWS: usize = 100;

// ponytail: the most rows of a plan that are read back. A real plan is tens of
// lines, and an ANALYZE has already run the statement by the time they arrive,
// so the rows are drained rather than stopped; the ceiling only keeps a server
// that answers a plan with a table from filling memory. The upgrade is a setting,
// if a plan ever needs more.
pub(crate) const PLAN_ROWS: usize = 10_000;

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

/// The profile an agent's call resolved to, taken from the list in the turn
/// that resolved it. [`Workspace::run_agent_work`] is handed this and not an id
/// to look up again: nothing between the two can change the list.
pub(crate) struct Resolved {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) config: ConnectionConfig,
}

impl Resolved {
    pub(super) fn of(profile: &Profile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            config: profile.config.clone(),
        }
    }
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
    /// The connection the statement runs on, held here as soon as it exists so
    /// a cancel can reach it: a call that opens its own, as the first of a lane
    /// and every DuckDB call do, has none until its open returns. `None` only
    /// while that open is still in flight, when no statement has been sent.
    connection: Option<Connection>,
    _work: gpui::Task<()>,
    /// Ends the call at [`CALL_DEADLINE`]. Dropped with the call, whatever
    /// ended it, so it never fires for one that is over.
    _deadline: gpui::Task<()>,
}

impl Running {
    /// Whether a cancel has a connection to go to.
    #[cfg(test)]
    pub(crate) fn holds_connection(&self) -> bool {
        self.connection.is_some()
    }
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
#[derive(Clone)]
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
    /// One relation's definition, read through the driver like the catalog.
    Structure {
        schema: String,
        table: String,
    },
    /// The engine's EXPLAIN of the agent's statement, already composed and
    /// already judged by the gate as this exact text.
    Explain {
        statement: String,
    },
}

impl AgentWork {
    fn tool(&self) -> Tool {
        match self {
            AgentWork::Query { .. } => Tool::ExecuteSql,
            AgentWork::Catalog { schema: None } => Tool::ListSchemas,
            AgentWork::Catalog { schema: Some(_) } => Tool::ListTables,
            AgentWork::Structure { .. } => Tool::DescribeTable,
            AgentWork::Explain { .. } => Tool::ExplainQuery,
        }
    }

    /// The statement the audit records, for the work that sends one.
    fn sql(&self) -> Option<String> {
        match self {
            AgentWork::Query { sql } => Some(sql.clone()),
            AgentWork::Explain { statement } => Some(statement.clone()),
            AgentWork::Catalog { .. } | AgentWork::Structure { .. } => None,
        }
    }
}

/// Said for a `list_tables` call whose schema the catalog does not hold.
pub(crate) const NO_SUCH_SCHEMA: &str = "there is no schema of that name";

/// Said for a `describe_table` call whose schema or relation the catalog does
/// not hold.
pub(crate) const NOT_IN_CATALOG: &str = "there is no table or view of that name in that schema";

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
        result: Result<Shaped, DbError>,
    },
    /// A catalog call came back: `None` is a name the catalog does not hold.
    Listed {
        connection: Connection,
        hidden: Vec<String>,
        listed: Result<Option<Listing>, DbError>,
    },
}

impl Ran {
    /// Whether the statement failed and the session it ran on is gone, so
    /// nothing sent on that connection again can succeed.
    fn session_lost(&self) -> bool {
        match self {
            Ran::Done {
                connection,
                result: Err(_),
                ..
            }
            | Ran::Listed {
                connection,
                listed: Err(_),
                ..
            } => connection.is_lost(),
            Ran::NotOpened(_) | Ran::Done { .. } | Ran::Listed { .. } => false,
        }
    }
}

/// What a statement's result became for the agent, written on the thread that
/// ran the statement: a result can hold one very large cell, and copying it
/// into text is not work for the thread that draws the window.
struct Shaped {
    text: String,
    /// How many rows went back and how many there were.
    rows: (usize, Total),
}

/// A catalog reply and how many rows it lists.
struct Listing {
    text: String,
    rows: usize,
}

/// One result set as the agent reads it. A Postgres spatial cell is rendered
/// first, as an export renders it, so hex EWKB never spends the agent's tokens.
pub(super) fn table_of(engine: Engine, result: &QueryResult) -> result::Table {
    // Only the cells that wait on their type are copied out and replaced; the
    // rest are read where they stand.
    let rendered: HashMap<(usize, usize), String> = db::render(db::unrendered(engine, result))
        .into_iter()
        .map(|(row, column, _, value)| ((row, column), value))
        .collect();
    let rows: Vec<Vec<Option<String>>> = result
        .rows
        .iter()
        .enumerate()
        .map(|(at, row)| {
            row.iter()
                .enumerate()
                .map(|(column, cell)| {
                    rendered
                        .get(&(at, column))
                        .cloned()
                        .or_else(|| cell.map(str::to_string))
                })
                .collect()
        })
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

/// A statement's rows as the agent reads them.
fn shaped(engine: Engine, result: &QueryResult) -> Shaped {
    let table = table_of(engine, result);
    let rows = (table.rows.len(), table.total);
    Shaped {
        text: result::text(&[table], result.rest.len(), None),
        rows,
    }
}

/// A plan as the agent reads it.
fn shaped_plan(engine: Engine, result: &QueryResult) -> Shaped {
    let (_, text) = plan_reply(engine, result);
    Shaped {
        text,
        rows: (result.rows.len(), Total::Exact(result.rows.len())),
    }
}

impl Workspace {
    /// Take one statement the gate let through for `profile_id`: it runs on
    /// the agent's own connection to that profile when the lane is free, and
    /// after the calls ahead of it when not. Answers with its first rows.
    pub(crate) fn run_agent_sql(
        &mut self,
        id: CallId,
        profile: Resolved,
        sql: String,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_work(id, profile, AgentWork::Query { sql }, reply, arrived, cx);
    }

    /// Take work for `profile`'s lane, on the same terms as
    /// [`Workspace::run_agent_sql`]: the profile must be one an agent may read,
    /// and the work waits its turn on the agent's own connection.
    pub(crate) fn run_agent_work(
        &mut self,
        id: CallId,
        profile: Resolved,
        work: AgentWork,
        reply: SyncSender<Reply>,
        arrived: Instant,
        cx: &mut Context<Self>,
    ) {
        let Resolved {
            id: profile_id,
            name: profile_name,
            config,
        } = profile;
        if let Some(refusal) = config.agent_refusal() {
            // Before a lane exists: nothing is opened for a profile an agent
            // cannot read.
            self.agent_answered(
                id,
                work.tool(),
                Some(profile_name),
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
            // Not expected to run: the only place a profile leaves the list is
            // `remove_profile`, which ends this lane in the same turn through
            // `reject_calls_for`. Kept so that if a profile ever did go with
            // calls waiting, each is answered and none is stranded.
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
        let open = self.agent.opener.unwrap_or(open_agent);
        let mut reused = cached.is_some();
        let mut opening = match cached {
            Some(cached) => gpui::Task::ready(Ok(cached)),
            None => open(
                cx.background_executor(),
                config.clone(),
                profile_id.to_string(),
            ),
        };
        let task = cx.spawn({
            let profile_id = profile_id.to_string();
            let cancel = cancel.clone();
            async move |workspace, cx| {
                let ran = loop {
                    let (connection, hidden) = match opening.await {
                        Ok(opened) => opened,
                        Err(error) => break Ran::NotOpened(error),
                    };
                    // Before the statement is sent, so every end of the call
                    // after this point has a connection to cancel on. A call
                    // that ended during the open finds nothing running under
                    // its id, and sends no statement.
                    let held = workspace.update(cx, |workspace, _| {
                        workspace.hold_agent_connection(id, &profile_id, &connection)
                    });
                    if !held.unwrap_or(false) {
                        return;
                    }
                    let (ran, lost) = spawn_blocking(cx.background_executor(), {
                        let (work, cancel) = (work.clone(), cancel.clone());
                        move || {
                            let ran = run_work(work, connection, hidden, &cancel, engine);
                            // Asked only of a connection that sat idle between
                            // calls and then failed: the driver learns the
                            // server dropped it from the statement that finds
                            // out, so nothing before one can tell.
                            let lost = reused && ran.session_lost();
                            (ran, lost)
                        }
                    })
                    .await;
                    if !lost {
                        break ran;
                    }
                    // ponytail: the one reopen is a retry of a read that
                    // failed on a dead session, and a read that died with the
                    // session while running is run again, inside the same
                    // call deadline. The upgrade is telling the two apart.
                    reused = false;
                    opening = open(cx.background_executor(), config.clone(), profile_id.clone());
                };
                // The window may have closed meanwhile, and then nobody is
                // left to answer.
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.land_agent_read(id, &profile_id, ran, cx);
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
                connection: None,
                _work: task,
                _deadline: deadline,
            });
        }
    }

    /// Remember `connection` as the one running call `id` is on, so a hangup,
    /// the deadline, a hide or the kill switch can cancel the statement there.
    /// Answers whether `id` is still the call running on `profile_id`'s lane.
    fn hold_agent_connection(
        &mut self,
        id: CallId,
        profile_id: &str,
        connection: &Connection,
    ) -> bool {
        let running = self
            .agent
            .lanes
            .get_mut(profile_id)
            .and_then(|lane| lane.running.as_mut())
            .filter(|running| running.id == id);
        match running {
            Some(running) => {
                running.connection = Some(connection.clone());
                true
            }
            None => false,
        }
    }

    /// Take what a read came back with: keep its connection and answer. A read
    /// whose call ended meanwhile finds nothing running under its id, and what
    /// it came back with is dropped.
    fn land_agent_read(&mut self, id: CallId, profile_id: &str, ran: Ran, cx: &mut Context<Self>) {
        let Some(lane) = self.agent.lanes.get_mut(profile_id) else {
            return;
        };
        if lane.running.as_ref().is_none_or(|running| running.id != id) {
            return;
        }
        let mut ended = Ended::default();
        // What a catalog call that found nothing says: which tool asked decides
        // whether it was a schema or a relation.
        let tool = lane.running.as_ref().map(|running| running.tool);
        let not_found = match tool {
            Some(Tool::DescribeTable) => NOT_IN_CATALOG,
            _ => NO_SUCH_SCHEMA,
        };
        let (status, text) = match ran {
            Ran::NotOpened(error) => {
                lane.connection = None;
                ended.error = Some(error.message);
                (Status::Failed, OPEN_FAILED.to_string())
            }
            Ran::Done {
                connection,
                hidden,
                result: Ok(Shaped { text, rows }),
            } => {
                // A DuckDB connection is let go here, so the next call opens
                // afresh and sees what the user has committed since.
                if !lane.opened_from.agent_opens_per_call() {
                    lane.connection = Some(connection);
                    lane.hidden = hidden;
                }
                ended.rows = Some(rows);
                (Status::Executed, text)
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
                    None => (Status::Failed, not_found.to_string()),
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

/// The blocking half of any call: what `work` asks, on `connection`.
fn run_work(
    work: AgentWork,
    connection: Connection,
    hidden: Vec<String>,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    match work {
        AgentWork::Query { sql } => read_on(connection, hidden, &sql, cancel, engine),
        AgentWork::Catalog { schema } => list_on(
            connection,
            hidden,
            schema.as_deref(),
            engine.agent_catalog_note(),
        ),
        AgentWork::Explain { statement } => {
            explain_on(connection, hidden, &statement, cancel, engine)
        }
        AgentWork::Structure { schema, table } => describe_on(
            connection,
            hidden,
            &schema,
            &table,
            engine.agent_catalog_note(),
        ),
    }
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
        result: result.map(|result| shaped(engine, &result)),
    }
}

/// The blocking half of `explain_query`: send the composed EXPLAIN as it is
/// and read the whole plan back. Drained, never stopped at a limit: an ANALYZE
/// has already run the statement it wraps when the first row arrives.
fn explain_on(
    connection: Connection,
    hidden: Vec<String>,
    statement: &str,
    cancel: &CancelToken,
    engine: Engine,
) -> Ran {
    let result = connection.query(
        statement,
        cancel,
        Fetch {
            limit: Some(PLAN_ROWS),
            ..Fetch::default()
        },
    );
    Ran::Done {
        connection,
        hidden,
        result: result.map(|result| shaped_plan(engine, &result)),
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

/// The blocking half of `describe_table`: see [`describe`].
fn describe_on(
    connection: Connection,
    hidden: Vec<String>,
    schema: &str,
    table: &str,
    note: Option<&str>,
) -> Ran {
    let listed = connection.catalog().and_then(|catalog| {
        describe(&catalog, schema, table, note, |s, t| {
            connection.structure(s, t)
        })
    });
    Ran::Listed {
        connection,
        hidden,
        listed,
    }
}

/// Describe `table` of `schema`, once the catalog holds both by exact name.
///
/// The catalog is looked in first, and `load` is called only for a pair found
/// there: MongoDB opens whatever database it is given, so a name the agent
/// made up must never reach a loader. `None` is a pair the catalog does not
/// hold.
fn describe(
    catalog: &Catalog,
    schema: &str,
    table: &str,
    note: Option<&str>,
    load: impl FnOnce(&str, &str) -> Result<Structure, DbError>,
) -> Result<Option<Listing>, DbError> {
    let held = catalog
        .schemas
        .iter()
        .find(|held| held.name == schema)
        .is_some_and(|held| held.relations.iter().any(|relation| relation.name == table));
    if !held {
        return Ok(None);
    }
    let structure = load(schema, table)?;
    let rows = structure.columns.len();
    Ok(Some(Listing {
        text: structure_text(&structure, note),
        rows,
    }))
}

/// Said for a statement that is already an EXPLAIN: wrapping it again would
/// ask the server to explain an explanation.
pub(crate) const ALREADY_EXPLAIN: &str = "the statement is already an EXPLAIN";

/// Said for an engine that has no way to ask for a plan.
fn no_plan(engine: Engine) -> String {
    format!("DBDelve does not read query plans from {}.", engine.label())
}

/// Said for `analyze` on an engine that can plan but not measure. Nothing is
/// run, and a plain plan is not given in its place.
fn no_analyzed_plan(engine: Engine) -> String {
    format!(
        "DBDelve reads no analyzed plan from {}; nothing was run.",
        engine.label()
    )
}

/// Said for an answer that held no plan node.
fn empty_plan(engine: Engine) -> String {
    format!("{} returned no plan DBDelve could read.", engine.label())
}

/// What `explain_statement` answered without sending anything: how, and what
/// the audit records of it. `verdict` and `sql` are set when the gate judged a
/// composed statement.
#[derive(Debug)]
pub(crate) struct Declined {
    pub(crate) status: Status,
    pub(crate) text: String,
    pub(crate) verdict: Option<AgentVerdict>,
    pub(crate) sql: Option<String>,
}

impl Declined {
    fn said(status: Status, text: String) -> Self {
        Declined {
            status,
            text,
            verdict: None,
            sql: None,
        }
    }
}

/// The statement an `explain_query` call sends, or the answer it gets instead.
///
/// Every check comes before anything is sent, in this order: an input that is
/// already an EXPLAIN; an engine with no plan; `analyze` on an engine with no
/// analyzed plan, which is never answered with a plain plan, and where the
/// engine composes its EXPLAIN; a statement the engine cannot explain. Then
/// [`agent_gate`] judges that composed text, so an ANALYZE follows the verdict
/// of the statement it wraps. Only `Run` returns the statement, and that string
/// is what is sent.
pub(crate) fn explain_statement(
    engine: Engine,
    sql: &str,
    analyze: bool,
    mode: Mode,
) -> Result<String, Declined> {
    if crate::sql::typed_explain(engine, sql).is_some() {
        return Err(Declined::said(
            Status::RefusedByGate,
            ALREADY_EXPLAIN.to_string(),
        ));
    }
    if engine.explain_prefix(ExplainMode::Plan).is_none() {
        return Err(Declined::said(Status::Executed, no_plan(engine)));
    }
    let explaining = if analyze {
        ExplainMode::Analyze
    } else {
        ExplainMode::Plan
    };
    let Some(composed) = engine.explain(explaining, sql) else {
        return Err(Declined::said(Status::Executed, no_analyzed_plan(engine)));
    };
    if let Err(message) = crate::sql::explainable(engine, sql) {
        return Err(Declined::said(Status::RefusedByGate, message));
    }
    let verdict = agent_gate(engine, &composed, mode);
    // What an approval would show is not for the agent.
    let text = match &verdict {
        AgentVerdict::Run => return Ok(composed),
        AgentVerdict::Approve(..) => NEEDS_APPROVAL.to_string(),
        AgentVerdict::Refuse(reason) => reason.to_string(),
    };
    Err(Declined {
        status: Status::RefusedByGate,
        text,
        verdict: Some(verdict),
        sql: Some(composed),
    })
}

/// The reply for a plan that came back: the compact text, findings before the
/// tree, or a sentence when no node could be read.
fn plan_reply(engine: Engine, result: &QueryResult) -> (Status, String) {
    let columns: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    if crate::explain::parse(&columns, &result.rows)
        .nodes
        .is_empty()
    {
        return (Status::Executed, empty_plan(engine));
    }
    (
        Status::Executed,
        compact::agent_plan(
            &columns,
            &result.rows,
            engine,
            compact::NODE_CAP,
            result::RESULT_BUDGET,
        ),
    )
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

/// The reply for `describe_table`: four tables, in the order the agent reads a
/// table's shape. An index's key entries are one JSON array in one cell,
/// because an entry can itself hold a comma. The names are the database's own.
fn structure_text(structure: &Structure, note: Option<&str>) -> String {
    let labelled = |label: &'static str, heads: &[&str], rows: Vec<Vec<Option<String>>>| {
        let mut table = table_of_names(heads, rows);
        table.label = label;
        table
    };
    let columns = structure
        .columns
        .iter()
        .map(|column| {
            vec![
                Some(column.name.clone()),
                Some(column.data_type.clone()),
                Some(column.nullable.to_string()),
                column.default.clone(),
            ]
        })
        .collect();
    let primary_key = structure
        .primary_key()
        .into_iter()
        .map(|name| vec![Some(name)])
        .collect();
    let foreign_keys = structure
        .foreign_keys
        .iter()
        .map(|key| {
            vec![
                Some(key.column.clone()),
                Some(key.referenced_schema.clone()),
                Some(key.referenced_table.clone()),
                Some(key.referenced_column.clone()),
            ]
        })
        .collect();
    let indexes = structure
        .indexes
        .iter()
        .map(|index| {
            vec![
                Some(index.name.clone()),
                Some(index_kind_name(index.kind).to_string()),
                // A list of strings always serializes; an empty list is what
                // the impossible case would say.
                Some(serde_json::to_string(&index.columns).unwrap_or_else(|_| "[]".to_string())),
                index.predicate.clone(),
            ]
        })
        .collect();
    result::text(
        &[
            labelled("columns", &["name", "type", "nullable", "default"], columns),
            labelled("primary_key", &["column"], primary_key),
            labelled(
                "foreign_keys",
                &[
                    "column",
                    "references_schema",
                    "references_table",
                    "references_column",
                ],
                foreign_keys,
            ),
            labelled(
                "indexes",
                &["name", "kind", "columns", "predicate"],
                indexes,
            ),
        ],
        0,
        note,
    )
}

fn index_kind_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Primary => "primary",
        IndexKind::Unique => "unique",
        IndexKind::Plain => "plain",
    }
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
    use crate::db::{ColumnDefinition, ForeignKey, Index, NamedDefinition};

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
    fn a_statement_is_sent_only_as_the_composed_explain_the_gate_ran() {
        let select = "SELECT * FROM t";
        let composed = explain_statement(Engine::Postgres, select, false, Mode::ReadOnly)
            .expect("a plain plan of a read runs");
        assert_eq!(composed, "EXPLAIN SELECT * FROM t");
        let composed = explain_statement(Engine::Postgres, select, true, Mode::ReadOnly)
            .expect("an analyzed plan of a read runs");
        assert_eq!(composed, "EXPLAIN ANALYZE SELECT * FROM t");

        // An ANALYZE of a write is never sent, and says what the gate judged.
        for mode in Mode::ALL {
            let held = explain_statement(Engine::Postgres, "UPDATE t SET a = 1", true, mode)
                .expect_err("an analyzed update is not run");
            assert_eq!(held.status, Status::RefusedByGate);
            assert_eq!(
                held.sql.as_deref(),
                Some("EXPLAIN ANALYZE UPDATE t SET a = 1")
            );
            assert!(held.verdict.is_some());
            assert_ne!(held.text, "");
            let removed = explain_statement(Engine::Postgres, "DELETE FROM t", true, mode)
                .expect_err("an analyzed delete is not run");
            assert_eq!(removed.status, Status::RefusedByGate);
        }
        let held = explain_statement(Engine::Postgres, "UPDATE t SET a = 1", true, Mode::Full)
            .expect_err("a person approves it first");
        assert_eq!(held.text, NEEDS_APPROVAL);
    }

    #[test]
    fn explain_analyze_follows_the_wrapped_statement() {
        let mode = Mode::ReadWrite;
        let composed = explain_statement(Engine::Postgres, "SELECT a FROM t", true, mode)
            .expect("an analyzed read runs");
        assert_eq!(composed, "EXPLAIN ANALYZE SELECT a FROM t");

        let held = explain_statement(
            Engine::Postgres,
            "UPDATE t SET a = 1 WHERE id = 1",
            true,
            mode,
        )
        .expect_err("an analyzed update waits for a person");
        assert_eq!(held.status, Status::RefusedByGate);
        assert_eq!(held.text, NEEDS_APPROVAL);

        let removed = explain_statement(Engine::Postgres, "DELETE FROM t WHERE id = 1", true, mode)
            .expect_err("an analyzed delete is refused");
        assert_eq!(removed.status, Status::RefusedByGate);
        assert_eq!(
            removed.text,
            "deleting data is refused for agent statements"
        );

        // A leading keyword says nothing: this reads as a SELECT and deletes.
        let writable = "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id) SELECT id FROM d";
        for mode in Mode::ALL {
            let refused = explain_statement(Engine::Postgres, writable, true, mode)
                .expect_err("an analyzed writable CTE is never run");
            assert_eq!(refused.status, Status::RefusedByGate);
        }

        let refused = explain_statement(Engine::MongoDb, "db.c.deleteMany({})", true, mode)
            .expect_err("a delete is not explained");
        assert_eq!(refused.status, Status::RefusedByGate);
        assert_eq!(
            Err(refused.text),
            crate::sql::explainable(Engine::MongoDb, "db.c.deleteMany({})")
        );
    }

    #[test]
    fn explain_defaults_to_the_plain_plan_and_never_downgrades_analyze() {
        for engine in Engine::ALL {
            let plain = explain_statement(engine, "SELECT 1", false, Mode::ReadOnly);
            match engine.explain_prefix(ExplainMode::Plan) {
                Some(_) => {
                    // Mongo and SQL alike: whatever is sent is what the engine
                    // composes for a plan, and only the gate may stop it.
                    if let Ok(composed) = &plain {
                        assert_eq!(
                            Some(composed.clone()),
                            engine.explain(ExplainMode::Plan, "SELECT 1")
                        );
                    }
                }
                None => assert!(plain.is_err(), "{engine:?} has no plan to ask for"),
            }

            let analyzed = explain_statement(engine, "SELECT 1", true, Mode::ReadOnly);
            if matches!(engine, Engine::Postgres | Engine::MySql | Engine::MariaDb) {
                assert!(plain.is_ok() && analyzed.is_ok(), "{engine:?} runs a read");
            }
            if let Ok(composed) = &analyzed {
                assert_eq!(
                    Some(composed.clone()),
                    engine.explain(ExplainMode::Analyze, "SELECT 1"),
                    "{engine:?} answered analyze with another plan"
                );
            }
            if engine.explain_prefix(ExplainMode::Plan).is_some()
                && engine.explain_prefix(ExplainMode::Analyze).is_none()
            {
                let said = analyzed.expect_err("no analyzed plan is run");
                assert_eq!(said.status, Status::Executed);
                assert_eq!(
                    said.text,
                    format!(
                        "DBDelve reads no analyzed plan from {}; nothing was run.",
                        engine.label()
                    )
                );
                assert!(said.sql.is_none());
            }
        }
        // Not left to the loop above: it must have found these two.
        for engine in [Engine::Sqlite, Engine::ClickHouse] {
            let said = explain_statement(engine, "SELECT 1", true, Mode::Full)
                .expect_err("neither measures a plan");
            assert_eq!(said.status, Status::Executed);
            assert_eq!(
                said.text,
                format!(
                    "DBDelve reads no analyzed plan from {}; nothing was run.",
                    engine.label()
                )
            );
        }
    }

    #[test]
    fn an_engine_without_a_plan_says_so_in_a_sentence() {
        for engine in [
            Engine::Turso,
            Engine::DuckDb,
            Engine::Snowflake,
            Engine::SqlServer,
            Engine::Redis,
            Engine::Generic,
        ] {
            for analyze in [false, true] {
                let said = explain_statement(engine, "SELECT 1", analyze, Mode::Full)
                    .expect_err("there is no plan to ask for");
                assert_eq!(said.status, Status::Executed, "{engine:?}");
                assert_eq!(
                    said.text,
                    format!("DBDelve does not read query plans from {}.", engine.label())
                );
                assert!(said.sql.is_none());
            }
        }
    }

    #[test]
    fn an_explain_typed_by_the_agent_is_refused() {
        for (engine, sql) in [
            (Engine::Postgres, "EXPLAIN SELECT 1"),
            (Engine::MongoDb, r#"db.c.find({}).explain("queryPlanner")"#),
        ] {
            for analyze in [false, true] {
                let refused = explain_statement(engine, sql, analyze, Mode::Full)
                    .expect_err("an explanation is not explained again");
                assert_eq!(refused.status, Status::RefusedByGate, "{engine:?}");
                assert_eq!(refused.text, "the statement is already an EXPLAIN");
            }
        }
    }

    #[test]
    fn a_plan_that_parses_to_nothing_says_so() {
        for engine in [Engine::Postgres, Engine::MySql, Engine::Sqlite] {
            assert_eq!(
                plan_reply(engine, &QueryResult::default()),
                (
                    Status::Executed,
                    format!("{} returned no plan DBDelve could read.", engine.label())
                )
            );
        }
    }

    /// The compose Postgres, from the standard `PG*` variables, opened the way
    /// the agent opens it.
    fn live_agent_connection() -> Connection {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
        let config = ConnectionConfig::postgres(crate::db::ServerConfig {
            host: var("PGHOST"),
            port: std::env::var("PGPORT")
                .ok()
                .map(|port| port.parse().expect("PGPORT must be a number")),
            database: var("PGDATABASE"),
            user: var("PGUSER"),
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
            // The compose database speaks no TLS, and this test is the one
            // place a plaintext connection is the point.
            sslmode: crate::db::SslMode::Disable,
            root_certificate: None,
            statement_timeout: 0,
            ssh: None,
            schema: None,
            read_only: false,
        });
        Connection::open_for_agent(config).expect("the compose Postgres opens for the agent")
    }

    #[test]
    #[ignore = "requires the compose databases"]
    fn live_an_explain_analyze_on_postgres_reads_a_measured_plan() {
        let connection = live_agent_connection();
        let statement = explain_statement(
            Engine::Postgres,
            "SELECT * FROM accounts",
            true,
            Mode::ReadOnly,
        )
        .expect("an analyzed read runs");
        let Ran::Done { result, .. } = explain_on(
            connection,
            Vec::new(),
            &statement,
            &CancelToken::default(),
            Engine::Postgres,
        ) else {
            panic!("the plan was not drained");
        };
        let text = result.expect("the server plans it").text;
        assert!(text.contains("analyzed=yes"), "{text}");
        assert!(text.contains("findings"), "{text}");
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

    fn column(
        name: &str,
        data_type: &str,
        nullable: bool,
        default: Option<&str>,
    ) -> ColumnDefinition {
        ColumnDefinition {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable,
            default: default.map(str::to_string),
        }
    }

    /// The lines of a reply that start with `label: ` and the row lines after
    /// each, up to the next label.
    fn section<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
        let labels = ["columns: ", "primary_key: ", "foreign_keys: ", "indexes: "];
        let start = format!("{label}: ");
        let mut lines = text.lines().skip_while(|line| !line.starts_with(&start));
        let mut found = vec![lines.next().expect("the label is there")];
        found.extend(lines.take_while(|line| !labels.iter().any(|l| line.starts_with(l))));
        found
    }

    #[test]
    fn a_structure_is_described_in_four_labelled_tables() {
        let structure = Structure {
            columns: vec![
                column("id", "integer", false, None),
                column("email", "text", true, Some("'x'")),
            ],
            indexes: vec![
                Index {
                    name: "by_email".to_string(),
                    kind: IndexKind::Unique,
                    columns: vec!["lower(email)".to_string(), "id".to_string()],
                    predicate: None,
                    definition: String::new(),
                },
                Index {
                    name: "live".to_string(),
                    kind: IndexKind::Plain,
                    columns: vec!["a, b".to_string()],
                    predicate: Some("a > 0".to_string()),
                    definition: String::new(),
                },
            ],
            constraints: vec![NamedDefinition {
                name: "t_pkey".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
            }],
            foreign_keys: vec![ForeignKey {
                column: "team".to_string(),
                referenced_schema: "main".to_string(),
                referenced_table: "teams".to_string(),
                referenced_column: "id".to_string(),
            }],
            referenced_by: Vec::new(),
        };
        let text = structure_text(&structure, None);
        let labels: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once(": ").map(|(label, _)| label))
            .filter(|label| ["columns", "primary_key", "foreign_keys", "indexes"].contains(label))
            .collect();
        assert_eq!(
            labels,
            ["columns", "primary_key", "foreign_keys", "indexes"]
        );
        assert_eq!(
            section(&text, "columns")[1..],
            [
                "[\"id\",\"integer\",\"false\",null]",
                "[\"email\",\"text\",\"true\",\"'x'\"]"
            ]
        );
        assert_eq!(section(&text, "primary_key")[1..], ["[\"id\"]"]);
        assert_eq!(
            section(&text, "foreign_keys")[1..],
            ["[\"team\",\"main\",\"teams\",\"id\"]"]
        );
        assert_eq!(
            section(&text, "indexes")[1..],
            [
                "[\"by_email\",\"unique\",\"[\\\"lower(email)\\\",\\\"id\\\"]\",null]",
                "[\"live\",\"plain\",\"[\\\"a, b\\\"]\",\"a > 0\"]"
            ]
        );
    }

    #[test]
    fn a_table_with_no_keys_or_indexes_is_described_with_empty_tables() {
        let structure = Structure {
            columns: vec![
                column("a", "text", true, None),
                column("b", "text", true, None),
            ],
            ..Structure::default()
        };
        let text = structure_text(&structure, None);
        assert_eq!(section(&text, "columns").len(), 3, "{text}");
        for label in ["primary_key", "foreign_keys", "indexes"] {
            assert_eq!(section(&text, label).len(), 1, "{label}: {text}");
        }
    }

    #[test]
    fn a_pair_the_catalog_does_not_hold_never_reaches_the_loader() {
        let catalog = Catalog {
            schemas: vec![schema(
                "main",
                vec![relation("t", RelationKind::Table, None, None)],
            )],
            database: None,
        };
        let refuse = |_: &str, _: &str| -> Result<Structure, DbError> {
            panic!("the loader was handed a name the catalog does not hold")
        };
        for (schema, table) in [
            ("main", "nope"),
            ("other", "t"),
            ("Main", "t"),
            ("main", "T"),
        ] {
            assert!(
                matches!(describe(&catalog, schema, table, None, refuse), Ok(None)),
                "{schema}.{table}"
            );
        }
        let mut asked = Vec::new();
        let held = describe(&catalog, "main", "t", None, |schema, table| {
            asked.push((schema.to_string(), table.to_string()));
            Ok(Structure::default())
        });
        assert!(matches!(held, Ok(Some(_))));
        assert_eq!(asked, [("main".to_string(), "t".to_string())]);
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

    #[test]
    fn the_reply_is_written_where_the_statement_ran_with_a_wide_cell_clipped() {
        let dir = std::env::temp_dir().join(format!("dbdelve-agent-shaped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the directory is made");
        let path = dir.join("shaped.db");
        let _ = std::fs::remove_file(&path);
        rusqlite::Connection::open(&path).expect("the file is made");
        let connection = Connection::open_for_agent(ConnectionConfig::Sqlite {
            path: path.to_string_lossy().into_owned(),
            statement_timeout: 0,
        })
        .expect("the agent opens the file");
        let ran = read_on(
            connection,
            Vec::new(),
            "SELECT replace(hex(zeroblob(3000)), '0', 'x') AS wide",
            &CancelToken::default(),
            Engine::Sqlite,
        );
        let Ran::Done { result, .. } = ran else {
            panic!("the statement ran");
        };
        let shaped = result.expect("the statement answered");
        // The text the window sends is already final: the header says a cell
        // was clipped and from how long.
        assert!(
            shaped
                .text
                .starts_with("truncated: 1 of 1 rows shown (clipped cells)"),
            "{}",
            &shaped.text[..80]
        );
        assert!(shaped.text.contains("[clipped from 6000 characters]"));
        assert_eq!(shaped.rows, (1, Total::Exact(1)));
        std::fs::remove_dir_all(&dir).expect("the directory goes");
    }
}
