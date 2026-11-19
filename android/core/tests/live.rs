//! Against real servers. `#[ignore]`d; each reads `DBDELVE_ANDROID_<ENGINE>`
//! as `host:port:user:password:database` and skips when it is unset.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dbdelve_core::{
    ConnectionConfig, CoreError, Destructive, Engine, Mode, ObjectKind, RunOutcome, Session,
    SslMode, Stop,
};

fn config(engine: Engine, variable: &str, ssl_mode: SslMode) -> Option<ConnectionConfig> {
    let value = std::env::var(variable).ok()?;
    let parts: Vec<&str> = value.splitn(5, ':').collect();
    let [host, port, user, password, database] = parts[..] else {
        panic!("{variable} is host:port:user:password:database");
    };
    Some(ConnectionConfig {
        engine,
        host: host.into(),
        port: port.parse().unwrap(),
        user: user.into(),
        password: password.into(),
        database: database.into(),
        ssl_mode,
    })
}

fn run(session: &Session, text: &str) -> RunOutcome {
    session
        .run(text.into(), 2, Vec::new(), false, None)
        .unwrap()
}

fn done(outcome: RunOutcome) -> dbdelve_core::ResultSet {
    match outcome {
        RunOutcome::Done { result } => result,
        RunOutcome::Stopped { stop, .. } => panic!("stopped: {stop:?}"),
    }
}

/// Read-only refuses a write before sending it; Read-write runs it; the row
/// limit keeps two rows and says it capped; a drop asks first.
fn exercise(session: &Session, create: &str, insert: &str, select: &str, drop: &str) {
    assert!(matches!(
        run(session, create),
        RunOutcome::Stopped {
            stop: Stop::Upgrade {
                mode: Mode::ReadWrite
            },
            ..
        }
    ));
    session.set_mode(Mode::ReadWrite).unwrap();
    // Left over from a run that failed half-way.
    let _ = session.run(
        drop.into(),
        2,
        vec![Destructive::Drop],
        false,
        Some(Mode::Full),
    );
    done(run(session, create));
    done(run(session, insert));
    let result = done(run(session, select));
    assert_eq!(result.rows.len(), 2);
    assert!(result.capped, "three rows past a limit of two");
    assert!(matches!(
        run(session, drop),
        RunOutcome::Stopped {
            stop: Stop::Upgrade { mode: Mode::Full },
            ..
        }
    ));
    let confirmed = session
        .run(drop.into(), 2, Vec::new(), false, Some(Mode::Full))
        .unwrap();
    assert!(matches!(
        confirmed,
        RunOutcome::Stopped {
            stop: Stop::Confirm {
                kind: Destructive::Drop
            },
            ..
        }
    ));
    done(
        session
            .run(
                drop.into(),
                2,
                vec![Destructive::Drop],
                false,
                Some(Mode::Full),
            )
            .unwrap(),
    );
    assert_eq!(
        session.mode(),
        Mode::ReadWrite,
        "a run once leaves the mode where it was"
    );
}

/// The server's own hold, behind the gate: a write the parser cannot read,
/// run once in Read-only, is refused by the server rather than run.
fn held_by_server(session: &Session, create: &str, unreadable_write: &str) {
    session.set_mode(Mode::ReadWrite).unwrap();
    let drop = "DROP TABLE android_held";
    let _ = session.run(
        drop.into(),
        2,
        vec![Destructive::Drop],
        false,
        Some(Mode::Full),
    );
    done(run(session, create));
    session.set_mode(Mode::ReadOnly).unwrap();
    let verdict = dbdelve_core::classify(session.engine(), unreadable_write.into());
    assert_eq!(
        verdict.destructive,
        vec![Destructive::Unreadable],
        "the gate cannot read it"
    );
    let held = session.run(unreadable_write.into(), 2, vec![], true, None);
    match held {
        Err(CoreError::Query { reason: message }) => {
            let message = message.to_ascii_lowercase();
            assert!(
                message.contains("read-only") || message.contains("read only"),
                "{message}"
            );
        }
        other => panic!("ran in Read-only: {other:?}"),
    }
    done(
        session
            .run(
                drop.into(),
                2,
                vec![Destructive::Drop],
                false,
                Some(Mode::Full),
            )
            .unwrap(),
    );
}

