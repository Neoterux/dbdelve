//! One set of scenarios every engine's live tests run unchanged, against the
//! small `conformance` fixture each dev database is seeded with.
//!
//! An engine's `tests` module builds a [`Subject`] (an open connection plus
//! the per-engine data a scenario cannot derive) and lists the scenarios it
//! takes in [`suite!`]. Engines are asked only through `Engine` and
//! `db::Connection`, so a scenario that fails is a finding about the engine,
//! not something to bend the scenario around.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::{
    CancelToken, Connection, Engine, Fetch, QueryResult, RelationKind, Stopped, is_cancel,
};
use crate::result_grid::{NewValue, PendingRow};
use crate::sql;

pub(super) struct Subject {
    pub connection: Connection,
    pub engine: Engine,
    pub schema: &'static str,
    pub relation: &'static str,
    pub key: &'static str,
    pub columns: &'static [&'static str],
    pub read: String,
    pub endless: &'static str,
    pub runaway: &'static str,
    pub read_only_refusal: &'static str,
    pub fixture: Option<Fixture>,
    /// Kept alive until the scenario ends, and dropped after `connection`.
    pub _guard: Option<Box<dyn std::any::Any>>,
}

pub(super) struct Fixture {
    pub types: &'static [&'static str],
    pub renders: &'static [(&'static str, usize, &'static str)],
    pub find: Box<dyn Fn(&str) -> String>,
}

/// Puts the fixture back when a scenario ends, panicking or not: whatever
/// transaction it left open is rolled back, the session is made writable
/// again and the ids it wrote are deleted. Errors are ignored, since the
/// scenario's own failure is the one worth reading.
struct Cleanup<'a> {
    connection: &'a Connection,
    statements: Vec<String>,
    rollback: bool,
    read_write: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let run = |sql: &str| {
            let _ = self
                .connection
                .query(sql, &CancelToken::default(), Fetch::default());
        };
        if self.rollback {
            run("ROLLBACK");
        }
        if self.read_write {
            let _ = self.connection.set_read_only(false);
        }
        for statement in &self.statements {
            run(statement);
        }
    }
}

pub(super) const COLUMNS: [&str; 9] = [
    "id", "label", "whole", "fraction", "ratio", "flag", "day", "moment", "bytes",
];

