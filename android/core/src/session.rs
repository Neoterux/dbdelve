//! One open connection, and the only way a statement reaches it.
//!
//! `Session::run` classifies and gates before anything goes out, so no
//! caller can send a statement the gate has not read. Each driver blocks on
//! a runtime of its own, from the caller's thread: nothing that leaves this
//! module is a future.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bson::{Bson, Document};
use futures_util::{StreamExt, TryStreamExt, pin_mut};
use mysql::prelude::Queryable;
use tokio::runtime::Runtime;
use tokio_postgres::{CancelToken, SimpleQueryMessage, SimpleQueryRow};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::gate::{self, Destructive, Mode, Stop, Verdict};
use crate::mongo_cmd::{self, Call};
use crate::users::{self, Account, DbUser, Prepared, RoleRef, UserAction};
use crate::{ConnectionConfig, CoreError, Engine, SslMode};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, uniffi::Record)]
pub struct Column {
    pub name: String,
    /// Empty where the protocol does not say.
    pub type_tag: String,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows arrived than the limit kept.
    pub capped: bool,
    pub affected: Option<u64>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum RunOutcome {
    Done {
        result: ResultSet,
    },
    /// Nothing was sent. The UI asks, then calls `run` again with the answer.
    Stopped {
        stop: Stop,
        verdict: Verdict,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ObjectKind {
    Table,
    View,
    Collection,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct CatalogEntry {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    pub size_bytes: Option<u64>,
}

enum Driver {
    Postgres {
        runtime: Runtime,
        client: tokio_postgres::Client,
    },
    MySql(mysql::Conn),
    Mongo {
        runtime: Runtime,
        database: mongodb::Database,
    },
}

enum Canceller {
    Postgres(Box<PostgresStop>),
    MySql {
        opts: mysql::Opts,
        id: u32,
    },
    /// A MongoDB read stops at the row limit by dropping its cursor; a write
    /// runs to completion.
    Mongo,
}

#[derive(uniffi::Object)]
pub struct Session {
    engine: Engine,
    driver: Mutex<Driver>,
    mode: Mutex<Mode>,
    canceller: Canceller,
}

fn connect_error(message: impl ToString) -> CoreError {
    CoreError::Connect {
        reason: message.to_string(),
    }
}

fn query_error(message: impl ToString) -> CoreError {
    CoreError::Query {
        reason: message.to_string(),
    }
}

fn runtime() -> Result<Runtime, CoreError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(connect_error)
}

fn tls_config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// A TLS handshake MySQL 5.6 fails, named, instead of a bare alert: builds
/// linked against yaSSL offer only TLS 1.0 and 1.1, which rustls refuses.
fn mysql_tls_hint(error: mysql::Error, ssl: SslMode) -> CoreError {
    let message = error.to_string();
    let old_tls = [
        "ProtocolVersion",
        "protocol version",
        "PeerIncompatible",
        "HandshakeFailure",
    ]
    .iter()
    .any(|needle| message.contains(needle));
    if ssl == SslMode::VerifyFull && old_tls {
        return connect_error(format!(
            "{message}\n\nThe server offered no TLS version this app accepts (1.2 or later). \
             MySQL 5.6 built with yaSSL offers only TLS 1.0 and 1.1. Enable TLS 1.2 on the \
             server, or reach it through a VPN or SSH tunnel with TLS off."
        ));
    }
    connect_error(message)
}

fn postgres_error(error: tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("{} {}: {}", db.severity(), db.code().code(), db.message()),
        None => error.to_string(),
    }
}

#[uniffi::export]
impl Session {
    #[uniffi::constructor]
    pub fn connect(config: ConnectionConfig, mode: Mode) -> Result<Arc<Self>, CoreError> {
        let (driver, canceller) = match config.engine {
            Engine::Postgres => connect_postgres(&config)?,
            Engine::MySql => connect_mysql(&config)?,
            Engine::Mongo => connect_mongo(&config)?,
        };
        let session = Session {
            engine: config.engine,
            driver: Mutex::new(driver),
            mode: Mutex::new(mode),
            canceller,
        };
        session.hold(mode)?;
        Ok(Arc::new(session))
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The only place a session's mode changes. The server's hold follows it,
    /// and the mode is not changed if the server will not.
    pub fn set_mode(&self, mode: Mode) -> Result<(), CoreError> {
        self.hold(mode)?;
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
        Ok(())
    }

    /// Classify, gate, and only then send one statement.
    ///
    /// `confirmed` are the destructive kinds the user just agreed to;
    /// `run_once` answers an unreadable statement's prompt; `once_in` lets
    /// this one run in a higher mode, after which the session's own mode and
    /// hold are back in place whatever happened.
    pub fn run(
        &self,
        statement: String,
        row_limit: u32,
        confirmed: Vec<Destructive>,
        run_once: bool,
        once_in: Option<Mode>,
    ) -> Result<RunOutcome, CoreError> {
        let verdict = gate::classify(self.engine, &statement);
        self.gated(
            &statement, verdict, row_limit, &confirmed, run_once, once_in,
        )
    }

    /// A user-management statement built by [`Session::prepare_user`],
    /// through the same gate as any other, on the verdict it was built with.
    pub fn run_prepared(
        &self,
        prepared: Arc<Prepared>,
        confirmed: Vec<Destructive>,
        once_in: Option<Mode>,
    ) -> Result<RunOutcome, CoreError> {
        self.gated(
            &prepared.text,
            prepared.verdict.clone(),
            1_000,
            &confirmed,
            false,
            once_in,
        )
    }

    pub fn prepare_user(&self, action: UserAction) -> Result<Arc<Prepared>, CoreError> {
        let database = match &*self.driver.lock().unwrap_or_else(PoisonError::into_inner) {
            Driver::Mongo { database, .. } => database.name().to_owned(),
            _ => String::new(),
        };
        users::prepare(self.engine, &database, &action).map(Arc::new)
    }

    /// The server's users and roles. DBDelve's own catalog read, like
    /// [`Session::catalog`].
    pub fn users(&self) -> Result<Vec<DbUser>, CoreError> {
        let mut driver = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *driver {
            Driver::Postgres { runtime, client } => {
                let rows = runtime
                    .block_on(client.simple_query(
                        "SELECT r.rolname, r.rolcanlogin::text, r.rolsuper::text, \
                         r.rolcreatedb::text, r.rolcreaterole::text, r.rolreplication::text, \
                         r.rolbypassrls::text, r.rolconnlimit::text, \
                         COALESCE(r.rolvaliduntil::text, ''), \
                         COALESCE((SELECT array_to_json(array_agg(g.rolname ORDER BY g.rolname)) \
                         FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid \
                         WHERE m.member = r.oid)::text, '[]') \
                         FROM pg_roles r WHERE r.rolname !~ '^pg_' ORDER BY 1",
                    ))
                    .map_err(|error| query_error(postgres_error(error)))?;
                Ok(rows
                    .iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::Row(row) => Some(row),
                        _ => None,
                    })
                    .map(|row| {
                        // `::text` on a boolean spells it out: `true`, not the bare `t`.
                        let flag = |at: usize| text(row, at) == Some("true");
                        let mut attributes = Vec::new();
                        for (at, name) in [
                            (3, "create db"),
                            (4, "create role"),
                            (5, "replication"),
                            (6, "bypass RLS"),
                        ] {
                            if flag(at) {
                                attributes.push(name.to_owned());
                            }
                        }
                        if let Some(limit) = text(row, 7).filter(|limit| *limit != "-1") {
                            attributes.push(format!("{limit} connections"));
                        }
                        if let Some(until) = text(row, 8).filter(|until| !until.is_empty()) {
                            attributes.push(format!("valid until {until}"));
                        }
                        DbUser {
                            account: Account {
                                name: text(row, 0).unwrap_or_default().to_owned(),
                                host: String::new(),
                            },
                            can_login: flag(1),
                            superuser: flag(2),
                            attributes,
                            roles: serde_json::from_str::<Vec<String>>(
                                text(row, 9).unwrap_or("[]"),
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .map(|role| RoleRef {
                                role,
                                database: String::new(),
                            })
                            .collect(),
                        }
                    })
                    .collect())
            }
            Driver::MySql(conn) => {
                // `password_expired` arrived in 5.6.6.
                let rows: Vec<(String, String, String, String)> = conn
                    .query(
                        "SELECT User, Host, Super_priv, password_expired FROM mysql.user \
                         ORDER BY User, Host",
                    )
                    .or_else(|_| {
                        conn.query("SELECT User, Host, Super_priv, 'N' FROM mysql.user ORDER BY User, Host")
                    })
                    .map_err(query_error)?;
                Ok(rows
                    .into_iter()
                    .map(|(name, host, superuser, expired)| DbUser {
                        account: Account { name, host },
                        can_login: true,
                        superuser: superuser == "Y",
                        attributes: if expired == "Y" {
                            vec!["password expired".to_owned()]
                        } else {
                            Vec::new()
                        },
                        roles: Vec::new(),
                    })
                    .collect())
            }
            Driver::Mongo { runtime, database } => {
                let reply = runtime
                    .block_on(async { database.run_command(bson::doc! { "usersInfo": 1 }).await })
                    .map_err(query_error)?;
                let users = reply.get_array("users").map_err(query_error)?;
                Ok(users
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|user| DbUser {
                        account: Account {
                            name: user.get_str("user").unwrap_or_default().to_owned(),
                            host: String::new(),
                        },
                        can_login: true,
                        superuser: false,
                        attributes: Vec::new(),
                        roles: mongo_roles(user.get_array("roles").ok()),
                    })
                    .collect())
            }
        }
    }

    /// What one account may do, a line per object.
    pub fn user_grants(&self, account: Account) -> Result<Vec<String>, CoreError> {
        if account.name.contains('\\') || account.host.contains('\\') {
            return Err(query_error("Names with a backslash are not looked up."));
        }
        let mut driver = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *driver {
            Driver::Postgres { runtime, client } => {
                let rows = runtime
                    .block_on(client.simple_query(&format!(
                        "SELECT table_schema || '.' || table_name || ': ' || \
                         string_agg(privilege_type, ', ' ORDER BY privilege_type) \
                         FROM information_schema.table_privileges WHERE grantee = '{}' \
                         GROUP BY table_schema, table_name ORDER BY 1",
                        account.name.replace('\'', "''")
                    )))
                    .map_err(|error| query_error(postgres_error(error)))?;
                Ok(rows
                    .iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::Row(row) => text(row, 0).map(str::to_owned),
                        _ => None,
                    })
                    .collect())
            }
            Driver::MySql(conn) => {
                let host = if account.host.is_empty() {
                    "%"
                } else {
                    &account.host
                };
                let grants: Vec<String> = conn
                    .query(format!(
                        "SHOW GRANTS FOR '{}'@'{}'",
                        account.name.replace('\'', "''"),
                        host.replace('\'', "''")
                    ))
                    .map_err(query_error)?;
                Ok(grants
                    .into_iter()
                    .map(|grant| without_hash(&grant))
                    .collect())
            }
            Driver::Mongo { runtime, database } => {
                let reply = runtime
                    .block_on(async {
                        database
                            .run_command(bson::doc! {
                                "usersInfo": { "user": account.name.clone(), "db": database.name() },
                                "showPrivileges": true,
                            })
                            .await
                    })
                    .map_err(query_error)?;
                let users = reply.get_array("users").map_err(query_error)?;
                let Some(user) = users.first().and_then(Bson::as_document) else {
                    return Ok(Vec::new());
                };
                Ok(user
                    .get_array("inheritedPrivileges")
                    .map(|privileges| {
                        privileges
                            .iter()
                            .filter_map(Bson::as_document)
                            .map(|privilege| {
                                let resource = privilege.get_document("resource").ok();
                                let on = resource.map_or("cluster".to_owned(), |resource| {
                                    match (resource.get_str("db"), resource.get_str("collection")) {
                                        (Ok(""), _) => "every database".to_owned(),
                                        (Ok(db), Ok("")) => db.to_owned(),
                                        (Ok(db), Ok(collection)) => format!("{db}.{collection}"),
                                        _ => "cluster".to_owned(),
                                    }
                                });
                                let actions = privilege
                                    .get_array("actions")
                                    .map(|actions| {
                                        actions
                                            .iter()
                                            .filter_map(Bson::as_str)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .unwrap_or_default();
                                format!("{on}: {actions}")
                            })
                            .collect()
                    })
                    .unwrap_or_default())
            }
        }
    }

    /// Stops the statement running now, from any thread.
    pub fn cancel(&self) {
        match &self.canceller {
            Canceller::Postgres(stop) => {
                let PostgresStop { token, tls, .. } = &**stop;
                let Ok(runtime) = runtime() else { return };
                // Best effort: a cancel that cannot reach the server leaves the
                // statement to finish, which is what not cancelling does.
                let _ = runtime.block_on(async {
                    match tls {
                        Some(tls) => token.cancel_query(tls.clone()).await,
                        None => token.cancel_query(tokio_postgres::NoTls).await,
                    }
                });
            }
            Canceller::MySql { opts, id } => {
                // On a side connection: the session's own is busy running it.
                if let Ok(mut side) = mysql::Conn::new(opts.clone()) {
                    let _ = side.query_drop(format!("KILL QUERY {id}"));
                }
            }
            Canceller::Mongo => {}
        }
    }

    pub fn catalog(&self) -> Result<Vec<CatalogEntry>, CoreError> {
        let mut driver = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *driver {
            Driver::Postgres { runtime, client } => {
                let rows = runtime
                    .block_on(client.simple_query(
                        "SELECT n.nspname, c.relname, c.relkind::text, \
                         pg_total_relation_size(c.oid)::text \
                         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE c.relkind IN ('r', 'p', 'v', 'm') \
                         AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                         AND n.nspname NOT LIKE 'pg\\_toast%' \
                         ORDER BY 1, 2",
                    ))
                    .map_err(|error| query_error(postgres_error(error)))?;
                Ok(rows
                    .iter()
                    .filter_map(|message| match message {
                        SimpleQueryMessage::Row(row) => Some(CatalogEntry {
                            schema: text(row, 0).unwrap_or_default().to_owned(),
                            name: text(row, 1).unwrap_or_default().to_owned(),
                            kind: if matches!(text(row, 2), Some("v" | "m")) {
                                ObjectKind::View
                            } else {
                                ObjectKind::Table
                            },
                            size_bytes: text(row, 3).and_then(|size| size.parse().ok()),
                        }),
                        _ => None,
                    })
                    .collect())
            }
            Driver::MySql(conn) => {
                let rows: Vec<(String, String, String, Option<u64>)> = conn
                    .query(
                        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, DATA_LENGTH + INDEX_LENGTH \
                         FROM information_schema.TABLES \
                         WHERE TABLE_SCHEMA NOT IN \
                         ('mysql', 'information_schema', 'performance_schema', 'sys') \
                         ORDER BY 1, 2",
                    )
                    .map_err(query_error)?;
                Ok(rows
                    .into_iter()
                    .map(|(schema, name, kind, size_bytes)| CatalogEntry {
                        schema,
                        name,
                        kind: if kind == "VIEW" {
                            ObjectKind::View
                        } else {
                            ObjectKind::Table
                        },
                        size_bytes,
                    })
                    .collect())
            }
            Driver::Mongo { runtime, database } => {
                let mut names = runtime
                    .block_on(async { database.list_collection_names().await })
                    .map_err(query_error)?;
                names.sort();
                Ok(names
                    .into_iter()
                    .map(|name| CatalogEntry {
                        schema: database.name().to_owned(),
                        name,
                        kind: ObjectKind::Collection,
                        size_bytes: None,
                    })
                    .collect())
            }
        }
    }

    /// DBDelve's own preview of an object: the one place it writes a `LIMIT`,
    /// returned as text so the UI shows what it runs.
    pub fn preview(&self, schema: String, name: String, limit: u32) -> String {
        match self.engine {
            Engine::Postgres => format!(
                "SELECT * FROM {}.{} LIMIT {limit}",
                quote(&schema, '"'),
                quote(&name, '"')
            ),
            Engine::MySql => format!(
                "SELECT * FROM {}.{} LIMIT {limit}",
                quote(&schema, '`'),
                quote(&name, '`')
            ),
            // The row limit drops the cursor; no chained `.limit()` to write.
            Engine::Mongo => format!("{}.find({{}})", mongo_cmd::collection_ref(&name)),
        }
    }
}

fn quote(name: &str, mark: char) -> String {
    let doubled = name.replace(mark, &format!("{mark}{mark}"));
    format!("{mark}{doubled}{mark}")
}

impl Session {
    fn gated(
        &self,
        statement: &str,
        verdict: Verdict,
        row_limit: u32,
        confirmed: &[Destructive],
        run_once: bool,
        once_in: Option<Mode>,
    ) -> Result<RunOutcome, CoreError> {
        let mode = self.mode();
        let allowed = once_in.map_or(mode, |once| once.max(mode));
        match gate::gate(&verdict, allowed, confirmed) {
            Some(Stop::RunOnce) if run_once => {}
            Some(stop) => return Ok(RunOutcome::Stopped { stop, verdict }),
            None => {}
        }
        let lifted = allowed != mode;
        if lifted {
            self.hold(allowed)?;
        }
        let result = self.send(statement, row_limit as usize);
        if lifted {
            self.hold(mode)?;
        }
        result.map(|result| RunOutcome::Done { result })
    }

