//! The single answer for a statement an agent originates: run it, ask the human
//! to approve it, or refuse it by name.
//!
//! [`agent_gate`] is a pure function of `(engine, sql, mode)`. It reads no
//! profile, no silenced confirmation and no earlier approval, so the mode passed
//! for a call is the ceiling for that call and nothing else, and two callers
//! cannot change each other's answers. It reads the parse [`super::classify`]
//! judges and decides about the text as written; it never returns another text.
//!
//! The ladder, each step returning at once. Steps 1 to 6 do not depend on the
//! mode, so a refusal's reason is the same on every profile.
//!
//! 1. `Unreadable` in the verdict is `Refuse(Unreadable)`, before `gate` is
//!    asked: `gate` answers `Upgrade` for an unreadable statement on an engine
//!    with no server-side hold, which would read as a negotiation.
//! 2. The statement count: the parse `classify` judged and the buffer's token
//!    bearing ranges. None is `Refuse(NoStatement)`, two or more is
//!    `Refuse(MoreThanOne)`.
//! 3. (Redis commands the editor never sends are refused here, in plan 03-03.)
//! 4. `DROP` is `Refuse(Drop)`, `TRUNCATE` is `Refuse(Truncate)`, an unfiltered
//!    `DELETE` is `Refuse(Delete)`.
//! 5. (Server-side scripts and Redis disclosure commands are refused here, in
//!    plan 03-03.)
//! 6. (Statements that change the session are refused here, in task 3.)
//! 7. A verdict that needs `Mode::Full` is `Refuse(NeedsFull)`: a Full profile
//!    grants an agent nothing a Read-write one does not.
//! 8. `gate(&verdict, mode, &[])` stopping the statement for the mode is
//!    `Refuse(NeedsMode { needed, held })`. The silenced list is always empty,
//!    so a kind a human silenced changes nothing for an agent.
//! 9. A write the profile's mode allows is `Approve(verdict)`.
//! 10. (A read that calls an egress function is approved here, in plan 03-05.)
//! 11. Anything else is a read, and `Run`s.
//!
//! The mode stays the ceiling: `Run` and `Approve` both imply that `gate` with
//! the same mode and no silenced kind lets the statement through, and the
//! corpus in `sql_snapshots::agent` holds that over every cell.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent tools (Phase 4) are the first caller")
)]

use std::fmt;

use crate::db::{Engine, Syntax};

use super::{Buffer, Destructive, Mode, Stop, Verdict, dialect, gate, holds_a_token, parsed};

/// What an agent's statement is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentVerdict {
    /// A read: nothing is asked of the human.
    Run,
    /// A write the profile's mode allows: it waits for the human, who sees the
    /// exact statement first.
    Approve(Verdict),
    /// Not allowed on any profile, or not on this one.
    Refuse(Refusal),
}

/// Why a statement was refused. One variant per reason, so a refusal is
/// diagnosable by what it says; the text is what an agent reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The text holds no statement.
    NoStatement,
    /// The text holds two or more.
    MoreThanOne,
    /// DBDelve could not read the statement, so it cannot say what it does.
    Unreadable,
    Drop,
    Truncate,
    /// Any `DELETE`, whatever its shape.
    Delete,
    /// A statement that needs Full.
    NeedsFull,
    /// A statement that needs more than the profile's mode.
    NeedsMode {
        needed: Mode,
        held: Mode,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoStatement => f.write_str("there is no statement to run"),
            Refusal::MoreThanOne => f.write_str("more than one statement"),
            Refusal::Unreadable => f.write_str("DBDelve can't read this statement"),
            Refusal::Drop => f.write_str("DROP is refused for agent statements"),
            Refusal::Truncate => f.write_str("TRUNCATE is refused for agent statements"),
            Refusal::Delete => f.write_str("deleting data is refused for agent statements"),
            Refusal::NeedsFull => {
                f.write_str("statements that need Full are refused for agent statements")
            }
            Refusal::NeedsMode { needed, held } => {
                write!(
                    f,
                    "needs {}; the profile is {}",
                    needed.label(),
                    held.label()
                )
            }
        }
    }
}