pub(super) const EXPECTED: [[Option<&str>; 9]; 3] = [
    [
        Some("1"),
        Some("alpha"),
        Some("9007199254740993"),
        Some("12.50"),
        Some("0.5"),
        Some("1"),
        Some("2024-02-29"),
        Some("2024-02-29 12:34:56"),
        Some("0x00FF"),
    ],
    [
        Some("2"),
        Some("Zoë 李 🐉 'quoted'"),
        Some("-42"),
        Some("-0.01"),
        Some("-1.25"),
        Some("0"),
        Some("1999-12-31"),
        Some("1999-12-31 23:59:59"),
        Some("0xDEADBEEF"),
    ],
    [
        Some("3"),
        Some("nulls"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ],
];

const WITHIN: Duration = Duration::from_secs(10);

pub(super) fn sql_read(engine: Engine, schema: &str) -> String {
    let columns: Vec<String> = COLUMNS
        .iter()
        .map(|column| engine.quote_identifier(column))
        .collect();
    format!(
        "SELECT {} FROM {} WHERE {} < 900 ORDER BY {}",
        columns.join(", "),
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
        engine.quote_identifier("id"),
    )
}

pub(super) fn sql_find(engine: Engine, schema: &str, key: &str) -> String {
    format!(
        "SELECT {} FROM {} WHERE {} = {key}",
        engine.quote_identifier("label"),
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
    )
}

fn sql_insert(engine: Engine, schema: &str, id: u32, label: &str) -> String {
    format!(
        "INSERT INTO {} ({}, {}) VALUES ({id}, {})",
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
        engine.quote_identifier("label"),
        engine.quote_literal(label),
    )
}

fn sql_delete(engine: Engine, schema: &str, id: u32) -> String {
    format!(
        "DELETE FROM {} WHERE {} = {id}",
        engine.qualified(schema, "conformance"),
        engine.quote_identifier("id"),
    )
}

macro_rules! suite {
    ($module:ident, $why:literal, $subject:ident: $($scenario:ident),+ $(,)?) => {
        mod $module {
            $(
                #[test]
                #[ignore = $why]
                fn $scenario() {
                    crate::db::conformance::$scenario(&super::$subject());
                }
            )+
        }
    };
}
pub(super) use suite;

impl Subject {
    fn run(&self, scenario: &str, sql: &str, fetch: Fetch) -> QueryResult {
        self.connection
            .query(sql, &CancelToken::default(), fetch)
            .unwrap_or_else(|error| {
                panic!("{scenario} on {:?}: `{sql}` failed: {error}", self.engine)
            })
    }

    fn read(&self, scenario: &str) -> QueryResult {
        self.run(scenario, &self.read, Fetch::default())
    }

    fn fixture(&self, scenario: &str) -> &Fixture {
        self.fixture
            .as_ref()
            .unwrap_or_else(|| panic!("{scenario} on {:?} needs a fixture", self.engine))
    }

    fn label_of(&self, scenario: &str, key: &str) -> Vec<Option<String>> {
        let find = (self.fixture(scenario).find)(key);
        self.run(scenario, &find, Fetch::default())
            .rows
            .iter()
            .map(|row| row.get(0).map(str::to_string))
            .collect()
    }
}

fn names(result: &QueryResult) -> Vec<&str> {
    result.columns.iter().map(|c| c.name.as_str()).collect()
}

pub(super) fn connects(subject: &Subject) {
    assert!(
        !subject.connection.is_lost(),
        "connects on {:?}: the session is lost",
        subject.engine
    );
    let result = subject.read("connects");
    assert_eq!(
        names(&result),
        subject.columns,
        "connects on {:?}",
        subject.engine
    );
}

pub(super) fn query_round_trip(subject: &Subject) {
    let at = format!("query_round_trip on {:?}", subject.engine);
    let result = subject.read("query_round_trip");
    assert_eq!(names(&result), subject.columns, "{at}");
    assert_eq!(result.rows.len(), 3, "{at}");
    for row in &result.rows[..2] {
        assert!(row.iter().all(|cell| cell.is_some()), "{at}: {row:?}");
    }
    let nulls = &result.rows[2];
    assert!(nulls.get(0).is_some() && nulls.get(1).is_some(), "{at}");
    assert!((2..9).all(|col| nulls.get(col).is_none()), "{at}");
}

pub(super) fn catalog_round_trip(subject: &Subject) {
    let at = format!("catalog_round_trip on {:?}", subject.engine);
    let catalog = subject
        .connection
        .catalog()
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let schema = catalog
        .schemas
        .iter()
        .find(|schema| schema.name == subject.schema)
        .unwrap_or_else(|| panic!("{at}: no schema {}", subject.schema));
    let relation = schema
        .relations
        .iter()
        .find(|relation| relation.name == subject.relation)
        .unwrap_or_else(|| panic!("{at}: no relation {}", subject.relation));
    assert_eq!(relation.kind, RelationKind::Table, "{at}");
    if subject.engine.copies_ddl() {
        let ddl = subject
            .connection
            .ddl(subject.schema, subject.relation, RelationKind::Table)
            .unwrap_or_else(|error| panic!("{at}: ddl: {error}"));
        assert!(ddl.contains(subject.relation), "{at}: {ddl}");
    }
}

pub(super) fn structure_round_trip(subject: &Subject) {
    let at = format!("structure_round_trip on {:?}", subject.engine);
    let structure = subject
        .connection
        .structure(subject.schema, subject.relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let columns: Vec<&str> = structure.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, subject.columns, "{at}");
    let key = structure
        .columns
        .iter()
        .find(|column| column.name == subject.key)
        .unwrap_or_else(|| panic!("{at}: no key column"));
    assert!(!key.nullable, "{at}: the key column is nullable");
    assert_eq!(structure.primary_key(), [subject.key], "{at}");
}

pub(super) fn types_render_as_expected(subject: &Subject) {
    let at = format!("types_render_as_expected on {:?}", subject.engine);
    let fixture = subject.fixture("types_render_as_expected");
    let result = subject.read("types_render_as_expected");
    let types: Vec<Option<&str>> = result
        .columns
        .iter()
        .map(|column| column.data_type.as_deref())
        .collect();
    let wanted: Vec<Option<&str>> = fixture.types.iter().map(|t| Some(*t)).collect();
    assert_eq!(types, wanted, "{at}: column types");
    assert_eq!(result.rows.len(), 3, "{at}");
    for (row, expected) in EXPECTED.iter().enumerate() {
        for (col, base) in expected.iter().enumerate() {
            let name = subject.columns[col];
            let over = fixture
                .renders
                .iter()
                .find(|(column, at_row, _)| *column == name && *at_row == row)
                .map(|(_, _, text)| *text);
            assert_eq!(
                result.rows[row].get(col),
                over.or(*base),
                "{at}: row {row}, column {name}"
            );
        }
    }
}

pub(super) fn cancel_stops_a_long_query(subject: &Subject) {
    let at = format!("cancel_stops_a_long_query on {:?}", subject.engine);
    let token = CancelToken::default();
    let (sender, receiver) = mpsc::channel();
    let connection = subject.connection.clone();
    let (sql, run_token) = (subject.runaway, token.clone());
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = sender.send(connection.query(sql, &run_token, Fetch::default()));
    });
    let outcome = loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(outcome) => break outcome,
            Err(_) if started.elapsed() >= WITHIN => panic!("{at}: still running after {WITHIN:?}"),
            Err(_) => {
                let _ = subject.connection.cancel(&token);
            }
        }
    };
    let error = outcome.expect_err(&format!("{at}: the run should have been cancelled"));
    assert!(is_cancel(subject.engine, &error), "{at}: {}", error.message);
    assert!(started.elapsed() < WITHIN, "{at}");
    subject.read("cancel_stops_a_long_query");
}

