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
//! 4. `DROP` is `Refuse(Drop)`, `TRUNCATE` is `Refuse(Truncate)`, any `DELETE`
//!    the walk reached, filtered or not, is `Refuse(Delete)`, and an `ALTER
//!    TABLE` carrying anything but `ADD COLUMN` is `Refuse(Alter)`.
//! 5. (Server-side scripts and Redis disclosure commands are refused here, in
//!    plan 03-03.)
//! 6. A statement that changes the session, or assigns a user variable with
//!    `:=`, is `Refuse(Session)`: a later approved write must hit the table the
//!    human read it for, so nothing an agent sends may repoint the session
//!    unseen. Stateless engines included. A plain read of what the session
//!    holds, `currval` or `last_insert_id()`, stays a read.
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

use sqlparser::tokenizer::{Token, Tokenizer};

use crate::db::{Engine, Syntax};

use super::{
    Buffer, Destructive, Mode, Stop, Verdict, dialect, executed_tokens, gate, holds_a_token, parsed,
};

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
    /// An `ALTER TABLE` carrying anything but `ADD COLUMN`.
    Alter,
    /// A statement that leaves the session changed, savepoints included.
    Session,
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
            Refusal::Alter => {
                f.write_str("ALTER other than ADD COLUMN is refused for agent statements")
            }
            Refusal::Session => {
                f.write_str("statements that change the session are refused for agent statements")
            }
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
    ] {
        if verdict.destructive.contains(&kind) {
            return AgentVerdict::Refuse(refusal);
        }
    }
    // A filtered `DELETE` is a plain write to the verdict, so only the walk
    // that read it knows it removed rows; an unfiltered one carries its kind
    // as well.
    if read.removes || verdict.destructive.contains(&Destructive::UnfilteredDelete) {
        return AgentVerdict::Refuse(Refusal::Delete);
    }
    if read.alters {
        return AgentVerdict::Refuse(Refusal::Alter);
    }

    // Judged by the statements' own kinds and by the tokens, never by
    // `leaves`: that answers nothing on an engine that keeps no session, and
    // it flags a plain read of session state, which a read is.
    if read.session || assigns_a_user_variable(engine, sql) {
        return AgentVerdict::Refuse(Refusal::Session);
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
        // ponytail: `REPLACE INTO` and `INSERT OR REPLACE` delete a conflicting
        // row before they insert, and classify as the `INSERT` they parse as,
        // so they are approved like one. The human reads the statement before
        // approving it (D-23). The upgrade path is a removal flag on a
        // replacing insert in the walk, which would refuse them as `Delete`.
        return AgentVerdict::Approve(verdict);
    }
    AgentVerdict::Run
}