/// The answer for `sql` as an agent originates it on a profile whose mode is
/// `mode`. See the module header for the order the reasons are asked in.
pub(crate) fn agent_gate(engine: Engine, sql: &str, mode: Mode) -> AgentVerdict {
    // ponytail: MongoDB and Redis are not answered yet, so they are refused as
    // unreadable rather than run through a ladder written for SQL; plan 03-03
    // gives each its own steps. Failing closed is the only safe default for a
    // function whose `Run` sends a statement with no human.
    if engine.syntax() != Syntax::Sql {
        return AgentVerdict::Refuse(Refusal::Unreadable);
    }

    let read = parsed(engine, sql);
    let verdict = read.verdict;

    if verdict.destructive.contains(&Destructive::Unreadable) {
        return AgentVerdict::Refuse(Refusal::Unreadable);
    }

    match statement_count(engine, sql, read.statements) {
        0 => return AgentVerdict::Refuse(Refusal::NoStatement),
        1 => {}
        _ => return AgentVerdict::Refuse(Refusal::MoreThanOne),
    }

    for (kind, refusal) in [
        (Destructive::Drop, Refusal::Drop),
        (Destructive::Truncate, Refusal::Truncate),
        (Destructive::UnfilteredDelete, Refusal::Delete),
    ] {
        if verdict.destructive.contains(&kind) {
            return AgentVerdict::Refuse(refusal);
        }
    }

    if verdict.mode == Mode::Full {
        return AgentVerdict::Refuse(Refusal::NeedsFull);
    }

    // Never a silenced kind: an agent has no confirmations of its own and
    // borrows none of a human's.
    match gate(&verdict, mode, &[]) {
        Some(Stop::Upgrade(needed)) => {
            return AgentVerdict::Refuse(Refusal::NeedsMode { needed, held: mode });
        }
        // Steps 1 and 4 took every kind these two answer for, so neither is
        // reached; each is refused as what it names rather than panicking.
        Some(Stop::Confirm(_)) | Some(Stop::RunOnce) => {
            return AgentVerdict::Refuse(Refusal::Unreadable);
        }
        None => {}
    }

    if verdict.mode == Mode::ReadWrite {
        return AgentVerdict::Approve(verdict);
    }
    AgentVerdict::Run
}