#[test]
#[ignore]
fn live_postgres() {
    let Some(config) = config(
        Engine::Postgres,
        "DBDELVE_ANDROID_POSTGRES",
        SslMode::Disable,
    ) else {
        return;
    };
    let session = Session::connect(config, Mode::ReadOnly).unwrap();
    exercise(
        &session,
        "CREATE TABLE android_live (id int PRIMARY KEY, label text)",
        "INSERT INTO android_live VALUES (1, 'a'), (2, NULL), (3, 'é')",
        "SELECT * FROM android_live ORDER BY id",
        "DROP TABLE android_live",
    );

    held_by_server(
        &session,
        "CREATE TABLE android_held (a int)",
        "INSERT INTO android_held OVERRIDING SYSTEM VALUE VALUES (1)",
    );

    let session = Arc::new(session);
    let cancelling = Arc::clone(&session);
    let started = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        cancelling.cancel();
    });
    let slept = session.run("SELECT pg_sleep(30)".into(), 10, vec![], false, None);
    assert!(matches!(slept, Err(CoreError::Query { .. })), "{slept:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
#[ignore]
fn live_mysql() {
    let Some(config) = config(Engine::MySql, "DBDELVE_ANDROID_MYSQL", SslMode::Disable) else {
        return;
    };
    let session = Session::connect(config.clone(), Mode::ReadOnly).unwrap();
    exercise(
        &session,
        "CREATE TABLE android_live (id int PRIMARY KEY, label varchar(10))",
        "INSERT INTO android_live VALUES (1, 'a'), (2, NULL), (3, 'é')",
        "SELECT * FROM android_live ORDER BY id",
        "DROP TABLE android_live",
    );
    let result = done(run(&session, "SELECT 1 AS one, NULL AS nothing"));
    assert_eq!(result.rows, vec![vec![Some("1".into()), None]]);
    assert_eq!(result.columns[0].type_tag, "LONGLONG");

    held_by_server(
        &session,
        "CREATE TABLE android_held (a int)",
        "UPDATE LOW_PRIORITY IGNORE android_held SET a = 3 ORDER BY a LIMIT 1",
    );

    let session = Arc::new(session);
    let cancelling = Arc::clone(&session);
    let started = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        cancelling.cancel();
    });
    let _ = session.run("SELECT SLEEP(30)".into(), 10, vec![], false, None);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "KILL QUERY stopped the sleep"
    );

    let tables = session.catalog().unwrap();
    assert!(
        tables
            .iter()
            .all(|entry| entry.kind != ObjectKind::Collection)
    );
}

#[test]
#[ignore]
fn live_mysql_tls() {
    let Some(config) = config(Engine::MySql, "DBDELVE_ANDROID_MYSQL", SslMode::VerifyFull) else {
        return;
    };
    // A dev server's self-signed certificate, or a 5.6 that offers only TLS
    // 1.0: either way refused, and never retried in plaintext.
    let refused = Session::connect(config, Mode::ReadOnly)
        .err()
        .expect("refused");
    println!("{refused}");
}

#[test]
#[ignore]
fn live_mongo() {
    let Some(config) = config(Engine::Mongo, "DBDELVE_ANDROID_MONGO", SslMode::Disable) else {
        return;
    };
    let session = Session::connect(config, Mode::ReadOnly).unwrap();
    exercise(
        &session,
        "db.android_live.insertOne({_id: 0, label: 'seed'})",
        "db.android_live.insertMany([{_id: 1, label: 'a'}, {_id: 2, label: null}, {_id: 3}])",
        "db.android_live.find({_id: {$gt: 0}})",
        "db.android_live.drop()",
    );
    assert!(matches!(
        run(&session, "db.android_live.deleteMany({})"),
        RunOutcome::Stopped {
            stop: Stop::Upgrade { mode: Mode::Full },
            ..
        }
    ));
    let preview = session.preview("dev".into(), "odd.name".into(), 10);
    assert_eq!(preview, r#"db.getCollection("odd.name").find({})"#);
}
