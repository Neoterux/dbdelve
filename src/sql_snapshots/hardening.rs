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
const CALLS: &[(&str, &str)] = &[
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
    ("get_lock", "'k', 1"),
    ("release_lock", "'k'"),
    ("release_all_locks", ""),
    ("load_extension", "'x'"),
    ("system$cancel_all_queries", "1"),
    ("system$cancel_query", "'q'"),
    ("system$abort_session", "1"),
    ("getnextval", "seq1"),
    ("openquery", "srv, 'SELECT 1'"),
    ("openrowset", "'SQLNCLI', 'x', 'DELETE FROM t'"),
    ("opendatasource", "'SQLNCLI', 'x'"),
];

/// The same call in each position the parser can put one, and the controls that
/// must never move: a function nobody listed, and a projection calling nothing.
const SHAPES: &[(&str, &str)] = &[
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
    (
        "openquery in table position",
        "SELECT * FROM OPENQUERY(srv, 'SELECT 1')",
    ),
    (
        "openrowset in table position",
        "SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'DELETE FROM t')",
    ),
    (
        "opendatasource in table position",
        "SELECT * FROM OPENDATASOURCE('SQLNCLI', 'x')",
    ),
    (
        "table function wrapper",
        "SELECT * FROM TABLE(GETNEXTVAL(seq1))",
    ),
    ("sequence without parentheses", "SELECT s.nextval"),
    (
        "sequence without parentheses, qualified",
        "SELECT db.s.nextval FROM t",
    ),
    ("control qualified column", "SELECT t.id FROM t"),
    (
        "control qualified column sharing no listed name",
        "SELECT t.nextvalue FROM t",
    ),
    ("control session read lastval", "SELECT lastval()"),
    ("control session read currval", "SELECT currval('s')"),
    ("control last_insert_id", "SELECT last_insert_id()"),
    ("control sleep", "SELECT sleep(1)"),
    ("control file reader", "SELECT * FROM read_csv('f.csv')"),
    ("control file string factor", "SELECT * FROM 'f.csv'"),
    (
        "control pg_read_file",
        "SELECT pg_read_file('/etc/hostname')",
    ),
    ("control load_file", "SELECT LOAD_FILE('/etc/hostname')"),
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