pub(super) fn row_limit_stops_an_eligible_read(subject: &Subject) {
    let at = format!("row_limit_stops_an_eligible_read on {:?}", subject.engine);
    assert!(sql::stoppable(subject.engine, subject.endless), "{at}");
    let started = Instant::now();
    let result = subject.run(
        "row_limit_stops_an_eligible_read",
        subject.endless,
        Fetch {
            limit: Some(3),
            reads_only: true,
            ..Fetch::default()
        },
    );
    assert_eq!(result.stopped, Some(Stopped::AtLimit), "{at}");
    assert_eq!(result.capped_from, None, "{at}");
    assert_eq!(result.rows.len(), 3, "{at}");
    assert!(started.elapsed() < WITHIN, "{at}");
}

pub(super) fn row_limit_drains_an_ineligible_read(subject: &Subject) {
    let at = format!(
        "row_limit_drains_an_ineligible_read on {:?}",
        subject.engine
    );
    let full = subject.read("row_limit_drains_an_ineligible_read");
    let capped = subject.run(
        "row_limit_drains_an_ineligible_read",
        &subject.read,
        Fetch {
            limit: Some(2),
            reads_only: false,
            ..Fetch::default()
        },
    );
    assert_eq!(capped.stopped, None, "{at}");
    assert_eq!(capped.capped_from, Some(3), "{at}");
    assert_eq!(capped.rows, full.rows[..2], "{at}");
    assert_eq!(capped.columns, full.columns, "{at}");
}

pub(super) fn capped_read_keeps_the_users_transaction(subject: &Subject) {
    let scenario = "capped_read_keeps_the_users_transaction";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let delete = sql_delete(engine, subject.schema, 950);
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![delete.clone()],
        rollback: true,
        read_write: false,
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    let begin = engine.transaction_start().unwrap_or("BEGIN");
    let capped_read_in_a_transaction = || {
        run(&delete);
        run(begin);
        run(&sql_insert(engine, subject.schema, 950, "in transaction"));
        let capped = subject.run(
            scenario,
            &subject.read,
            Fetch {
                limit: Some(2),
                reads_only: sql::stoppable(engine, &subject.read),
                ..Fetch::default()
            },
        );
        assert_eq!(capped.rows.len(), 2, "{at}");
        assert!(
            (capped.stopped == Some(Stopped::AtLimit) && capped.capped_from.is_none())
                || (capped.stopped.is_none() && capped.capped_from == Some(3)),
            "{at}: stopped {:?}, capped_from {:?}",
            capped.stopped,
            capped.capped_from
        );
        assert_eq!(
            subject.label_of(scenario, "950"),
            [Some("in transaction".to_string())],
            "{at}: the open transaction lost its own write"
        );
    };
    let seen_apart = || {
        let apart = subject
            .connection
            .apart()
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        let find = (subject.fixture(scenario).find)("950");
        let seen = apart
            .query(&find, &CancelToken::default(), Fetch::default())
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        seen.rows
            .iter()
            .map(|row| row.get(0).map(str::to_string))
            .collect::<Vec<_>>()
    };

    // A read that had quietly committed would leave nothing for ROLLBACK to
    // undo, so the row surviving it is the proof the transaction was cut short.
    capped_read_in_a_transaction();
    run("ROLLBACK");
    assert!(
        subject.label_of(scenario, "950").is_empty(),
        "{at}: ROLLBACK left the row, so the capped read ended the transaction"
    );
    assert!(
        seen_apart().is_empty(),
        "{at}: another session sees the row"
    );

    capped_read_in_a_transaction();
    run("COMMIT");
    assert_eq!(
        seen_apart(),
        [Some("in transaction".to_string())],
        "{at}: a fresh session does not see the committed row"
    );
}

