//! What `sql::agent_gate` answers for each kind of statement an agent could
//! send, recorded per SQL engine and per mode, so every answer is one line of a
//! `.snap` read once and held. A record is the answer in Read-only, Read-write
//! and Full, in that order, for the text exactly as written.
//!
//! A `.snap` accepted by regeneration alone reviews nothing: the explicit
//! tests in `sql::agent` pin each cell the success criteria name, and the
//! invariants below hold over every cell recorded here.

use super::{check, fragment, visible};
use crate::{
    db::{Engine, Syntax},
    sql::{self, AgentVerdict, Mode},
};

/// Stands for the engine's own first deny-listed function, called with no
/// arguments, in a kind below. An engine that lists none records `(none)`.
const DENIED_CALL: &str = "<the engine's first deny-listed function>";

/// The kinds, in the order they are recorded: a case name and the text sent.
const SQL_KINDS: &[(&str, &str)] = &[
    ("plain select", "SELECT a FROM t"),
    ("scalar function", "SELECT lower(a) FROM t"),
    ("user-defined function", "SELECT my_udf(a) FROM t"),
    ("deny-listed call", DENIED_CALL),
    ("insert", "INSERT INTO t (a) VALUES (1)"),
    ("update", "UPDATE t SET a = 1 WHERE id = 1"),
    ("update without where", "UPDATE t SET a = 1"),
    ("create table", "CREATE TABLE u (a INT)"),
    ("create view", "CREATE VIEW v AS SELECT a FROM t"),
    ("create schema", "CREATE SCHEMA s"),
    ("create index", "CREATE INDEX i ON t (a)"),
    (
        "create index if not exists",
        "CREATE INDEX IF NOT EXISTS i ON t (a)",
    ),
    (
        "create index concurrently",
        "CREATE INDEX CONCURRENTLY i ON t (a)",
    ),
    ("comment", "COMMENT ON TABLE t IS 'x'"),
    ("analyze", "ANALYZE t"),
    ("vacuum", "VACUUM t"),
    ("add column", "ALTER TABLE t ADD COLUMN b INT"),
    ("filtered delete", "DELETE FROM t WHERE id = 1"),
    ("unfiltered delete", "DELETE FROM t"),
    ("drop", "DROP TABLE t"),
    ("drop, lower case", "drop table t"),
    ("drop, mixed case", "DrOp TaBlE t"),
    ("truncate", "TRUNCATE TABLE t"),
    ("truncate, lower case", "truncate t"),
    ("grant", "GRANT SELECT ON t TO u"),
    ("call", "CALL p()"),
    ("copy to a file", "COPY t TO '/tmp/x.csv'"),
    ("show tables", "SHOW TABLES"),
    ("describe", "DESCRIBE t"),
    ("plain explain", "EXPLAIN SELECT a FROM t"),
    ("empty", ""),
    ("whitespace only", "   "),
    ("line comment only", "-- nothing"),
    ("block comment only", "/* x */"),
    ("two reads", "SELECT 1; SELECT 2"),
    ("two drops", "DROP TABLE a; DROP TABLE b"),
    ("read then garbage", "SELECT 1; garbage"),
    ("a typo", "selec 1"),
    ("read, then a line comment", "SELECT 1; -- c"),
    ("line comment, then a read", "-- c\nSELECT 1"),
    ("block comment, then a read", "/* c */ SELECT 1"),
    ("read, then a hash comment", "SELECT 1 # c"),
];

/// The text of one kind on one engine, `None` where the engine has none: the
/// deny-listed call of an engine that lists no function.
fn text_of(engine: Engine, sql: &str) -> Option<String> {
    if sql != DENIED_CALL {
        return Some(sql.to_owned());
    }
    engine
        .spec()
        .sql
        .denied_functions
        .first()
        .map(|name| format!("SELECT {name}()"))
}

fn sql_engines() -> impl Iterator<Item = Engine> {
    Engine::ALL
        .into_iter()
        .filter(|engine| engine.syntax() == Syntax::Sql)
}

/// Every `(engine, case, text)` the corpus records, in the order it records
/// them: `Engine::ALL`, then the kinds, skipping what an engine has no text for.
pub(super) fn sql_cells() -> Vec<(Engine, String, String)> {
    sql_engines()
        .flat_map(|engine| {
            SQL_KINDS.iter().filter_map(move |(case, sql)| {
                text_of(engine, sql).map(|text| (engine, (*case).to_owned(), text))
            })
        })
        .collect()
}

fn answer(verdict: &AgentVerdict) -> String {
    match verdict {
        AgentVerdict::Run => "run".to_owned(),
        AgentVerdict::Approve(verdict) => format!("approve({:?})", verdict.mode),
        AgentVerdict::Refuse(refusal) => format!("refuse({refusal:?})"),
    }
}

#[test]
fn agent_gate_corpus_for_sql_engines() {
    let mut out = String::new();
    for engine in sql_engines() {
        for (case, sql) in SQL_KINDS {
            let Some(text) = text_of(engine, sql) else {
                fragment(&mut out, engine, case, None);
                continue;
            };
            let [ro, rw, full] =
                Mode::ALL.map(|mode| answer(&sql::agent_gate(engine, &text, mode)));
            fragment(
                &mut out,
                engine,
                &format!("{case}: {}", visible(&text)),
                Some(&format!("ro={ro} rw={rw} full={full}")),
            );
        }
    }
    check("agent_gate_sql", &out);
}

#[test]
fn every_run_or_approve_cell_is_one_the_mode_already_allows() {
    for (engine, case, text) in sql_cells() {
        let verdict = sql::classify(engine, &text);
        for mode in Mode::ALL {
            match sql::agent_gate(engine, &text, mode) {
                AgentVerdict::Run | AgentVerdict::Approve(_) => assert_eq!(
                    sql::gate(&verdict, mode, &[]),
                    None,
                    "{engine:?} {mode:?} {case}: {text:?}"
                ),
                AgentVerdict::Refuse(_) => {}
            }
        }
    }
}

#[test]
fn every_run_cell_is_a_rerunnable_read() {
    for (engine, case, text) in sql_cells() {
        for mode in Mode::ALL {
            if sql::agent_gate(engine, &text, mode) == AgentVerdict::Run {
                assert!(
                    sql::rerunnable(engine, &text),
                    "{engine:?} {mode:?} {case}: {text:?}"
                );
            }
        }
    }
}

#[test]
fn the_full_column_equals_the_read_write_column() {
    for (engine, case, text) in sql_cells() {
        assert_eq!(
            sql::agent_gate(engine, &text, Mode::Full),
            sql::agent_gate(engine, &text, Mode::ReadWrite),
            "{engine:?} {case}: {text:?}"
        );
    }
}

/// What an agent reads for a refusal is the rule that refused and nothing else:
/// no advice, no way round it.
fn reason(refusal: &sql::Refusal) -> String {
    refusal.to_string().to_lowercase()
}

#[test]
fn every_refusal_in_the_corpus_states_a_rule_and_gives_no_advice() {
    for (engine, case, text) in sql_cells() {
        for mode in Mode::ALL {
            let AgentVerdict::Refuse(refusal) = sql::agent_gate(engine, &text, mode) else {
                continue;
            };
            let reason = reason(&refusal);
            assert!(!reason.is_empty(), "{engine:?} {mode:?} {case}");
            for advice in ["upgrade", "try", "instead", "please", "should"] {
                assert!(
                    !reason.contains(advice),
                    "{engine:?} {mode:?} {case}: {reason}"
                );
            }
        }
    }
}