    /// Puts the server's own read-only hold where `mode` says. A backstop
    /// behind the gate, not the boundary: a write-less role is.
    fn hold(&self, mode: Mode) -> Result<(), CoreError> {
        let read_only = mode == Mode::ReadOnly;
        let mut driver = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *driver {
            Driver::Postgres { runtime, client } => runtime
                .block_on(client.batch_execute(if read_only {
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
                } else {
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
                }))
                .map_err(|error| connect_error(postgres_error(error))),
            Driver::MySql(conn) => conn
                .query_drop(if read_only {
                    "SET SESSION TRANSACTION READ ONLY"
                } else {
                    "SET SESSION TRANSACTION READ WRITE"
                })
                .map_err(|error| {
                    connect_error(format!(
                        "{error}\n\nThe server cannot hold this session to reads; \
                         MySQL can from 5.6.5 on."
                    ))
                }),
            Driver::Mongo { .. } => Ok(()),
        }
    }

    fn send(&self, statement: &str, limit: usize) -> Result<ResultSet, CoreError> {
        let started = Instant::now();
        let stoppable = gate::stoppable(self.engine, statement);
        let mut driver = self.driver.lock().unwrap_or_else(PoisonError::into_inner);
        let mut result = match (&mut *driver, &self.canceller) {
            (Driver::Postgres { runtime, client }, Canceller::Postgres(stop)) => {
                runtime.block_on(run_postgres(
                    client,
                    statement,
                    limit,
                    stoppable.then(|| (**stop).clone()),
                ))?
            }
            (Driver::MySql(conn), Canceller::MySql { opts, id }) => {
                run_mysql(conn, statement, limit, stoppable.then_some((opts, *id)))?
            }
            (Driver::Mongo { runtime, database }, _) => {
                runtime.block_on(run_mongo(database, statement, limit))?
            }
            _ => unreachable!("a session's driver and canceller are made together"),
        };
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }
}

/// A Postgres value as text. `SimpleQueryRow::get` panics on bytes that are
/// not UTF-8, which a `SQL_ASCII` database can hold; those are marked instead.
fn text(row: &SimpleQueryRow, at: usize) -> Option<&str> {
    row.try_get(at).unwrap_or(Some("\u{FFFD} (not UTF-8)"))
}

fn empty_result() -> ResultSet {
    ResultSet {
        columns: Vec::new(),
        rows: Vec::new(),
        capped: false,
        affected: None,
        elapsed_ms: 0,
    }
}

fn connect_postgres(config: &ConnectionConfig) -> Result<(Driver, Canceller), CoreError> {
    let runtime = runtime()?;
    let mut pg = tokio_postgres::Config::new();
    pg.host(&config.host)
        .port(config.port)
        .user(&config.user)
        .password(&config.password)
        .application_name("DBDelve Android")
        .connect_timeout(CONNECT_TIMEOUT);
    if !config.database.is_empty() {
        pg.dbname(&config.database);
    }
    let (client, tls) = match config.ssl_mode {
        SslMode::Disable => {
            pg.ssl_mode(tokio_postgres::config::SslMode::Disable);
            let (client, connection) = runtime
                .block_on(pg.connect(tokio_postgres::NoTls))
                .map_err(|error| connect_error(postgres_error(error)))?;
            runtime.spawn(connection);
            (client, None)
        }
        SslMode::VerifyFull => {
            // `Require` with a verifying connector: never `prefer`, which
            // falls back to plaintext without saying so.
            pg.ssl_mode(tokio_postgres::config::SslMode::Require);
            let tls = MakeRustlsConnect::new(tls_config());
            let (client, connection) = runtime
                .block_on(pg.connect(tls.clone()))
                .map_err(|error| connect_error(postgres_error(error)))?;
            runtime.spawn(connection);
            (client, Some(tls))
        }
    };
    let pid = runtime
        .block_on(client.simple_query("SELECT pg_backend_pid()"))
        .map_err(|error| connect_error(postgres_error(error)))?
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => text(row, 0).and_then(|pid| pid.parse().ok()),
            _ => None,
        })
        .ok_or_else(|| {
            connect_error("The server did not say which process serves this session.")
        })?;
    let stop = PostgresStop {
        token: client.cancel_token(),
        tls,
        config: pg,
        pid,
    };
    Ok((
        Driver::Postgres { runtime, client },
        Canceller::Postgres(Box::new(stop)),
    ))
}