/// How many statements `sql` holds, as the more of the two readings of it.
///
/// Both have to count one. `typed` is the count of the parse `classify`
/// judged, MariaDB's executable comments expanded: it reads `BEGIN; ...;
/// COMMIT` as three statements where the grammar reads one transaction block,
/// and finds a statement a comment carries. The grammar's ranges are counted
/// where the dialect's own lexer finds a real token in them, which leaves a
/// trailing comment, MySQL's `#` among them, uncounted. A statement wholly
/// inside an executable comment is one to `typed` and none to the lexer, and
/// is counted as that one.
fn statement_count(engine: Engine, sql: &str, typed: usize) -> usize {
    let Some(dialect) = dialect(engine) else {
        return typed;
    };
    let ranges = Buffer::for_engine(engine, sql)
        .statements()
        .iter()
        .filter(|range| holds_a_token(dialect.as_ref(), &sql[(*range).clone()]))
        .count();
    if typed == 0 { 0 } else { typed.max(ranges) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_engines() -> impl Iterator<Item = Engine> {
        Engine::ALL
            .into_iter()
            .filter(|engine| engine.syntax() == Syntax::Sql)
    }

    fn refused(refusal: Refusal) -> AgentVerdict {
        AgentVerdict::Refuse(refusal)
    }

    fn needs_read_write() -> AgentVerdict {
        refused(Refusal::NeedsMode {
            needed: Mode::ReadWrite,
            held: Mode::ReadOnly,
        })
    }

    #[test]
    fn a_plain_select_runs_on_every_engine_and_mode() {
        for engine in sql_engines() {
            for sql in [
                "SELECT a FROM t",
                "SELECT lower(a) FROM t",
                "SELECT my_udf(a) FROM t",
            ] {
                for mode in Mode::ALL {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        AgentVerdict::Run,
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_select_calling_a_listed_function_never_runs() {
        for engine in sql_engines() {
            let Some(name) = engine.spec().sql.denied_functions.first() else {
                continue;
            };
            let sql = format!("SELECT {name}()");
            assert_eq!(
                agent_gate(engine, &sql, Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}: {sql}"
            );
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert!(
                    matches!(agent_gate(engine, &sql, mode), AgentVerdict::Approve(_)),
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn an_update_on_a_read_write_profile_is_approved_not_run() {
        let sql = "UPDATE t SET a = 1 WHERE id = 1";
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, sql, Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}"
            );
            for mode in [Mode::ReadWrite, Mode::Full] {
                assert_eq!(
                    agent_gate(engine, sql, mode),
                    AgentVerdict::Approve(Verdict::WRITE),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn create_index_is_approvable_and_needs_read_write() {
        for engine in sql_engines() {
            for sql in [
                "CREATE INDEX i ON t (a)",
                "CREATE INDEX IF NOT EXISTS i ON t (a)",
            ] {
                assert_eq!(
                    agent_gate(engine, sql, Mode::ReadOnly),
                    needs_read_write(),
                    "{engine:?}: {sql}"
                );
                for mode in [Mode::ReadWrite, Mode::Full] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        AgentVerdict::Approve(Verdict::WRITE),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
            }
        }
        let concurrently = "CREATE INDEX CONCURRENTLY i ON t (a)";
        for engine in sql_engines() {
            let held = agent_gate(engine, concurrently, Mode::ReadOnly);
            let approved = agent_gate(engine, concurrently, Mode::ReadWrite);
            // Only Postgres's dialect reads the keyword; another engine must
            // say it cannot, never run it.
            assert!(
                (held == needs_read_write() && approved == AgentVerdict::Approve(Verdict::WRITE))
                    || held == refused(Refusal::Unreadable) && approved == held,
                "{engine:?}: {held:?} {approved:?}"
            );
        }
    }

    #[test]
    fn a_statement_needing_more_than_the_profile_is_refused_never_offered_an_upgrade() {
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, "INSERT INTO t (a) VALUES (1)", Mode::ReadOnly),
                needs_read_write(),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn an_unreadable_statement_is_refused_with_or_without_a_server_hold() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, "selec 1", mode),
                    refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn a_silenced_kind_changes_nothing_for_an_agent() {
        for engine in sql_engines() {
            let sql = "DELETE FROM t";
            let human = super::super::classify(engine, sql);
            assert_eq!(
                gate(&human, Mode::Full, &[Destructive::UnfilteredDelete]),
                None,
                "{engine:?}"
            );
            assert_eq!(
                agent_gate(engine, sql, Mode::Full),
                refused(Refusal::Delete),
                "{engine:?}"
            );
        }
    }

    #[test]
    fn drop_and_truncate_are_refused_by_name_in_every_mode() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                for sql in ["DROP TABLE t", "drop table t", "DrOp TaBlE t"] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::Drop),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
                for sql in ["TRUNCATE TABLE t", "truncate t"] {
                    let answer = agent_gate(engine, sql, mode);
                    // A dialect with no `TRUNCATE` cannot read it, and says so.
                    assert!(
                        answer == refused(Refusal::Truncate)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for mode in Mode::ALL {
            assert_eq!(
                agent_gate(Engine::Postgres, "truncate t", mode),
                refused(Refusal::Truncate)
            );
        }
    }

    #[test]
    fn full_class_statements_are_refused_on_every_profile() {
        for engine in sql_engines() {
            for sql in ["GRANT SELECT ON t TO u", "CALL p()"] {
                let answer = agent_gate(engine, sql, Mode::Full);
                assert!(
                    answer == refused(Refusal::NeedsFull) || answer == refused(Refusal::Unreadable),
                    "{engine:?}: {sql}: {answer:?}"
                );
            }
        }
        for sql in [
            "GRANT SELECT ON t TO u",
            "CALL p()",
            "COPY t TO '/tmp/x.csv'",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::NeedsFull),
                    "{sql} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn empty_and_comment_only_text_is_no_statement() {
        for engine in sql_engines() {
            for sql in ["", "   ", "-- nothing", "/* x */"] {
                for mode in Mode::ALL {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::NoStatement),
                        "{engine:?} {mode:?}: {sql:?}"
                    );
                }
            }
            assert_eq!(
                agent_gate(engine, "SELECT 1", Mode::ReadOnly),
                AgentVerdict::Run
            );
        }
    }

    #[test]
    fn two_statements_are_refused_whatever_they_are() {
        for engine in sql_engines() {
            for mode in Mode::ALL {
                for sql in ["SELECT 1; SELECT 2", "DROP TABLE a; DROP TABLE b"] {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        refused(Refusal::MoreThanOne),
                        "{engine:?} {mode:?}: {sql}"
                    );
                }
                assert_eq!(
                    agent_gate(engine, "SELECT 1; garbage", mode),
                    refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}"
                );
            }
        }
    }

    #[test]
    fn a_comment_beside_one_statement_is_still_one_statement() {
        for engine in sql_engines() {
            for sql in ["SELECT 1; -- c", "-- c\nSELECT 1", "/* c */ SELECT 1"] {
                for mode in Mode::ALL {
                    assert_eq!(
                        agent_gate(engine, sql, mode),
                        AgentVerdict::Run,
                        "{engine:?} {mode:?}: {sql:?}"
                    );
                }
            }
        }
        for engine in [Engine::MySql, Engine::MariaDb] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, "SELECT 1 # c", mode),
                    AgentVerdict::Run,
                    "{engine:?} {mode:?}"
                );
            }
        }
    }
}
