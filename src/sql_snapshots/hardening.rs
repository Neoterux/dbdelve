//! The verdict for every call the classifier hardening reclassifies, recorded per
//! engine, so each change reads as a one-line diff in the `.snap` rather than as
//! a new file. A record is `classify` and `gate` for the text exactly as
//! written, on every engine, which is what shows a call moving only where the
//! engine lists it.

use super::{check, fragment};
use crate::{
    db::Engine,
    sql::{self, Mode},
};

/// Function calls whose verdict the deny-list moves, one name and its arguments
/// each, written as a plain `SELECT` so the scalar position is the one under
/// test.
const CALLS: [(&str, &str); 31] = [
    (
        "set_config",
        "'default_transaction_read_only', 'off', false",
    ),
    ("nextval", "'s'"),
    ("setval", "'s', 10"),
    ("pg_advisory_lock", "1"),
    ("pg_advisory_lock_shared", "1"),
    ("pg_advisory_xact_lock", "1"),
    ("pg_advisory_xact_lock_shared", "1"),
    ("pg_try_advisory_lock", "1"),
    ("pg_try_advisory_lock_shared", "1"),
    ("pg_try_advisory_xact_lock", "1"),
    ("pg_try_advisory_xact_lock_shared", "1"),
    ("pg_advisory_unlock", "1"),
    ("pg_advisory_unlock_shared", "1"),
    ("pg_advisory_unlock_all", ""),
    ("pg_notify", "'c', 'p'"),
    ("pg_terminate_backend", "0"),
    ("pg_cancel_backend", "0"),
    ("pg_reload_conf", ""),
    ("pg_rotate_logfile", ""),
    ("pg_switch_wal", ""),
    ("pg_create_restore_point", "'x'"),
    ("pg_stat_reset", ""),
    ("lo_create", "0"),
    ("lo_import", "'/etc/hostname'"),
    ("lo_export", "1, '/tmp/x'"),
    ("lo_unlink", "1"),
    ("dblink", "'c', 'SELECT 1'"),
    ("dblink_exec", "'c', 'DELETE FROM t'"),
    ("dblink_connect", "'c', 'dbname=x'"),
    ("dblink_connect_u", "'c', 'dbname=x'"),
    ("dblink_send_query", "'c', 'SELECT 1'"),
];

/// The same call in each position the parser can put one, and the controls that
/// must never move: a function nobody listed, and a projection calling nothing.
const SHAPES: [(&str, &str); 10] = [
    ("table position", "SELECT * FROM nextval('s')"),
    (
        "lateral table position",
        "SELECT * FROM t, LATERAL nextval('s') x",
    ),
    (
        "schema-qualified",
        "SELECT pg_catalog.set_config('a', 'b', false)",
    ),
    (
        "where subquery",
        "SELECT * FROM t WHERE id = (SELECT nextval('s'))",
    ),
    (
        "cte body",
        "WITH n AS (SELECT nextval('s') AS v) SELECT v FROM n",
    ),
    ("comment before paren", "SELECT nextval /* c */ ('s')"),
    (
        "two denied calls",
        "SELECT nextval('s'), set_config('a', 'b', false)",
    ),
    (
        "two denied calls reversed",
        "SELECT set_config('a', 'b', false), nextval('s')",
    ),
    ("control user-defined call", "SELECT my_function(1)"),
    ("control plain projection", "SELECT a, b FROM t"),
];

fn record(out: &mut String, engine: Engine, case: &str, sql: &str) {
    let verdict = sql::classify(engine, sql);
    let [ro, rw, full] = Mode::ALL.map(|mode| sql::gate(&verdict, mode, &[]));
    let record = format!(
        "classify={:?} {:?} gate ro={ro:?} rw={rw:?} full={full:?}",
        verdict.mode, verdict.destructive,
    );
    fragment(out, engine, &format!("{case}: {sql}"), Some(&record));
}

#[test]
fn denied_function_corpus_per_engine() {
    let mut out = String::new();
    for engine in Engine::ALL {
        for (name, args) in CALLS {
            record(
                &mut out,
                engine,
                "scalar",
                &format!("SELECT {name}({args})"),
            );
        }
        for (case, sql) in SHAPES {
            record(&mut out, engine, case, sql);
        }
    }
    check("hardening_functions", &out);
}