/// What stopping a Postgres read at the row limit needs: a side session to
/// ask the server what this one is doing, and the token to cancel it.
#[derive(Clone)]
struct PostgresStop {
    token: CancelToken,
    tls: Option<MakeRustlsConnect>,
    config: tokio_postgres::Config,
    pid: i32,
}

impl PostgresStop {
    /// Cancels `statement` only if a side session sees this session's
    /// process running exactly it as a transaction of its own: a cancel
    /// inside a transaction the user opened would abort all of it. Anything
    /// it cannot confirm leaves the read to drain, which is only slower.
    ///
    /// `finished` is set once the read has ended; a later run of the same
    /// text must never be the one cancelled.
    async fn stop_if_alone(
        self,
        statement: String,
        requested: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    ) {
        let side = match &self.tls {
            Some(tls) => self
                .config
                .connect(tls.clone())
                .await
                .map(|(client, connection)| {
                    tokio::spawn(connection);
                    client
                }),
            None => self
                .config
                .connect(tokio_postgres::NoTls)
                .await
                .map(|(client, connection)| {
                    tokio::spawn(connection);
                    client
                }),
        };
        let Ok(side) = side else { return };
        let alone = side
            .query_opt(
                "SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active' \
                 AND xact_start = query_start AND query = $2",
                &[&self.pid, &statement],
            )
            .await;
        if matches!(alone, Ok(Some(_))) && !finished.load(Ordering::SeqCst) {
            requested.store(true, Ordering::SeqCst);
            let _ = match &self.tls {
                Some(tls) => self.token.cancel_query(tls.clone()).await,
                None => self.token.cancel_query(tokio_postgres::NoTls).await,
            };
        }
    }
}