pub(super) fn read_only_session_refuses_a_write(subject: &Subject) {
    let scenario = "read_only_session_refuses_a_write";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    if !engine.holds_read_only() {
        subject
            .connection
            .set_read_only(true)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        subject
            .connection
            .set_read_only(false)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
        return;
    }
    let delete = sql_delete(engine, subject.schema, 960);
    let insert = sql_insert(engine, subject.schema, 960, "read-only");
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![delete.clone()],
        rollback: false,
        read_write: true,
    };
    subject.run(scenario, &delete, Fetch::default());
    subject
        .connection
        .set_read_only(true)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let refused = subject
        .connection
        .query(&insert, &CancelToken::default(), Fetch::default());
    subject
        .connection
        .set_read_only(false)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let error = refused.expect_err(&format!("{at}: a read-only session took a write"));
    assert!(
        error.message.contains(subject.read_only_refusal),
        "{at}: refused for another reason: {}",
        error.message
    );
    subject.run(scenario, &insert, Fetch::default());
}

/// The classifier's new verdicts against what the server does, as a pair per
/// case: the statement is rated above a read, and the server really does what
/// that rating claims. Each is matched by a control that must not move, a
/// statement whose effect on the hold is none, which is rated a read and
/// leaves the canary write refused in the same session -- without it a server
/// that refused everything would pass.
pub(super) fn denied_functions_agree_with_the_server(subject: &Subject) {
    let scenario = "denied_functions_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let schema = subject.schema;
    // Row 970, one lock key, one large object and one sequence, all named for
    // this scenario: CI runs four scenarios at a time against one server.
    let canary = 970;
    let sequence = engine.qualified(schema, "dbdelve_denied_970");
    let insert = sql_insert(engine, schema, canary, "denied function");
    let rated = |sql: &str| sql::classify(engine, sql).mode;
    let above_a_read = |sql: &str| {
        assert!(
            rated(sql) > sql::Mode::ReadOnly,
            "{at}: `{sql}` is rated {:?}, which a Read-only profile would run",
            rated(sql)
        );
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let cell = |result: &QueryResult| {
        result
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: no value came back"))
            .to_string()
    };

    let mut unwind = vec![
        sql_delete(engine, schema, canary),
        format!("DROP SEQUENCE IF EXISTS {sequence}"),
    ];
    let (held_calls, control, advance, current): (Vec<(&str, &str)>, &str, String, String) =
        match engine {
            Engine::Postgres => {
                unwind.push("SELECT pg_advisory_unlock_all()".to_string());
                unwind.push("SELECT lo_unlink(970970)".to_string());
                (
                    vec![
                        ("an advisory lock", "SELECT pg_advisory_lock(970970)"),
                        (
                            "a notification",
                            "SELECT pg_notify('dbdelve_denied_970', 'x')",
                        ),
                        ("a large object", "SELECT lo_create(970970)"),
                    ],
                    "SELECT current_setting('search_path')",
                    format!("SELECT nextval('{schema}.dbdelve_denied_970')"),
                    format!("SELECT currval('{schema}.dbdelve_denied_970')"),
                )
            }
            Engine::MySql | Engine::MariaDb => {
                unwind.push("SELECT RELEASE_ALL_LOCKS()".to_string());
                (
                    vec![("a named lock", "SELECT GET_LOCK('dbdelve_denied_970', 0)")],
                    "SELECT version()",
                    format!("SELECT NEXTVAL({sequence})"),
                    format!("SELECT LASTVAL({sequence})"),
                )
            }
            _ => (
                Vec::new(),
                "SELECT version()",
                format!("SELECT nextval('{schema}.dbdelve_denied_970')"),
                format!("SELECT currval('{schema}.dbdelve_denied_970')"),
            ),
        };
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: unwind,
        rollback: false,
        read_write: engine.holds_read_only(),
    };

    // An engine with no hold has nothing to differ on here: both directions
    // of the switch must still be accepted, and the sequence case below runs.
    if !engine.holds_read_only() {
        hold(true);
        hold(false);
    } else {
        let refused = || {
            let error = subject
                .connection
                .query(&insert, &CancelToken::default(), Fetch::default())
                .expect_err(&format!("{at}: the hold took a write it should refuse"));
            assert!(
                error.message.contains(subject.read_only_refusal),
                "{at}: refused for another reason: {}",
                error.message
            );
        };
        // The control: rated a read, runs on the held session, and the hold
        // is still armed after it.
        let controlled = || {
            assert_eq!(
                rated(control),
                sql::Mode::ReadOnly,
                "{at}: control `{control}`"
            );
            hold(true);
            subject.run(scenario, control, Fetch::default());
            refused();
            hold(false);
        };

        for (case, call) in &held_calls {
            above_a_read(call);
            hold(true);
            // The call itself succeeds with the hold armed, which is why the
            // verdict has to be the one to stop it.
            subject.run(&format!("{scenario}: {case}"), call, Fetch::default());
            hold(false);
            controlled();
        }

        if engine == Engine::Postgres {
            let disarm = "SELECT set_config('default_transaction_read_only', 'off', false)";
            above_a_read(disarm);
            hold(true);
            refused();
            subject.run(scenario, disarm, Fetch::default());
            // The write that was refused a moment ago now goes through.
            subject.run(scenario, &insert, Fetch::default());
            hold(false);
            controlled();
        }
    }

    if engine.spec().sql.denied_functions.contains(&"nextval") {
        subject.run(
            scenario,
            &format!("CREATE SEQUENCE {sequence}"),
            Fetch::default(),
        );
        above_a_read(&advance);
        assert_eq!(rated(&current), sql::Mode::ReadOnly, "{at}: `{current}`");
        let first: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        let second: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            second,
            first + 1,
            "{at}: the call did not advance the sequence"
        );
        // The control reads where the sequence stands and leaves it there.
        let read: i64 = cell(&subject.run(scenario, &current, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            read, second,
            "{at}: the read-only spelling moved the sequence"
        );
        let third: i64 = cell(&subject.run(scenario, &advance, Fetch::default()))
            .parse()
            .unwrap_or_else(|_| panic!("{at}: the sequence did not answer a number"));
        assert_eq!(
            third,
            second + 1,
            "{at}: the control disturbed the sequence"
        );
    }
}

