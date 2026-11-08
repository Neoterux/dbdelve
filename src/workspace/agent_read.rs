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

/// Said when a statement the agent was allowed to run returned an error.
pub(crate) const STATEMENT_FAILED: &str = "The database returned an error for this statement.";

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
        result: Result<QueryResult, DbError>,
    },
}

/// One result set as the agent reads it.
fn table_of(result: &QueryResult) -> result::Table {
    let rows: Vec<Vec<Option<String>>> = result
        .rows
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
            .map(|lane| lane.connection.clone());

        let task = spawn_blocking(cx.background_executor(), {
            let profile_id = profile_id.clone();
            let config = config.clone();
            move || {
                let connection = match cached {
                    Some(connection) => connection,
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
                        let Ok(connection) = Connection::open(config) else {
                            return Ran::NotOpened;
                        };
                        // The hold is what makes a read the gate misjudged
                        // harmless, so a connection that cannot take it is
                        // not one the agent may use.
                        if connection.set_read_only(true).is_err() {
                            return Ran::NotOpened;
                        }
                        connection
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
                Ran::Done { connection, result }
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
        let (status, text) = match ran {
            Ran::NotOpened => (Status::Failed, OPEN_FAILED.to_string()),
            Ran::Done { connection, result } => {
                // A profile removed while the read ran has no lane to keep.
                if name.is_some() {
                    self.agent.lanes.insert(
                        profile_id.to_string(),
                        Lane {
                            opened_from,
                            connection,
                        },
                    );
                }
                match result {
                    Ok(result) => (
                        Status::Executed,
                        result::text(&[table_of(&result)], result.rest.len()),
                    ),
                    Err(_) => (Status::Failed, STATEMENT_FAILED.to_string()),
                }
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
}