/// Set when a read ends, however it ends.
struct Finished(Arc<AtomicBool>);

impl Drop for Finished {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Runs one statement and keeps `limit` rows. Past the limit a stoppable
/// read is cancelled when [`PostgresStop`] can prove that undoes nothing,
/// and drained otherwise.
async fn run_postgres(
    client: &tokio_postgres::Client,
    statement: &str,
    limit: usize,
    stop: Option<PostgresStop>,
) -> Result<ResultSet, CoreError> {
    let stream = client
        .simple_query_raw(statement)
        .await
        .map_err(|error| query_error(postgres_error(error)))?;
    pin_mut!(stream);
    let mut result = empty_result();
    let mut stop = stop;
    let requested = Arc::new(AtomicBool::new(false));
    let finished = Finished(Arc::new(AtomicBool::new(false)));
    loop {
        let message = match stream.try_next().await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            // The cancel asked for at the limit: the rows kept are the result.
            Err(error)
                if requested.load(Ordering::SeqCst)
                    && error.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) =>
            {
                result.capped = true;
                break;
            }
            Err(error) => return Err(query_error(postgres_error(error))),
        };
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                // The simple protocol names columns and sends every value as
                // text; it does not say their types.
                result.columns = columns
                    .iter()
                    .map(|column| Column {
                        name: column.name().to_owned(),
                        type_tag: String::new(),
                    })
                    .collect();
                result.rows.clear();
            }
            SimpleQueryMessage::Row(row) => {
                if result.rows.len() < limit {
                    result.rows.push(
                        (0..row.len())
                            .map(|at| text(&row, at).map(str::to_owned))
                            .collect(),
                    );
                } else {
                    result.capped = true;
                    if let Some(stop) = stop.take() {
                        // A thread of its own: rows already buffered never
                        // yield this runtime, so a task here would wait for
                        // the very drain it is meant to cut short.
                        let (statement, requested, finished) =
                            (statement.to_owned(), requested.clone(), finished.0.clone());
                        std::thread::spawn(move || {
                            if let Ok(runtime) = runtime() {
                                runtime
                                    .block_on(stop.stop_if_alone(statement, requested, finished));
                            }
                        });
                    }
                }
            }
            SimpleQueryMessage::CommandComplete(count) => result.affected = Some(count),
            _ => {}
        }
    }
    Ok(result)
}