/// The classifier's verdicts on the statements that undo a Read-only hold,
/// against what the server does with them. Each statement is rated above a
/// read, and with the hold armed the canary write the server refused a moment
/// ago goes through after it. The one statement that is not a disarm is the
/// identity change: the hold stays armed, so that case asserts on the session's
/// reported user instead, and on the hold still being on.
///
/// Every positive is matched by a control from `plain_settings`, rated a read,
/// that leaves the same canary write refused in the same session. Without the
/// controls a server that refused everything would pass.
pub(super) fn session_settings_agree_with_the_server(subject: &Subject) {
    let scenario = "session_settings_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let schema = subject.schema;
    let rated = |sql: &str| sql::classify(engine, sql).mode;
    let above_a_read = |sql: &str| {
        assert!(
            rated(sql) > sql::Mode::ReadOnly,
            "{at}: `{sql}` is rated {:?}, which a Read-only profile would run",
            rated(sql)
        );
    };

    // The statement an engine sends to release its own hold is the one the
    // classifier must never let a Read-only profile run. Pure, so it covers
    // every engine and not only the live one.
    for each in Engine::ALL {
        if let Some(release) = super::read_only_statement(each, false) {
            assert!(
                sql::classify(each, release).mode > sql::Mode::ReadOnly,
                "{at}: the statement {each:?} sends to release its hold, `{release}`, is rated {:?}",
                sql::classify(each, release).mode
            );
        }
    }
    if !engine.holds_read_only() {
        return;
    }

    // Row 971, named for this scenario: CI runs four scenarios at a time
    // against one server.
    let canary = 971;
    let insert = sql_insert(engine, schema, canary, "session setting");
    let delete = sql_delete(engine, schema, canary);
    let postgres = engine == Engine::Postgres;
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: if postgres {
            vec![
                "RESET ROLE".to_string(),
                "RESET SESSION AUTHORIZATION".to_string(),
                delete.clone(),
            ]
        } else {
            vec![delete.clone()]
        },
        rollback: true,
        read_write: true,
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    let cell = |sql: &str| {
        run(sql)
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: `{sql}` answered nothing"))
            .to_string()
    };
    let refused = || {
        let error = subject
            .connection
            .query(&insert, &CancelToken::default(), Fetch::default())
            .expect_err(&format!("{at}: the hold took a write it should refuse"));
        assert!(
            error.message.contains(subject.read_only_refusal),
            "{at}: refused for another reason: {}",
            error.message
        );
    };
    // Back to a clean session: nothing open, the hold lifted, no canary row.
    let reset = || {
        run("ROLLBACK");
        hold(false);
        run(&delete);
    };
    // Rated above a read, and the write refused with the hold armed goes
    // through after it, with the hold still nominally on.
    let disarms = |case: &str, before: &[&str], statement: &str| {
        above_a_read(statement);
        hold(true);
        refused();
        for sql in before {
            run(sql);
        }
        subject.run(&format!("{scenario}: {case}"), statement, Fetch::default());
        subject.run(&format!("{scenario}: {case}"), &insert, Fetch::default());
        reset();
    };
    // Rated a read, and the same write is still refused afterwards.
    let leaves_the_hold = |case: &str, before: &[&str], statement: &str| {
        assert_eq!(
            rated(statement),
            sql::Mode::ReadOnly,
            "{at}: control `{statement}`"
        );
        hold(true);
        for sql in before {
            run(sql);
        }
        subject.run(&format!("{scenario}: {case}"), statement, Fetch::default());
        refused();
        reset();
    };

    let release = super::read_only_statement(engine, false)
        .unwrap_or_else(|| panic!("{at}: a holding engine has a statement that releases it"));
    disarms("the hold's own release", &[], release);

    if postgres {
        disarms("a write transaction", &[], "BEGIN READ WRITE");
        disarms(
            "a session write access mode",
            &[],
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        );
        disarms(
            "a transaction write access mode",
            &["BEGIN"],
            "SET TRANSACTION READ WRITE",
        );
        disarms(
            "the transaction read-only setting",
            &["BEGIN"],
            "SET transaction_read_only = off",
        );
        disarms(
            "the function form of the setting",
            &[],
            "SELECT set_config('default_transaction_read_only', 'off', false)",
        );

        // An identity change leaves the hold armed, so there is no canary to
        // ask: the session's own answer to who it is has to move instead.
        let other = cell(
            "SELECT rolname FROM pg_roles WHERE rolname <> current_user ORDER BY rolname LIMIT 1",
        );
        let other = engine.quote_identifier(&other);
        for (case, statement, undo) in [
            ("a role change", format!("SET ROLE {other}"), "RESET ROLE"),
            (
                "an authorization change",
                format!("SET SESSION AUTHORIZATION {other}"),
                "RESET SESSION AUTHORIZATION",
            ),
        ] {
            above_a_read(&statement);
            hold(true);
            let who = cell("SELECT current_user");
            subject.run(&format!("{scenario}: {case}"), &statement, Fetch::default());
            assert_ne!(
                cell("SELECT current_user"),
                who,
                "{at}: {case} did not change the session's user"
            );
            assert_eq!(
                cell("SELECT current_setting('default_transaction_read_only')"),
                "on",
                "{at}: {case} disarmed the hold, which it is rated for not doing"
            );
            run(undo);
            reset();
        }

        leaves_the_hold(
            "a search-path change",
            &[],
            &format!("SET search_path TO {schema}"),
        );
        leaves_the_hold(
            "a read access mode",
            &["BEGIN"],
            "SET TRANSACTION READ ONLY",
        );
    } else {
        disarms("a write transaction", &[], "START TRANSACTION READ WRITE");
        disarms(
            "a write access mode for the next transaction",
            &[],
            "SET TRANSACTION READ WRITE",
        );
        disarms(
            "the transaction read-only setting",
            &[],
            "SET SESSION transaction_read_only = 0",
        );
        disarms(
            "the transaction read-only setting, doubled-at",
            &[],
            "SET @@session.transaction_read_only = OFF",
        );
        if engine == Engine::MariaDb {
            disarms("its older spelling", &[], "SET tx_read_only = 0");
        } else {
            // Removed in 8.0.3, so the server refuses the name outright. It is
            // still rated above a read, and the hold is still armed.
            above_a_read("SET tx_read_only = 0");
            hold(true);
            let error = subject
                .connection
                .query(
                    "SET tx_read_only = 0",
                    &CancelToken::default(),
                    Fetch::default(),
                )
                .expect_err(&format!("{at}: the server accepted a removed variable"));
            assert!(
                error.message.contains("Unknown system variable"),
                "{at}: refused for another reason: {}",
                error.message
            );
            refused();
            reset();
        }

        leaves_the_hold("a comfort setting", &[], "SET SESSION wait_timeout = 28800");
        leaves_the_hold("a read access mode", &[], "SET TRANSACTION READ ONLY");
    }

    // The fixture is as it was found.
    assert_eq!(
        subject.read(scenario).rows.len(),
        EXPECTED.len(),
        "{at}: the fixture does not hold its original rows"
    );
}

