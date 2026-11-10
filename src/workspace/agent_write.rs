//! An approved write.
//!
//! It runs on a connection opened for that one call, never a lane's and never
//! the user's, because a session change the statement makes must die with the
//! connection (`SET`, `USE`, a temporary table, an open transaction). It has
//! no deadline and no cancel: a lane's calls end by cancelling the statement
//! on the server, and a write interrupted half-way is worse than one that
//! finishes, so nothing but its own result ends it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    time::Instant,
};

use super::agent::{AuditVerdict, CallFacts, Decision, elapsed_ms};
use super::agent_read::{AGENT_ROWS, OPEN_FAILED, scrubbed, table_of};
use super::*;
use crate::{
    agent::{CallId, Reply, Status, result, tools::Tool},
    db::{CancelToken, Connection, ConnectionConfig, DbError, Engine, Fetch, QueryResult},
};

// ponytail: the longest an approved write is left to run where the engine has
// a server-side bound. A profile's own shorter timeout wins, as for reads. It
// is not a deadline that cancels: a running write is never cancelled. The
// upgrade is a setting, if a user ever needs one.
pub(crate) const WRITE_TIMEOUT: u32 = 600;

/// The statement timeout in seconds an approved write's connection carries,
/// given the profile's own: the smaller of the two, with 0 (none) meaning
/// [`WRITE_TIMEOUT`].
pub(crate) fn write_timeout(user: u32) -> u32 {
    match user {
        0 => WRITE_TIMEOUT,
        user => user.min(WRITE_TIMEOUT),
    }
}

/// The config an approved write's connection is opened from: the profile's,
/// bounded by [`write_timeout`]. A read's `agent_config` caps at thirty
/// seconds, which a `CREATE INDEX` would not survive.
pub(crate) fn agent_write_config(config: &ConnectionConfig) -> ConnectionConfig {
    config
        .clone()
        .with_statement_timeout(write_timeout(config.statement_timeout()))
}

/// An approved write that is running. Holds no connection, no cancel token and
/// no deadline: the connection lives in the blocking closure and is dropped
/// there, and the call ends only when the result lands in
/// [`Workspace::finish_write`].
pub(crate) struct Writing {
    pub(crate) id: CallId,
    profile_name: String,
    engine: Engine,
    /// Kept after a hangup: the answer is still sent and nobody reads it.
    reply: SyncSender<Reply>,
    arrived: Instant,
    /// The statement exactly as it was approved, for the audit.
    sql: String,
    verdict: Option<AuditVerdict>,
    /// The caller hung up while the window already knew, as a `Hangup` that
    /// found this write running said. It is not cancelled for that.
    pub(super) requester_gone: bool,
    /// The socket's own flag for the call, set by its hangup watcher before the
    /// window hears of it. Read when the write ends, so a hangup the window has
    /// not yet heard still counts.
    gone: Arc<AtomicBool>,
    _work: gpui::Task<()>,
}

/// What opens, runs and answers an approved write: [`open_for_write`], or one a
/// test puts in [`AgentState::write_runner`](super::agent::AgentState::write_runner),
/// as `Opener` is for reads, to keep a write running.
pub(crate) type WriteRunner =
    fn(&gpui::BackgroundExecutor, ConnectionConfig, String, String, Engine) -> gpui::Task<Wrote>;

/// What the blocking half of an approved write came back with.
pub(crate) enum Wrote {
    NotOpened(DbError),
    /// `hidden` is what an error from the server must not name.
    Done {
        hidden: Vec<String>,
        result: Result<String, DbError>,
    },
}

/// Open a connection for this one call, run `sql` on it as written, build the
/// answer and drop the connection before returning, so a runtime shutdown
/// never happens on the window's thread.
pub(super) fn open_for_write(
    executor: &gpui::BackgroundExecutor,
    config: ConnectionConfig,
    profile_id: String,
    sql: String,
    engine: Engine,
) -> gpui::Task<Wrote> {
    spawn_blocking(executor, move || {
        let mut config = agent_write_config(&config);
        // A file engine has nothing to authenticate to, so it never reaches
        // the keychain.
        if let Some(slot) = config.password_mut()
            && slot.is_empty()
        {
            match store::password(&profile_id) {
                Ok(Some(password)) => *slot = password,
                Ok(None) => {}
                Err(error) => {
                    return Wrote::NotOpened(DbError {
                        message: error,
                        position: None,
                    });
                }
            }
        }
        // Taken after the password is in: it is one of the strings a server's
        // error may echo.
        let hidden = config.identifying_text();
        let connection = match Connection::open_for_approved_write(config) {
            Ok(connection) => connection,
            Err(error) => return Wrote::NotOpened(error),
        };
        // Not `reads_only`: a write's rows are drained, never stopped early.
        let result = connection
            .query(
                &sql,
                &CancelToken::default(),
                Fetch {
                    limit: Some(AGENT_ROWS),
                    ..Fetch::default()
                },
            )
            .map(|result| written_text(engine, &sql, &result));
        drop(connection);
        Wrote::Done { hidden, result }
    })
}