/// Whether `sql` assigns a user variable with `:=` (`SELECT @x := 1`), the
/// same session change as `SET @x = 1` in another spelling. Read off the
/// tokens the server will run, so one inside an executable comment counts.
/// The variable is an `@`-prefixed name, however the dialect's tokenizer
/// spells it; a named argument (`f(a := 1)`) has no `@` and is not one.
fn assigns_a_user_variable(engine: Engine, sql: &str) -> bool {
    let Some(dialect) = dialect(engine) else {
        return false;
    };
    let tokens = executed_tokens(engine, dialect.as_ref(), sql)
        .unwrap_or_else(|| Tokenizer::new(dialect.as_ref(), sql).tokenize());
    // A submission the tokenizer cannot read was refused as unreadable before
    // this is asked.
    let Ok(tokens) = tokens else {
        return false;
    };
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    tokens.windows(2).any(|pair| {
        matches!(pair, [Token::Word(variable), Token::Assignment] if variable.value.starts_with('@'))
    }) || tokens
        .windows(3)
        .any(|triple| matches!(triple, [Token::AtSign, Token::Word(_), Token::Assignment]))
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

    /// Every shape of `DELETE` on the engine whose dialect reads it, each with
    /// the text that makes the walk reach it somewhere other than the top.
    const DELETES: &[(Engine, &str)] = &[
        (Engine::Postgres, "DELETE FROM t WHERE id = 1"),
        (Engine::Postgres, "DELETE FROM t"),
        (
            Engine::Postgres,
            "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
        ),
        (
            Engine::Postgres,
            "WITH s AS (SELECT 1) DELETE FROM t WHERE id IN (SELECT * FROM s)",
        ),
        (
            Engine::Postgres,
            "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1",
        ),
        (
            Engine::Postgres,
            "EXPLAIN (ANALYZE TRUE) DELETE FROM t WHERE id = 1",
        ),
        (Engine::MariaDb, "ANALYZE DELETE FROM t WHERE id = 1"),
        (Engine::SqlServer, "IF 1 = 1 DELETE FROM t WHERE id = 1"),
        (
            Engine::SqlServer,
            "IF 1 = 1 SELECT 1 ELSE DELETE FROM t WHERE id = 1",
        ),
        (Engine::Snowflake, "BEGIN DELETE FROM t WHERE id = 1; END"),
        (Engine::ClickHouse, "ALTER TABLE t DELETE WHERE id = 1"),
        (Engine::ClickHouse, "DELETE FROM t WHERE id = 1"),
    ];

    #[test]
    fn every_shape_of_delete_is_refused() {
        for (engine, sql) in DELETES {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(*engine, sql, mode),
                    refused(Refusal::Delete),
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
        // A one-row delete by key is not softened for naming one row.
        for engine in sql_engines() {
            let one = agent_gate(engine, "DELETE FROM t WHERE id = 1", Mode::ReadWrite);
            let all = agent_gate(engine, "DELETE FROM t", Mode::ReadWrite);
            assert_eq!(one, refused(Refusal::Delete), "{engine:?}");
            assert_eq!(one, all, "{engine:?}");
        }
    }

    #[test]
    fn a_plain_explain_runs_and_explain_analyze_takes_the_inner_verdict() {
        let postgres = |sql, mode| agent_gate(Engine::Postgres, sql, mode);
        for mode in Mode::ALL {
            assert_eq!(
                postgres("EXPLAIN DELETE FROM t WHERE id = 1", mode),
                AgentVerdict::Run,
                "{mode:?}"
            );
            assert_eq!(
                postgres("EXPLAIN ANALYZE SELECT a FROM t", mode),
                AgentVerdict::Run,
                "{mode:?}"
            );
        }
        let update = "EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE id = 1";
        assert_eq!(
            postgres(update, Mode::ReadWrite),
            AgentVerdict::Approve(Verdict::WRITE)
        );
        assert_eq!(postgres(update, Mode::ReadOnly), needs_read_write());
    }

    #[test]
    fn alter_beyond_add_column_is_refused_by_name() {
        for sql in [
            "ALTER TABLE t DROP COLUMN b",
            "ALTER TABLE t RENAME TO u",
            "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
            "alter table t drop column b",
            "ALTER TABLE t ADD COLUMN b INT, DROP COLUMN c",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::Alter),
                    "{mode:?}: {sql}"
                );
            }
        }
        let add = "ALTER TABLE t ADD COLUMN b INT";
        for engine in sql_engines() {
            assert_eq!(
                agent_gate(engine, add, Mode::ReadWrite),
                AgentVerdict::Approve(Verdict::WRITE),
                "{engine:?}"
            );
        }
        // A ClickHouse mutation amounts to the statement it is written as.
        assert_eq!(
            agent_gate(
                Engine::ClickHouse,
                "ALTER TABLE t UPDATE a = 1 WHERE id = 1",
                Mode::ReadWrite
            ),
            AgentVerdict::Approve(Verdict::WRITE)
        );
    }

    #[test]
    fn a_statement_inside_an_executable_comment_is_counted() {
        for mode in Mode::ALL {
            for (sql, refusal) in [
                ("SELECT 1 /*M!100100 ; SELECT 2 */", Refusal::MoreThanOne),
                (
                    "SELECT 1 /*M!100100 ; DELETE FROM t WHERE id = 1 */",
                    Refusal::MoreThanOne,
                ),
                ("/*M!100100 DELETE FROM t WHERE id = 1 */", Refusal::Delete),
            ] {
                assert_eq!(
                    agent_gate(Engine::MariaDb, sql, mode),
                    refused(refusal),
                    "{mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn a_transaction_block_is_more_than_one_statement() {
        let sql = "BEGIN; DELETE FROM t WHERE id = 1; COMMIT";
        for engine in sql_engines() {
            for mode in Mode::ALL {
                let answer = agent_gate(engine, sql, mode);
                // A dialect that cannot read the block says so.
                assert!(
                    answer == refused(Refusal::MoreThanOne)
                        || answer == refused(Refusal::Unreadable),
                    "{engine:?} {mode:?}: {answer:?}"
                );
            }
        }
        assert_eq!(
            agent_gate(Engine::Postgres, sql, Mode::Full),
            refused(Refusal::MoreThanOne)
        );
    }

    /// The statements the classifier has no arm for, or cannot parse. None of
    /// them may be run or approved on any engine in any mode.
    const NO_ARM: &[&str] = &[
        "PREPARE p AS SELECT 1",
        "EXECUTE p",
        "DEALLOCATE p",
        "DO $$ BEGIN DELETE FROM t; END $$",
        "LOAD DATA INFILE 'x.csv' INTO TABLE t",
        "SELECT a INTO OUTFILE '/tmp/x' FROM t",
        "SELECT a FROM t INTO OUTFILE '/tmp/x'",
        "DECLARE c CURSOR FOR SELECT 1",
        "FETCH NEXT FROM c",
        "LISTEN c",
        "NOTIFY c",
        "LOCK TABLE t",
        "REFRESH MATERIALIZED VIEW v",
        "REINDEX TABLE t",
        "CLUSTER t",
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1' LANGUAGE SQL",
        "CREATE TRIGGER g AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "CREATE EXTENSION x",
        "ATTACH 'x.db' AS x",
        "INSTALL httpfs",
        "LOAD httpfs",
        "EXPORT DATABASE '/tmp/x'",
        "OPTIMIZE TABLE t FINAL",
        "KILL 1",
        "SET GLOBAL max_connections = 1",
        "XA START 'x'",
    ];

    #[test]
    fn statements_with_no_arm_of_their_own_are_never_run_or_approved() {
        for engine in sql_engines() {
            for sql in NO_ARM {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        matches!(answer, AgentVerdict::Refuse(_)),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_statements_are_refused_whatever_the_mode() {
        for sql in [
            "BEGIN",
            "START TRANSACTION READ WRITE",
            "COMMIT",
            "ROLLBACK",
            "SET search_path = other",
            "SET ROLE other",
            "SET SESSION TRANSACTION READ WRITE",
            "CREATE TEMPORARY TABLE u (a INT)",
        ] {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(Engine::Postgres, sql, mode),
                    refused(Refusal::Session),
                    "{mode:?}: {sql}"
                );
            }
        }
        // Stateless engines keep no session, and an agent's `SET` is refused
        // there all the same.
        for engine in [Engine::Snowflake, Engine::ClickHouse] {
            for sql in ["SET x = 1", "USE other"] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        answer == refused(Refusal::Session)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for engine in [Engine::MySql, Engine::MariaDb] {
            for sql in ["USE other", "SELECT @x := 1", "SELECT 1 INTO @x"] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        answer == refused(Refusal::Session)
                            || answer == refused(Refusal::Unreadable),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
        for engine in sql_engines() {
            for sql in [
                "BEGIN",
                "COMMIT",
                "SET search_path = other",
                "SET x = 1",
                "USE other",
                "SELECT @x := 1",
                "SELECT 1 INTO @x",
                "SET GLOBAL max_connections = 1",
            ] {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    assert!(
                        matches!(answer, AgentVerdict::Refuse(_)),
                        "{engine:?} {mode:?}: {sql}: {answer:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_savepoint_family_is_refused_as_a_session_statement() {
        for sql in [
            "SAVEPOINT a",
            "RELEASE SAVEPOINT a",
            "ROLLBACK TO SAVEPOINT a",
        ] {
            for engine in sql_engines() {
                for mode in Mode::ALL {
                    let answer = agent_gate(engine, sql, mode);
                    let reads_it = matches!(
                        engine,
                        Engine::Postgres
                            | Engine::MySql
                            | Engine::MariaDb
                            | Engine::Sqlite
                            | Engine::DuckDb
                    );
                    if reads_it {
                        assert_eq!(
                            answer,
                            refused(Refusal::Session),
                            "{engine:?} {mode:?}: {sql}"
                        );
                    } else {
                        assert!(
                            answer == refused(Refusal::Session)
                                || answer == refused(Refusal::Unreadable),
                            "{engine:?} {mode:?}: {sql}: {answer:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_read_of_session_state_still_runs() {
        for mode in Mode::ALL {
            for (engine, sql) in [
                (Engine::Postgres, "SELECT currval('s')"),
                (Engine::MySql, "SELECT last_insert_id()"),
                (Engine::Postgres, "SELECT make_interval(days := 10)"),
            ] {
                assert_eq!(
                    agent_gate(engine, sql, mode),
                    AgentVerdict::Run,
                    "{engine:?} {mode:?}: {sql}"
                );
            }
        }
    }

    #[test]
    fn every_refusal_names_its_rule_in_its_own_words() {
        let all = [
            (Refusal::NoStatement, "there is no statement to run"),
            (Refusal::MoreThanOne, "more than one statement"),
            (Refusal::Unreadable, "DBDelve can't read this statement"),
            (Refusal::Drop, "DROP is refused for agent statements"),
            (
                Refusal::Truncate,
                "TRUNCATE is refused for agent statements",
            ),
            (
                Refusal::Delete,
                "deleting data is refused for agent statements",
            ),
            (
                Refusal::Alter,
                "ALTER other than ADD COLUMN is refused for agent statements",
            ),
            (
                Refusal::Session,
                "statements that change the session are refused for agent statements",
            ),
            (
                Refusal::NeedsFull,
                "statements that need Full are refused for agent statements",
            ),
            (
                Refusal::NeedsMode {
                    needed: Mode::ReadWrite,
                    held: Mode::ReadOnly,
                },
                "needs Read-write; the profile is Read-only",
            ),
        ];
        let mut seen = Vec::new();
        for (refusal, text) in all {
            let said = refusal.to_string();
            assert_eq!(said, text);
            assert!(!seen.contains(&said), "{said} is said twice");
            for advice in ["upgrade", "try", "instead", "please", "should"] {
                assert!(
                    !said.to_lowercase().contains(advice),
                    "{said} gives advice: {advice}"
                );
            }
            seen.push(said);
        }
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn a_cell_with_two_reasons_has_one_defined_answer() {
        let postgres = |sql, mode| agent_gate(Engine::Postgres, sql, mode);
        assert_eq!(
            postgres("DROP TABLE a; DROP TABLE b", Mode::Full),
            refused(Refusal::MoreThanOne)
        );
        assert_eq!(
            postgres("SELECT 1; garbage", Mode::Full),
            refused(Refusal::Unreadable)
        );
        assert_eq!(
            postgres("DELETE FROM t WHERE id = 1", Mode::ReadOnly),
            refused(Refusal::Delete)
        );
        assert_eq!(
            postgres("ALTER TABLE t ADD COLUMN b INT, DROP COLUMN c", Mode::Full),
            refused(Refusal::Alter)
        );
        assert_eq!(
            postgres("GRANT SELECT ON t TO u", Mode::Full),
            refused(Refusal::NeedsFull)
        );
    }

    #[test]
    fn the_same_question_gets_the_same_answer() {
        for (engine, case, text) in crate::sql_snapshots::agent::sql_cells() {
            for mode in Mode::ALL {
                assert_eq!(
                    agent_gate(engine, &text, mode),
                    agent_gate(engine, &text, mode),
                    "{engine:?} {mode:?} {case}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn replacing_a_row_is_approvable() {
        for (engine, sql) in [
            (Engine::MySql, "REPLACE INTO t (a) VALUES (1)"),
            (Engine::MariaDb, "REPLACE INTO t (a) VALUES (1)"),
            (Engine::Sqlite, "INSERT OR REPLACE INTO t (a) VALUES (1)"),
        ] {
            assert_eq!(
                agent_gate(engine, sql, Mode::ReadWrite),
                AgentVerdict::Approve(Verdict::WRITE),
                "{engine:?}: {sql}"
            );
        }
    }
}