/// The two servers of the MySQL dialect disagree about a MariaDB-only
/// executable comment (`/*M!100100 … */`): one runs what it holds, the other
/// reads a comment. The classifier cannot know which it is talking to, so it
/// rates what the executing one would run, and this scenario pins both halves:
/// the executing server really does the destructive thing and really does
/// disarm the hold, and the other really does neither.
///
/// Which one this is is asked of the server, not of the engine: `SELECT 2
/// /*M!100100 +1 */` answers 3 from one and 2 from the other. Every positive
/// has a control spelled as a plain comment that is rated a read and leaves the
/// same object, or the same refused write, untouched on both.
pub(super) fn executable_comments_agree_with_the_server(subject: &Subject) {
    let scenario = "executable_comments_agree_with_the_server";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let schema = subject.schema;
    // Row 972 and one helper table, both named for this scenario: CI runs four
    // scenarios at a time against one server. The shared fixture table is never
    // the target of a statement that drops anything.
    let canary = 972;
    let table = engine.qualified(schema, "dbdelve_comments_972");
    let create = format!("CREATE TABLE {table} (id INT)");
    let delete = sql_delete(engine, schema, canary);
    let insert = sql_insert(engine, schema, canary, "executable comment");
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: vec![delete.clone(), format!("DROP TABLE IF EXISTS {table}")],
        rollback: true,
        read_write: true,
    };
    let run = |sql: &str| subject.run(scenario, sql, Fetch::default());
    // The effect is what is asserted on, and a server that ignores a comment
    // answers a submission of nothing else with an error of its own.
    let send = |sql: &str| {
        let _ = subject
            .connection
            .query(sql, &CancelToken::default(), Fetch::default());
    };
    let hold = |on: bool| {
        subject
            .connection
            .set_read_only(on)
            .unwrap_or_else(|error| panic!("{at}: {error}"));
    };
    let cell = |sql: &str| {
        run(sql)
            .rows
            .first()
            .and_then(|row| row.get(0))
            .unwrap_or_else(|| panic!("{at}: `{sql}` answered nothing"))
            .to_string()
    };
    let table_exists = || {
        let found = cell(&format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = {} AND table_name = 'dbdelve_comments_972'",
            engine.quote_literal(schema)
        ));
        found == "1"
    };
    let refused = || {
        let error = subject
            .connection
            .query(&insert, &CancelToken::default(), Fetch::default())
            .expect_err(&format!("{at}: the hold took a write it should refuse"));
        assert!(
            error.message.contains(subject.read_only_refusal),
            "{at}: refused for another reason: {}",
            error.message
        );
    };
    let verdict = |sql: &str| sql::classify(engine, sql);
    let stopped = |sql: &str| {
        let rated = verdict(sql);
        assert!(
            rated.mode > sql::Mode::ReadOnly
                && !rated.destructive.contains(&sql::Destructive::Unreadable),
            "{at}: `{sql}` is rated {rated:?}, which a Read-only profile would run"
        );
    };
    let a_read = |sql: &str| {
        assert_eq!(
            verdict(sql),
            sql::Verdict::READ,
            "{at}: control `{sql}` is not rated a plain read"
        );
    };

    let executes = match cell("SELECT 2 /*M!100100 +1 */").as_str() {
        "3" => true,
        "2" => false,
        other => panic!("{at}: the probe answered {other}, which neither server gives"),
    };

    // The destructive form: rated by the kind it holds on both servers, and the
    // object is gone afterwards on the one that runs it and still there on the
    // other.
    let drop = format!("/*M!100100 DROP TABLE {table} */");
    assert_eq!(
        verdict(&drop).destructive,
        [sql::Destructive::Drop],
        "{at}: `{drop}`"
    );
    run(&create);
    send(&drop);
    assert_eq!(
        table_exists(),
        !executes,
        "{at}: the server that {} the marker {} the canary",
        if executes { "runs" } else { "ignores" },
        if executes { "left" } else { "dropped" }
    );
    if executes {
        run(&create);
    }
    // The control: four digits are no version, a plain comment to both.
    let plain = format!("/*M!1234 DROP TABLE {table} */");
    a_read(&plain);
    send(&plain);
    assert!(table_exists(), "{at}: a plain comment dropped the canary");
    run(&format!("DROP TABLE {table}"));

    // The hole this closes: with the hold armed, a comment holding the release
    // is a write the server has refused until now.
    let release = super::read_only_statement(engine, false)
        .unwrap_or_else(|| panic!("{at}: a holding engine has a statement that releases it"));
    let disarm = format!("/*M!100100 {release} */");
    stopped(&disarm);
    stopped(&format!("SELECT 1; {disarm}"));
    hold(true);
    refused();
    send(&disarm);
    if executes {
        // The write that was refused a moment ago now goes through.
        run(&insert);
    } else {
        refused();
    }
    run("ROLLBACK");
    hold(false);
    run(&delete);

    let plain = format!("/*M!1234 {release} */");
    a_read(&plain);
    hold(true);
    send(&plain);
    refused();
    run("ROLLBACK");
    hold(false);

    // The fixture is as it was found.
    assert_eq!(
        subject.read(scenario).rows.len(),
        EXPECTED.len(),
        "{at}: the fixture does not hold its original rows"
    );
}

