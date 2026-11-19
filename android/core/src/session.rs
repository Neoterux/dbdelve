//! One open connection, and the only way a statement reaches it.
//!
//! `Session::run` classifies and gates before anything goes out, so no
//! caller can send a statement the gate has not read. Each driver blocks on
//! a runtime of its own, from the caller's thread: nothing that leaves this
//! module is a future.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::{Bson, Document};
use futures_util::{StreamExt, TryStreamExt, pin_mut};
use mysql::prelude::Queryable;
use tokio::runtime::Runtime;
use tokio_postgres::{CancelToken, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::gate::{self, Destructive, Mode, Stop, Verdict};
use crate::mongo_cmd::{self, Call};
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
    Postgres {
        token: CancelToken,
        tls: Option<MakeRustlsConnect>,
    },
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
        *self.mode.lock().unwrap()
    }

    /// The only place a session's mode changes. The server's hold follows it,
    /// and the mode is not changed if the server will not.
    pub fn set_mode(&self, mode: Mode) -> Result<(), CoreError> {
        self.hold(mode)?;
        *self.mode.lock().unwrap() = mode;
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
        let mode = self.mode();
        let allowed = once_in.map_or(mode, |once| once.max(mode));
        let verdict = gate::classify(self.engine, &statement);
        match gate::gate(&verdict, allowed, &confirmed) {
            Some(Stop::RunOnce) if run_once => {}
            Some(stop) => return Ok(RunOutcome::Stopped { stop, verdict }),
            None => {}
        }
        let lifted = allowed != mode;
        if lifted {
            self.hold(allowed)?;
        }
        let result = self.send(&statement, row_limit as usize);
        if lifted {
            self.hold(mode)?;
        }
        result.map(|result| RunOutcome::Done { result })
    }

    /// Stops the statement running now, from any thread.
    pub fn cancel(&self) {
        match &self.canceller {
            Canceller::Postgres { token, tls } => {
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
        let mut driver = self.driver.lock().unwrap();
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
                            schema: row.get(0).unwrap_or_default().to_owned(),
                            name: row.get(1).unwrap_or_default().to_owned(),
                            kind: if matches!(row.get(2), Some("v" | "m")) {
                                ObjectKind::View
                            } else {
                                ObjectKind::Table
                            },
                            size_bytes: row.get(3).and_then(|size| size.parse().ok()),
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
    /// Puts the server's own read-only hold where `mode` says. A backstop
    /// behind the gate, not the boundary: a write-less role is.
    fn hold(&self, mode: Mode) -> Result<(), CoreError> {
        let read_only = mode == Mode::ReadOnly;
        let mut driver = self.driver.lock().unwrap();
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
        let mut driver = self.driver.lock().unwrap();
        let mut result = match &mut *driver {
            Driver::Postgres { runtime, client } => {
                runtime.block_on(run_postgres(client, statement, limit))?
            }
            Driver::MySql(conn) => run_mysql(conn, statement, limit)?,
            Driver::Mongo { runtime, database } => {
                runtime.block_on(run_mongo(database, statement, limit))?
            }
        };
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }
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
    let token = client.cancel_token();
    Ok((
        Driver::Postgres { runtime, client },
        Canceller::Postgres { token, tls },
    ))
}

// ponytail: a read past the limit is drained to its end, where the desktop
// cancels it once it can prove the cancel undoes nothing. Bounded by the
// statement, not the limit; a cancel (`Session::cancel`) stops it.
async fn run_postgres(
    client: &tokio_postgres::Client,
    statement: &str,
    limit: usize,
) -> Result<ResultSet, CoreError> {
    let stream = client
        .simple_query_raw(statement)
        .await
        .map_err(|error| query_error(postgres_error(error)))?;
    pin_mut!(stream);
    let mut result = empty_result();
    while let Some(message) = stream
        .try_next()
        .await
        .map_err(|error| query_error(postgres_error(error)))?
    {
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
                            .map(|at| row.get(at).map(str::to_owned))
                            .collect(),
                    );
                } else {
                    result.capped = true;
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

// ponytail: as on Postgres, a read past the limit is drained rather than
// killed; the desktop's `KILL QUERY` at the limit is the upgrade path.
fn run_mysql(
    conn: &mut mysql::Conn,
    statement: &str,
    limit: usize,
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
        for row in set {
            let row = row.map_err(query_error)?;
            if result.rows.len() < limit {
                result
                    .rows
                    .push(row.unwrap().into_iter().map(mysql_text).collect());
            } else {
                result.capped = true;
            }
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
    let collection = database.collection::<Document>(&command.collection);
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
    }
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
    Ok(ResultSet {
        columns,
        rows,
        capped,
        affected: None,
        elapsed_ms: 0,
    })
}

fn bson_type(value: &Bson) -> String {
    format!("{:?}", value.element_type()).to_ascii_lowercase()
}