fn connect_mysql(config: &ConnectionConfig) -> Result<(Driver, Canceller), CoreError> {
    let builder = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(config.host.clone()))
        .tcp_port(config.port)
        .user(Some(config.user.clone()))
        .pass(Some(config.password.clone()))
        .db_name((!config.database.is_empty()).then(|| config.database.clone()))
        .tcp_connect_timeout(Some(CONNECT_TIMEOUT))
        .ssl_opts(match config.ssl_mode {
            SslMode::Disable => None,
            SslMode::VerifyFull => Some(mysql::SslOpts::default()),
        });
    let opts = mysql::Opts::from(builder);
    let conn =
        mysql::Conn::new(opts.clone()).map_err(|error| mysql_tls_hint(error, config.ssl_mode))?;
    let id = conn.connection_id();
    Ok((Driver::MySql(conn), Canceller::MySql { opts, id }))
}

/// Runs one statement and keeps `limit` rows. Past the limit a stoppable
/// read is ended with `KILL QUERY` from a side session, which stops the
/// statement and never a transaction; otherwise it is drained.
fn run_mysql(
    conn: &mut mysql::Conn,
    statement: &str,
    limit: usize,
    stop: Option<(&mysql::Opts, u32)>,
) -> Result<ResultSet, CoreError> {
    let mut rows = conn.query_iter(statement).map_err(query_error)?;
    let mut result = empty_result();
    // The last result set is the one shown, as a procedure call's is.
    while let Some(set) = rows.iter() {
        result.columns = set
            .columns()
            .as_ref()
            .iter()
            .map(|column| Column {
                name: column.name_str().into_owned(),
                type_tag: format!("{:?}", column.column_type())
                    .trim_start_matches("MYSQL_TYPE_")
                    .to_owned(),
            })
            .collect();
        result.rows.clear();
        result.capped = false;
        let mut killed = false;
        for row in set {
            let row = match row {
                Ok(row) => row,
                // ER_QUERY_INTERRUPTED: the kill asked for at the limit.
                Err(mysql::Error::MySqlError(error)) if killed && error.code == 1317 => break,
                Err(error) => return Err(query_error(error)),
            };
            if result.rows.len() < limit {
                result
                    .rows
                    .push(row.unwrap().into_iter().map(mysql_text).collect());
            } else {
                result.capped = true;
                if let (Some((opts, id)), false) = (stop, killed) {
                    // A side session: this one is busy reading. If it cannot
                    // connect, the rest is drained, which is only slower.
                    if let Ok(mut side) = mysql::Conn::new(opts.clone()) {
                        killed = side.query_drop(format!("KILL QUERY {id}")).is_ok();
                    }
                }
            }
        }
        if killed {
            return Ok(result);
        }
    }
    result.affected = Some(rows.affected_rows());
    Ok(result)
}