pub(super) fn generated_writes_round_trip(subject: &Subject) {
    let scenario = "generated_writes_round_trip";
    let at = format!("{scenario} on {:?}", subject.engine);
    let engine = subject.engine;
    let (schema, relation, key) = (subject.schema, subject.relation, subject.key);
    let structure = subject
        .connection
        .structure(schema, relation)
        .unwrap_or_else(|error| panic!("{at}: {error}"));
    let types: Vec<(String, String)> = structure
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.data_type.clone()))
        .collect();
    let generated = |sql: &str| {
        subject
            .connection
            .generated(sql, &CancelToken::default(), None, None)
            .unwrap_or_else(|error| panic!("{at}: `{sql}` failed: {error}"))
    };

    let cleanup = sql::delete_row(engine, schema, relation, &[(key, "901")], &types);
    let _cleanup = Cleanup {
        connection: &subject.connection,
        statements: cleanup.iter().cloned().collect(),
        rollback: false,
        read_write: false,
    };
    if let Some(delete) = &cleanup {
        generated(delete);
    }

    if engine.takes_inserts(RelationKind::Table) {
        let insert = sql::insert_row(
            engine,
            schema,
            relation,
            &[(key, Some("901")), ("label", Some("inserted"))],
            &types,
        )
        .unwrap_or_else(|error| panic!("{at}: insert_row: {error}"));
        assert!(
            sql::is_generated_write_on(engine, &insert),
            "{at}: {insert}"
        );
        generated(&insert);
        assert_eq!(
            subject.label_of(scenario, "901"),
            [Some("inserted".to_string())],
            "{at}"
        );
    }

    let label_type = types
        .iter()
        .find(|(name, _)| name == "label")
        .map(|(_, data_type)| data_type.as_str())
        .unwrap_or_else(|| panic!("{at}: no label column"));
    if engine.writes_back([label_type]) && !structure.primary_key().is_empty() {
        let row = PendingRow {
            schema: schema.to_string(),
            table: relation.to_string(),
            sets: vec![("label".into(), NewValue::Value("updated".into()))],
            keys: vec![(key.to_string(), "901".to_string())],
            types: types.clone(),
            held: vec![("label".into(), "inserted".into())],
        };
        let update = sql::update_batch(engine, &[row])
            .unwrap_or_else(|error| panic!("{at}: update_batch: {error}"));
        assert!(
            sql::is_generated_write_on(engine, &update),
            "{at}: {update}"
        );
        generated(&update);
        assert_eq!(
            subject.label_of(scenario, "901"),
            [Some("updated".to_string())],
            "{at}"
        );
    }

    let delete = sql::delete_row(engine, schema, relation, &[(key, "901")], &types)
        .unwrap_or_else(|| panic!("{at}: no delete for the key"));
    assert!(
        sql::is_generated_write_on(engine, &delete),
        "{at}: {delete}"
    );
    assert!(
        sql::delete_matches_key_on(engine, &delete, &[key]),
        "{at}: {delete}"
    );
    generated(&delete);
    assert!(subject.label_of(scenario, "901").is_empty(), "{at}");
}