/// What an approved write answers: its rows when it returned any, otherwise the
/// server's count, or that it ran when the server gave none.
pub(super) fn written_text(engine: Engine, _sql: &str, result: &QueryResult) -> String {
    if !result.columns.is_empty() {
        return result::text(&[table_of(engine, result)], result.rest.len(), None);
    }
    match result.rows_affected {
        Some(1) => "The server reported 1 row affected.".to_string(),
        Some(count) => format!("The server reported {count} rows affected."),
        None => "The statement ran.".to_string(),
    }
}

impl Workspace {
    /// Start an approved write on a connection of its own. No deadline timer
    /// is made: see the module's header.
    #[allow(
        clippy::too_many_arguments,
        reason = "the call's identity, the profile it is about, the statement as approved, its verdict and the way back to its caller are each one thing; a struct of them would be built once per call to be taken apart at once"
    )]
    pub(super) fn start_write(
        &mut self,
        id: CallId,
        profile_id: String,
        profile_name: String,
        config: ConnectionConfig,
        sql: String,
        verdict: Option<AuditVerdict>,
        reply: SyncSender<Reply>,
        arrived: Instant,
        gone: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let engine = config.engine();
        let run = self.agent.write_runner.unwrap_or(open_for_write);
        let work = cx.spawn({
            let sql = sql.clone();
            async move |workspace, cx| {
                let wrote = run(cx.background_executor(), config, profile_id, sql, engine).await;
                // The window may have closed meanwhile, and then nobody is
                // left to answer.
                let _ = workspace.update(cx, |workspace, cx| workspace.finish_write(id, wrote, cx));
            }
        });
        self.agent.writes.push(Writing {
            id,
            profile_name,
            engine,
            reply,
            arrived,
            sql,
            verdict,
            requester_gone: false,
            gone,
            _work: work,
        });
        cx.notify();
    }

    /// The one door an approved write ends through: its own result.
    fn finish_write(&mut self, id: CallId, wrote: Wrote, cx: &mut Context<Self>) {
        let Some(index) = self.agent.writes.iter().position(|write| write.id == id) else {
            return;
        };
        let write = self.agent.writes.remove(index);
        let (status, text, error) = match wrote {
            Wrote::NotOpened(error) => {
                (Status::Failed, OPEN_FAILED.to_string(), Some(error.message))
            }
            Wrote::Done {
                result: Ok(text), ..
            } => (Status::Executed, text, None),
            Wrote::Done {
                hidden,
                result: Err(error),
            } => (
                Status::Failed,
                scrubbed(&error.message, &hidden),
                Some(error.message),
            ),
        };
        // From the socket's flag as well as from a `Hangup` the window heard,
        // and known before the outcome line is written: `Undelivered` arrives
        // after it and could only change the in-memory entry.
        let requester_gone = write.requester_gone || write.gone.load(Ordering::Acquire);
        // The caller may have hung up, and then nobody reads this; the reply is
        // still sent, because a dropped sender would close the socket with no
        // answer for a caller that is still there.
        let _ = write.reply.send(Reply { status, text });
        self.agent.record(
            id,
            Tool::ExecuteSql,
            Some(write.profile_name),
            None,
            status,
            CallFacts {
                verdict: write.verdict,
                sql: Some(write.sql),
                elapsed_ms: elapsed_ms(write.arrived),
                error,
                requester_gone,
                decision: Some(Decision::Approved),
                engine: Some(write.engine),
                ..CallFacts::default()
            },
        );
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Column, every_shape};

    #[test]
    fn the_write_config_carries_the_write_bound() {
        assert_eq!(write_timeout(0), WRITE_TIMEOUT);
        assert_eq!(write_timeout(5), 5);
        assert_eq!(write_timeout(u32::MAX), WRITE_TIMEOUT);
        for profile in every_shape() {
            for user in [0, 5, WRITE_TIMEOUT, WRITE_TIMEOUT + 1] {
                let profile = profile.clone().with_statement_timeout(user);
                let opened = agent_write_config(&profile);
                assert_eq!(
                    opened.statement_timeout(),
                    write_timeout(user),
                    "{:?} {user}",
                    profile.engine()
                );
                assert_eq!(
                    opened.with_statement_timeout(user),
                    profile,
                    "only the timeout differs"
                );
            }
        }
    }

    #[test]
    fn an_approved_write_answers_its_count_or_that_it_ran() {
        let counted = |rows_affected| QueryResult {
            rows_affected,
            ..QueryResult::default()
        };
        const UPDATE: &str = "UPDATE t SET a = 1";
        const INDEX: &str = "CREATE INDEX i ON t (a)";
        for (sql, affected, text) in [
            (UPDATE, Some(1), "The server reported 1 row affected."),
            (UPDATE, Some(3), "The server reported 3 rows affected."),
            (INDEX, Some(0), "The statement ran."),
            (INDEX, None, "The statement ran."),
            (UPDATE, None, "The statement ran."),
        ] {
            assert_eq!(
                written_text(Engine::Sqlite, sql, &counted(affected)),
                text,
                "{sql} {affected:?}"
            );
        }

        let returned = QueryResult {
            columns: vec![Column {
                name: "id".to_string(),
                data_type: None,
            }],
            ..QueryResult::default()
        };
        let text = written_text(Engine::Sqlite, "UPDATE t SET a = 1 RETURNING id", &returned);
        assert!(text.contains("id"), "{text}");
        assert!(!text.contains("affected"), "{text}");
    }
}