fn mysql_text(value: mysql::Value) -> Option<String> {
    match value {
        mysql::Value::NULL => None,
        mysql::Value::Bytes(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        // The text protocol sends only bytes; anything else is rendered by the
        // driver's own SQL spelling, unquoted.
        other => Some(other.as_sql(true).trim_matches('\'').to_owned()),
    }
}

fn connect_mongo(config: &ConnectionConfig) -> Result<(Driver, Canceller), CoreError> {
    use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
    if config.database.is_empty() {
        return Err(connect_error("Name the database to open."));
    }
    let runtime = runtime()?;
    let credential = (!config.user.is_empty()).then(|| {
        Credential::builder()
            .username(config.user.clone())
            .password(config.password.clone())
            .source(config.database.clone())
            .build()
    });
    let options = ClientOptions::builder()
        .hosts(vec![ServerAddress::Tcp {
            host: config.host.clone(),
            port: Some(config.port),
        }])
        .credential(credential)
        .tls(match config.ssl_mode {
            SslMode::Disable => Tls::Disabled,
            SslMode::VerifyFull => Tls::Enabled(TlsOptions::default()),
        })
        .app_name("DBDelve Android".to_owned())
        .connect_timeout(CONNECT_TIMEOUT)
        .server_selection_timeout(CONNECT_TIMEOUT)
        .build();
    let database = runtime.block_on(async {
        let client = mongodb::Client::with_options(options).map_err(connect_error)?;
        let database = client.database(&config.database);
        database
            .run_command(bson::doc! { "ping": 1 })
            .await
            .map_err(connect_error)?;
        Ok::<_, CoreError>(database)
    })?;
    Ok((Driver::Mongo { runtime, database }, Canceller::Mongo))
}

async fn run_mongo(
    database: &mongodb::Database,
    statement: &str,
    limit: usize,
) -> Result<ResultSet, CoreError> {
    let command = mongo_cmd::parse(statement).map_err(query_error)?;
    let Some(collection) = command.collection else {
        return run_mongo_users(database, command.call, limit).await;
    };
    let collection = database.collection::<Document>(&collection);
    let one = |name: &str, value: u64| ResultSet {
        columns: vec![Column {
            name: name.to_owned(),
            type_tag: "int64".to_owned(),
        }],
        rows: vec![vec![Some(value.to_string())]],
        capped: false,
        affected: Some(value),
        elapsed_ms: 0,
    };
    match command.call {
        Call::Find { filter, projection } => {
            let mut find = collection.find(filter);
            if let Some(projection) = projection {
                find = find.projection(projection);
            }
            documents(find.await.map_err(query_error)?, limit).await
        }
        Call::Aggregate { pipeline } => {
            documents(
                collection.aggregate(pipeline).await.map_err(query_error)?,
                limit,
            )
            .await
        }
        Call::Count { filter } => Ok(one(
            "count",
            collection
                .count_documents(filter)
                .await
                .map_err(query_error)?,
        )),
        Call::InsertOne { document } => {
            collection.insert_one(document).await.map_err(query_error)?;
            Ok(one("inserted", 1))
        }
        Call::InsertMany { documents } => {
            let inserted = collection
                .insert_many(documents)
                .await
                .map_err(query_error)?;
            Ok(one("inserted", inserted.inserted_ids.len() as u64))
        }
        Call::UpdateOne { filter, update } => {
            let updated = collection
                .update_one(filter, update)
                .await
                .map_err(query_error)?;
            Ok(one("modified", updated.modified_count))
        }
        Call::UpdateMany { filter, update } => {
            let updated = collection
                .update_many(filter, update)
                .await
                .map_err(query_error)?;
            Ok(one("modified", updated.modified_count))
        }
        Call::DeleteOne { filter } => Ok(one(
            "deleted",
            collection
                .delete_one(filter)
                .await
                .map_err(query_error)?
                .deleted_count,
        )),
        Call::DeleteMany { filter } => Ok(one(
            "deleted",
            collection
                .delete_many(filter)
                .await
                .map_err(query_error)?
                .deleted_count,
        )),
        Call::Drop => {
            collection.drop().await.map_err(query_error)?;
            Ok(empty_result())
        }
        _ => Err(query_error(
            "That method belongs to the database: db.<method>(…).",
        )),
    }
}

/// The database's own user methods, as the commands the shell sends.
async fn run_mongo_users(
    database: &mongodb::Database,
    call: Call,
    limit: usize,
) -> Result<ResultSet, CoreError> {
    let command = match call {
        Call::GetUsers => {
            let reply = database
                .run_command(bson::doc! { "usersInfo": 1 })
                .await
                .map_err(query_error)?;
            let users = reply
                .get_array("users")
                .map_err(query_error)?
                .iter()
                .filter_map(|user| user.as_document().cloned())
                .collect::<Vec<_>>();
            let capped = users.len() > limit;
            return Ok(rows_of(users.into_iter().take(limit).collect(), capped));
        }
        Call::CreateUser {
            user,
            password,
            roles,
        } => bson::doc! { "createUser": user, "pwd": password, "roles": roles },
        Call::SetPassword { user, password } => bson::doc! { "updateUser": user, "pwd": password },
        Call::GrantRoles { user, roles } => bson::doc! { "grantRolesToUser": user, "roles": roles },
        Call::RevokeRoles { user, roles } => {
            bson::doc! { "revokeRolesFromUser": user, "roles": roles }
        }
        Call::DropUser { user } => bson::doc! { "dropUser": user },
        _ => {
            return Err(query_error(
                "That method belongs to a collection: db.<collection>.<method>(…).",
            ));
        }
    };
    database.run_command(command).await.map_err(query_error)?;
    Ok(empty_result())
}

/// Documents as rows: a column per top-level field in the order fields first
/// appear, typed by the first value that is not null. Stops reading at the
/// limit and drops the cursor, which ends it on the server.
async fn documents(
    mut cursor: mongodb::Cursor<Document>,
    limit: usize,
) -> Result<ResultSet, CoreError> {
    let mut read = Vec::new();
    let mut capped = false;
    while let Some(document) = cursor.next().await {
        let document = document.map_err(query_error)?;
        if read.len() == limit {
            capped = true;
            break;
        }
        read.push(document);
    }
    drop(cursor);
    Ok(rows_of(read, capped))
}

fn rows_of(read: Vec<Document>, capped: bool) -> ResultSet {
    let mut columns: Vec<Column> = Vec::new();
    for document in &read {
        for (key, value) in document {
            match columns.iter_mut().find(|column| &column.name == key) {
                Some(column) if column.type_tag.is_empty() && *value != Bson::Null => {
                    column.type_tag = bson_type(value);
                }
                Some(_) => {}
                None => columns.push(Column {
                    name: key.clone(),
                    type_tag: if *value == Bson::Null {
                        String::new()
                    } else {
                        bson_type(value)
                    },
                }),
            }
        }
    }
    let rows = read
        .iter()
        .map(|document| {
            columns
                .iter()
                .map(|column| match document.get(&column.name) {
                    None | Some(Bson::Null) => None,
                    Some(Bson::String(text)) => Some(text.clone()),
                    Some(value) => Some(value.clone().into_relaxed_extjson().to_string()),
                })
                .collect()
        })
        .collect();
    ResultSet {
        columns,
        rows,
        capped,
        affected: None,
        elapsed_ms: 0,
    }
}

fn bson_type(value: &Bson) -> String {
    format!("{:?}", value.element_type()).to_ascii_lowercase()
}

fn mongo_roles(roles: Option<&Vec<Bson>>) -> Vec<RoleRef> {
    roles
        .into_iter()
        .flatten()
        .filter_map(Bson::as_document)
        .map(|role| RoleRef {
            role: role.get_str("role").unwrap_or_default().to_owned(),
            database: role.get_str("db").unwrap_or_default().to_owned(),
        })
        .collect()
}

/// A `SHOW GRANTS` line without its password hash: what an account may do is
/// the point, and the hash is not.
fn without_hash(grant: &str) -> String {
    const MARK: &str = " IDENTIFIED BY PASSWORD '";
    match grant.find(MARK) {
        Some(start) => {
            let after = start + MARK.len();
            let end = grant[after..]
                .find('\'')
                .map_or(grant.len(), |end| after + end + 1);
            format!("{}{}", &grant[..start], &grant[end..])
        }
        None => grant.to_owned(),
    }
}
