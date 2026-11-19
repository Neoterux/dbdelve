//! DBDelve's Android core: the three engines the app speaks, the gate every
//! statement passes, and nothing that knows it is on a phone.
//!
//! What crosses into Kotlin is records of strings and type tags, never a
//! driver's row or value type, and the engine is a closed enum that stops
//! here: the UI asks this crate, it never matches on a driver.

uniffi::setup_scaffolding!();

pub mod gate;
pub mod mongo_cmd;
mod session;
pub mod split;
pub mod users;

pub use gate::{Destructive, Mode, Stop, Verdict};
pub use session::{CatalogEntry, Column, ObjectKind, ResultSet, RunOutcome, Session};
pub use split::Span;
pub use users::{Account, DbUser, Prepared, RoleRef, Scope, ScopeKind, UserAbilities, UserAction};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum Engine {
    Postgres,
    /// MySQL from 5.6.5 on, the first release that can hold a session to reads.
    MySql,
    Mongo,
}

/// Only the two modes this app can honour. A mode it cannot is refused by
/// name, never quietly weakened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SslMode {
    Disable,
    /// TLS 1.2 or later, the certificate checked against Mozilla's roots and
    /// the host name.
    VerifyFull,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct ConnectionConfig {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Blank is a valid password.
    pub password: String,
    pub database: String,
    pub ssl_mode: SslMode,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    // Not `message`: Kotlin's `Exception` already has one.
    #[error("{reason}")]
    Connect { reason: String },
    #[error("{reason}")]
    Query { reason: String },
}

#[uniffi::export]
pub fn classify(engine: Engine, text: String) -> Verdict {
    gate::classify(engine, &text)
}

#[uniffi::export]
pub fn statements(engine: Engine, text: String) -> Vec<Span> {
    split::statements(engine, &text)
}

#[uniffi::export]
pub fn statement_at(engine: Engine, text: String, cursor: u32) -> Option<Span> {
    split::statement_at(engine, &text, cursor)
}

#[uniffi::export]
pub fn default_port(engine: Engine) -> u16 {
    match engine {
        Engine::Postgres => 5432,
        Engine::MySql => 3306,
        Engine::Mongo => 27017,
    }
}

/// Whether the server holds a Read-only session to reads, so the UI can say
/// when nothing but the gate does.
#[uniffi::export]
pub fn holds_read_only(engine: Engine) -> bool {
    gate::holds_read_only(engine)
}

/// What user management can do on an engine.
#[uniffi::export]
pub fn user_abilities(engine: Engine) -> UserAbilities {
    users::abilities(engine)
}
