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

use std::sync::mpsc::SyncSender;

use super::*;
use crate::{
    agent::{CallId, Reply, Status, result},
    db::{CancelToken, Connection, ConnectionConfig, DbError, Fetch, QueryResult},
};

// ponytail: an agent is told the first hundred rows, and the tool takes no
// count, so what a call can cost the model's context does not depend on what
// the model asks for. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_ROWS: usize = 100;

// ponytail: the longest an agent's statement runs when the profile sets no
// timeout of its own, so no agent connection is ever unbounded. A profile's own
// shorter timeout wins. The upgrade is a setting, if a user ever needs one.
pub(crate) const AGENT_TIMEOUT: u32 = 30;

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

/// A connection of the agent's own to one profile's database.
pub(crate) struct Lane {
    /// The config it was opened from, its password slot as the profile holds
    /// it (blank when the keychain has it), so a profile whose config has
    /// since changed is told apart from one that has not.
    pub(crate) opened_from: ConnectionConfig,
    pub(crate) connection: Connection,
    /// What an error that came back through this connection must not say
    /// ([`ConnectionConfig::identifying_text`] of the config it opened with,
    /// its password included). It lives exactly as long as the connection,
    /// which holds the same strings.
    pub(crate) hidden: Vec<String>,
}

/// What the blocking half of a read came back with.
#[allow(
    clippy::large_enum_variant,
    reason = "one value per read, moved straight into its landing"
)]
enum Ran {
    /// The connection could not be made, or could not be held read-only.
    NotOpened,
    Done {
        connection: Connection,
        hidden: Vec<String>,
        result: Result<QueryResult, DbError>,
    },
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
    /// Run one statement the gate let through, on the agent's own connection
    /// to `profile_id`, and answer the call with its first rows. Opens the
    /// connection on first use, off the foreground, and keeps it for the next
    /// call.
    pub(crate) fn run_agent_sql(
        &mut self,
        id: CallId,
        profile_id: String,
        sql: String,
        reply: SyncSender<Reply>,
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
                None,
                None,
                &reply,
                Status::RefusedByGate,
                super::agent::AgentLookup::NotAvailable.to_string(),
            );
            return;
        };
        let config = profile.config.clone();
        let engine = config.engine();
        let cached = self
            .agent
            .lanes
            .get(&profile_id)
            .filter(|lane| lane.opened_from == config)
            .map(|lane| (lane.connection.clone(), lane.hidden.clone()));

        let task = spawn_blocking(cx.background_executor(), {
            let profile_id = profile_id.clone();
            let config = config.clone();
            move || {
                let (connection, hidden) = match cached {
                    Some(cached) => cached,
                    None => {
                        let mut config = agent_config(&config);
                        // A file engine has nothing to authenticate to, so it
                        // never reaches the keychain.
                        if let Some(slot) = config.password_mut()
                            && slot.is_empty()
                        {
                            match store::password(&profile_id) {
                                Ok(Some(password)) => *slot = password,
                                Ok(None) => {}
                                Err(_) => return Ran::NotOpened,
                            }
                        }
                        // Taken after the password is in: it is one of the
                        // strings a server's error may echo.
                        let hidden = config.identifying_text();
                        let Ok(connection) = Connection::open(config) else {
                            return Ran::NotOpened;
                        };
                        // The hold is what makes a read the gate misjudged
                        // harmless, so a connection that cannot take it is
                        // not one the agent may use.
                        if connection.set_read_only(true).is_err() {
                            return Ran::NotOpened;
                        }
                        (connection, hidden)
                    }
                };
                let result = connection.query(
                    &sql,
                    &CancelToken::default(),
                    Fetch {
                        limit: Some(AGENT_ROWS),
                        reads_only: crate::sql::stoppable(engine, &sql),
                        ..Fetch::default()
                    },
                );
                Ran::Done {
                    connection,
                    hidden,
                    result,
                }
            }
        });

        cx.spawn(async move |workspace, cx| {
            let ran = task.await;
            // The window may have closed meanwhile, and then nobody is left to
            // answer.
            let _ = workspace.update(cx, |workspace, _| {
                workspace.land_agent_read(id, &profile_id, config, ran, &reply);
            });
        })
        .detach();
    }

    /// Take what a read came back with: keep its connection, and answer.
    fn land_agent_read(
        &mut self,
        id: CallId,
        profile_id: &str,
        opened_from: ConnectionConfig,
        ran: Ran,
        reply: &SyncSender<Reply>,
    ) {
        let name = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .map(|profile| profile.name.clone());
        let engine = opened_from.engine();
        let (status, text) = match ran {
            Ran::NotOpened => {
                self.agent.lanes.remove(profile_id);
                (Status::Failed, OPEN_FAILED.to_string())
            }
            Ran::Done {
                connection,
                hidden,
                result: Ok(result),
            } => {
                // A profile removed while the read ran has no lane to keep.
                if name.is_some() {
                    self.agent.lanes.insert(
                        profile_id.to_string(),
                        Lane {
                            opened_from,
                            connection,
                            hidden,
                        },
                    );
                }
                (
                    Status::Executed,
                    result::text(&[table_of(engine, &result)], result.rest.len()),
                )
            }
            Ran::Done {
                hidden,
                result: Err(error),
                ..
            } => {
                // ponytail: every failure ends the lane rather than asking
                // which ones end a session, since a Redis timeout closes it
                // without `is_lost` knowing (D-02). The cost is one reopen
                // after a failed statement.
                self.agent.lanes.remove(profile_id);
                (Status::Failed, scrubbed(&error.message, &hidden))
            }
        };
        self.agent_answered(id, name, None, reply, status, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
