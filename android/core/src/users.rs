//! The server's own users and roles: who they are, what they may do, and the
//! statements that change it.
//!
//! Every statement here is built from validated parts: names quoted, privileges
//! taken from a fixed list, never a fragment of free text spliced in. The text
//! is held in a [`Prepared`] the UI can show and not edit, so what is reviewed
//! is what runs. Each needs Full, whatever the parser makes of it.
//!
//! Passwords for Postgres and MySQL are hashed here, on the phone: the server,
//! its statement log and the review dialog only ever see the hash. MongoDB
//! hashes on the server, so its password crosses the connection.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::{digest, hmac, pbkdf2, rand::SecureRandom};
use std::num::NonZeroU32;

use crate::gate::{self, Destructive, Mode, Verdict};
use crate::{CoreError, Engine};

/// An account as the server names it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Account {
    pub name: String,
    /// MySQL names an account by user and host; empty elsewhere.
    pub host: String,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct DbUser {
    pub account: Account,
    pub can_login: bool,
    pub superuser: bool,
    /// What else the server says about it, already worded: "create db",
    /// "password expired", "valid until …".
    pub attributes: Vec<String>,
    /// Roles it is a member of (Postgres) or holds (MongoDB).
    pub roles: Vec<RoleRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RoleRef {
    pub role: String,
    /// The database a MongoDB role is defined in; empty on Postgres.
    pub database: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Scope {
    /// Every database on the server (MySQL's `*.*`).
    Server,
    /// Every table in a schema (Postgres) or database (MySQL).
    Schema {
        name: String,
    },
    Table {
        schema: String,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ScopeKind {
    Server,
    Schema,
    Table,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum UserAction {
    Create {
        account: Account,
        password: String,
        can_login: bool,
    },
    SetPassword {
        account: Account,
        password: String,
    },
    SetLogin {
        account: Account,
        enabled: bool,
    },
    Grant {
        account: Account,
        privileges: Vec<String>,
        scope: Scope,
    },
    Revoke {
        account: Account,
        privileges: Vec<String>,
        scope: Scope,
    },
    /// `role_database` is MongoDB's: the database the role is defined in.
    GrantRole {
        account: Account,
        role: String,
        role_database: String,
    },
    RevokeRole {
        account: Account,
        role: String,
        role_database: String,
    },
    Drop {
        account: Account,
    },
}

/// What an engine's user management can do, so the UI asks rather than
/// matching on the engine.
#[derive(Clone, Debug, uniffi::Record)]
pub struct UserAbilities {
    pub host: bool,
    pub login_toggle: bool,
    pub roles: bool,
    pub privileges: Vec<String>,
    pub scopes: Vec<ScopeKind>,
    pub suggested_roles: Vec<String>,
    /// The longest user name the server takes; 0 for no limit worth stating.
    pub name_limit: u32,
    /// Where the password is hashed, said plainly for the form.
    pub password_note: String,
}

const POSTGRES_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];

const MYSQL_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "CREATE VIEW",
    "SHOW VIEW",
    "EXECUTE",
    "CREATE ROUTINE",
    "ALTER ROUTINE",
    "LOCK TABLES",
];

const MONGO_ROLES: &[&str] = &["read", "readWrite", "dbAdmin", "dbOwner", "userAdmin"];

pub fn abilities(engine: Engine) -> UserAbilities {
    match engine {
        Engine::Postgres => UserAbilities {
            host: false,
            login_toggle: true,
            roles: true,
            privileges: POSTGRES_PRIVILEGES.iter().map(|p| p.to_string()).collect(),
            scopes: vec![ScopeKind::Schema, ScopeKind::Table],
            suggested_roles: vec![
                "pg_read_all_data".into(),
                "pg_write_all_data".into(),
                "pg_monitor".into(),
            ],
            name_limit: 63,
            password_note: "Hashed on this phone as SCRAM-SHA-256: the server never sees it."
                .into(),
        },
        Engine::MySql => UserAbilities {
            host: true,
            login_toggle: false,
            roles: false,
            privileges: MYSQL_PRIVILEGES.iter().map(|p| p.to_string()).collect(),
            scopes: vec![ScopeKind::Server, ScopeKind::Schema, ScopeKind::Table],
            suggested_roles: Vec::new(),
            name_limit: MYSQL_NAME_LIMIT as u32,
            password_note: "Hashed on this phone as mysql_native_password: the server never \
                            sees it. MySQL 5.6 and 5.7 accept this; 8.0 does not."
                .into(),
        },
        Engine::Mongo => UserAbilities {
            host: false,
            login_toggle: false,
            roles: true,
            privileges: Vec::new(),
            scopes: Vec::new(),
            suggested_roles: MONGO_ROLES.iter().map(|r| r.to_string()).collect(),
            name_limit: 0,
            password_note: "MongoDB hashes it on the server, so it crosses the connection: \
                            use TLS."
                .into(),
        },
    }
}

/// A statement built for an action, held where the UI cannot change it.
#[derive(uniffi::Object)]
pub struct Prepared {
    pub(crate) text: String,
    shown: String,
    pub(crate) verdict: Verdict,
}

#[uniffi::export]
impl Prepared {
    /// The statement as the review shows it: exactly what runs, except a
    /// MongoDB password, which is masked.
    pub fn shown(&self) -> String {
        self.shown.clone()
    }

    pub fn verdict(&self) -> Verdict {
        self.verdict.clone()
    }
}

pub fn prepare(engine: Engine, database: &str, action: &UserAction) -> Result<Prepared, CoreError> {
    let (text, shown) = statement(engine, database, action).map_err(refused)?;
    // Full whatever the parser says: who may do what is the server's own
    // boundary. Unreadable is dropped because the review shows this exact
    // text and asks before it runs, which is all running once would ask.
    let parsed = gate::classify(engine, &text);
    let mut verdict = Verdict {
        mode: Mode::Full,
        destructive: parsed
            .destructive
            .into_iter()
            .filter(|kind| *kind != Destructive::Unreadable)
            .collect(),
    };
    if matches!(action, UserAction::Drop { .. }) {
        verdict = verdict.max(Verdict::destroys(Destructive::Drop));
    }
    Ok(Prepared {
        text,
        shown,
        verdict,
    })
}

fn refused(reason: String) -> CoreError {
    CoreError::Query { reason }
}

/// The statement for an action, and how the review shows it.
pub fn statement(
    engine: Engine,
    database: &str,
    action: &UserAction,
) -> Result<(String, String), String> {
    let text = match engine {
        Engine::Postgres => postgres(action)?,
        Engine::MySql => mysql(action)?,
        Engine::Mongo => return mongo(database, action),
    };
    Ok((text.clone(), text))
}

fn account(action: &UserAction) -> &Account {
    match action {
        UserAction::Create { account, .. }
        | UserAction::SetPassword { account, .. }
        | UserAction::SetLogin { account, .. }
        | UserAction::Grant { account, .. }
        | UserAction::Revoke { account, .. }
        | UserAction::GrantRole { account, .. }
        | UserAction::RevokeRole { account, .. }
        | UserAction::Drop { account } => account,
    }
}

/// Names go into quoted identifiers and literals. A backslash is refused
/// rather than escaped: whether it escapes depends on server settings this
/// cannot see, and a name that means two things is worse than one refused.
fn checked(what: &str, name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(format!("The {what} needs a name."));
    }
    if name.contains('\\') || name.contains('\0') {
        return Err(format!("The {what} cannot contain a backslash or a NUL."));
    }
    Ok(())
}

fn privileges(allowed: &[&str], asked: &[String]) -> Result<String, String> {
    if asked.is_empty() {
        return Err("Pick at least one privilege.".into());
    }
    for privilege in asked {
        if !allowed.contains(&privilege.as_str()) {
            return Err(format!("{privilege} is not a privilege this app grants."));
        }
    }
    Ok(asked.join(", "))
}

fn identifier(name: &str, mark: char) -> String {
    format!(
        "{mark}{}{mark}",
        name.replace(mark, &format!("{mark}{mark}"))
    )
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn postgres(action: &UserAction) -> Result<String, String> {
    let role = &account(action).name;
    checked("role", role)?;
    let quoted = identifier(role, '"');
    let on = |scope: &Scope| -> Result<String, String> {
        match scope {
            Scope::Schema { name } => {
                checked("schema", name)?;
                Ok(format!("ALL TABLES IN SCHEMA {}", identifier(name, '"')))
            }
            Scope::Table { schema, name } => {
                checked("schema", schema)?;
                checked("table", name)?;
                Ok(format!(
                    "TABLE {}.{}",
                    identifier(schema, '"'),
                    identifier(name, '"')
                ))
            }
            Scope::Server => Err("Postgres grants per schema or table.".into()),
        }
    };
    Ok(match action {
        UserAction::Create {
            password,
            can_login,
            ..
        } => {
            let login = if *can_login { "LOGIN" } else { "NOLOGIN" };
            format!(
                "CREATE ROLE {quoted} {login} PASSWORD {}",
                literal(&scram(password)?)
            )
        }
        UserAction::SetPassword { password, .. } => {
            format!(
                "ALTER ROLE {quoted} PASSWORD {}",
                literal(&scram(password)?)
            )
        }
        UserAction::SetLogin { enabled, .. } => {
            format!(
                "ALTER ROLE {quoted} {}",
                if *enabled { "LOGIN" } else { "NOLOGIN" }
            )
        }
        UserAction::Grant {
            privileges: asked,
            scope,
            ..
        } => {
            let granted = format!(
                "GRANT {} ON {} TO {quoted}",
                privileges(POSTGRES_PRIVILEGES, asked)?,
                on(scope)?
            );
            // Rights on a schema's tables are no use without the schema.
            match scope {
                Scope::Schema { name } | Scope::Table { schema: name, .. } => format!(
                    "GRANT USAGE ON SCHEMA {} TO {quoted};\n{granted}",
                    identifier(name, '"')
                ),
                Scope::Server => granted,
            }
        }
        UserAction::Revoke {
            privileges: asked,
            scope,
            ..
        } => {
            let revoked = format!(
                "REVOKE {} ON {} FROM {quoted}",
                privileges(POSTGRES_PRIVILEGES, asked)?,
                on(scope)?
            );
            // The schema's use goes with its tables, as the grant gave both;
            // left behind, it is what stops the role being dropped. A single
            // table leaves it: other grants in the schema may need it.
            match scope {
                Scope::Schema { name } => format!(
                    "{revoked};\nREVOKE USAGE ON SCHEMA {} FROM {quoted}",
                    identifier(name, '"')
                ),
                _ => revoked,
            }
        }
        UserAction::GrantRole { role: group, .. } => {
            checked("role to grant", group)?;
            format!("GRANT {} TO {quoted}", identifier(group, '"'))
        }
        UserAction::RevokeRole { role: group, .. } => {
            checked("role to revoke", group)?;
            format!("REVOKE {} FROM {quoted}", identifier(group, '"'))
        }
        UserAction::Drop { .. } => format!("DROP ROLE {quoted}"),
    })
}

/// MySQL 5.6's limit, in characters; 5.7 raised it to 32.
const MYSQL_NAME_LIMIT: usize = 16;

fn mysql_account(account: &Account) -> Result<String, String> {
    checked("user", &account.name)?;
    if account.name.chars().count() > MYSQL_NAME_LIMIT {
        return Err(format!(
            "MySQL 5.6 user names are at most {MYSQL_NAME_LIMIT} characters."
        ));
    }
    let host = if account.host.trim().is_empty() {
        "%"
    } else {
        account.host.trim()
    };
    checked("host", host)?;
    Ok(format!("{}@{}", literal(&account.name), literal(host)))
}

fn mysql(action: &UserAction) -> Result<String, String> {
    let user = mysql_account(account(action))?;
    let on = |scope: &Scope| -> Result<String, String> {
        match scope {
            Scope::Server => Ok("*.*".into()),
            Scope::Schema { name } => {
                checked("database", name)?;
                Ok(format!("{}.*", identifier(name, '`')))
            }
            Scope::Table { schema, name } => {
                checked("database", schema)?;
                checked("table", name)?;
                Ok(format!(
                    "{}.{}",
                    identifier(schema, '`'),
                    identifier(name, '`')
                ))
            }
        }
    };
    Ok(match action {
        UserAction::Create { password, .. } => format!(
            "CREATE USER {user} IDENTIFIED BY PASSWORD {}",
            literal(&native_password(password))
        ),
        UserAction::SetPassword { password, .. } => {
            format!(
                "SET PASSWORD FOR {user} = {}",
                literal(&native_password(password))
            )
        }
        UserAction::Grant {
            privileges: asked,
            scope,
            ..
        } => format!(
            "GRANT {} ON {} TO {user}",
            privileges(MYSQL_PRIVILEGES, asked)?,
            on(scope)?
        ),
        UserAction::Revoke {
            privileges: asked,
            scope,
            ..
        } => format!(
            "REVOKE {} ON {} FROM {user}",
            privileges(MYSQL_PRIVILEGES, asked)?,
            on(scope)?
        ),
        UserAction::Drop { .. } => format!("DROP USER {user}"),
        UserAction::SetLogin { .. } => {
            return Err("MySQL 5.6 cannot lock an account; change its password or drop it.".into());
        }
        UserAction::GrantRole { .. } | UserAction::RevokeRole { .. } => {
            return Err("MySQL 5.6 has no roles; grant privileges instead.".into());
        }
    })
}

/// MongoDB's shell spelling, which `mongo_cmd` reads back and the gate
/// classifies like any typed command. Strings go in as JSON, so a name is
/// always one string.
fn mongo(database: &str, action: &UserAction) -> Result<(String, String), String> {
    let user = &account(action).name;
    if user.trim().is_empty() {
        return Err("The user needs a name.".into());
    }
    let name = serde_json::Value::from(user.as_str()).to_string();
    let role = |role: &str, role_database: &str| -> Result<String, String> {
        if role.trim().is_empty() {
            return Err("Name the role.".into());
        }
        let on = if role_database.trim().is_empty() {
            database
        } else {
            role_database.trim()
        };
        Ok(serde_json::json!({ "role": role, "db": on }).to_string())
    };
    let masked = |password: &str, build: &dyn Fn(&str) -> String| {
        let pwd = serde_json::Value::from(password).to_string();
        (build(&pwd), build("\"••••••\""))
    };
    Ok(match action {
        UserAction::Create { password, .. } => masked(password, &|pwd| {
            format!("db.createUser({{user: {name}, pwd: {pwd}, roles: []}})")
        }),
        UserAction::SetPassword { password, .. } => masked(password, &|pwd| {
            format!("db.updateUser({name}, {{pwd: {pwd}}})")
        }),
        UserAction::GrantRole {
            role: granted,
            role_database,
            ..
        } => {
            let text = format!(
                "db.grantRolesToUser({name}, [{}])",
                role(granted, role_database)?
            );
            (text.clone(), text)
        }
        UserAction::RevokeRole {
            role: revoked,
            role_database,
            ..
        } => {
            let text = format!(
                "db.revokeRolesFromUser({name}, [{}])",
                role(revoked, role_database)?
            );
            (text.clone(), text)
        }
        UserAction::Drop { .. } => {
            let text = format!("db.dropUser({name})");
            (text.clone(), text)
        }
        UserAction::SetLogin { .. } => {
            return Err("MongoDB cannot lock a user; change its password or drop it.".into());
        }
        UserAction::Grant { .. } | UserAction::Revoke { .. } => {
            return Err("MongoDB grants through roles.".into());
        }
    })
}

const SCRAM_ITERATIONS: u32 = 4096;

/// A SCRAM-SHA-256 verifier, as `psql`'s `\password` builds one, which
/// Postgres 10 and later store as given.
///
/// Refuses anything but printable ASCII: Postgres normalises a password with
/// SASLprep before hashing it at login, and for printable ASCII that changes
/// nothing, so the verifier built here is the one the server would build.
fn scram(password: &str) -> Result<String, String> {
    if !password.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err(
            "Postgres passwords here are printable ASCII: the server would normalise anything \
             else before hashing it, and the hash built here would not match."
                .into(),
        );
    }
    let mut salt = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "No secure random numbers available.".to_owned())?;
    Ok(scram_with(password, &salt, SCRAM_ITERATIONS))
}

fn scram_with(password: &str, salt: &[u8], iterations: u32) -> String {
    let mut salted = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations).expect("iterations are not zero"),
        salt,
        password.as_bytes(),
        &mut salted,
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, &salted);
    let client_key = hmac::sign(&key, b"Client Key");
    let stored_key = digest::digest(&digest::SHA256, client_key.as_ref());
    let server_key = hmac::sign(&key, b"Server Key");
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        BASE64.encode(salt),
        BASE64.encode(stored_key.as_ref()),
        BASE64.encode(server_key.as_ref()),
    )
}

/// MySQL's `mysql_native_password` hash: `*` and SHA1(SHA1(password)) in
/// upper-case hex, what `PASSWORD()` returns on 5.6.
fn native_password(password: &str) -> String {
    let once = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, password.as_bytes());
    let twice = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, once.as_ref());
    let hex: String = twice
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    format!("*{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str) -> Account {
        Account {
            name: name.into(),
            host: String::new(),
        }
    }

    fn sql(engine: Engine, action: UserAction) -> String {
        statement(engine, "dev", &action).unwrap().0
    }

    #[test]
    fn mysql_hash_matches_password_function() {
        // SELECT PASSWORD('password') on MySQL 5.6.
        assert_eq!(
            native_password("password"),
            "*2470C0C06DEE42FD1618BB99005ADCA2EC9D1E19"
        );
        // A blank password is a valid one.
        assert_eq!(native_password("").len(), 41);
    }

    #[test]
    fn scram_matches_rfc_7677() {
        // RFC 7677's example: user "user", password "pencil", this salt.
        let salt = BASE64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let verifier = scram_with("pencil", &salt, 4096);
        // The server's signature in the RFC's exchange is HMAC(ServerKey, auth
        // message); checking the keys through it pins both.
        let server_key = BASE64.decode(verifier.rsplit(':').next().unwrap()).unwrap();
        let auth = "n=user,r=rOprNGfwEbeRWgbNEkqO,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let signature = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, &server_key),
            auth.as_bytes(),
        );
        assert_eq!(
            BASE64.encode(signature.as_ref()),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
        assert!(verifier.starts_with("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$"));
    }

    #[test]
    fn names_are_quoted_and_backslashes_refused() {
        let grant = sql(
            Engine::Postgres,
            UserAction::Grant {
                account: account(r#"we"ird"#),
                privileges: vec!["SELECT".into()],
                scope: Scope::Schema {
                    name: "public".into(),
                },
            },
        );
        assert_eq!(
            grant,
            "GRANT USAGE ON SCHEMA \"public\" TO \"we\"\"ird\";\nGRANT SELECT ON ALL TABLES IN SCHEMA \"public\" TO \"we\"\"ird\""
        );
        let drop = sql(
            Engine::MySql,
            UserAction::Drop {
                account: account("o'brien"),
            },
        );
        assert_eq!(drop, "DROP USER 'o''brien'@'%'");
        assert!(
            statement(
                Engine::MySql,
                "dev",
                &UserAction::Drop {
                    account: account(r"a\' OR 1"),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn privileges_come_from_the_list() {
        let asked = |privilege: &str| UserAction::Grant {
            account: account("app"),
            privileges: vec![privilege.into()],
            scope: Scope::Server,
        };
        assert!(statement(Engine::MySql, "dev", &asked("SELECT")).is_ok());
        assert!(statement(Engine::MySql, "dev", &asked("SELECT ON *.* TO x; DROP")).is_err());
        assert!(statement(Engine::MySql, "dev", &asked("SUPER")).is_err());
        // Postgres has no server-wide table grant.
        assert!(statement(Engine::Postgres, "dev", &asked("SELECT")).is_err());
    }

    #[test]
    fn mongo_masks_only_what_it_shows() {
        let (text, shown) = statement(
            Engine::Mongo,
            "dev",
            &UserAction::Create {
                account: account("app"),
                password: "s3cret\"".into(),
                can_login: true,
            },
        )
        .unwrap();
        assert_eq!(
            text,
            r#"db.createUser({user: "app", pwd: "s3cret\"", roles: []})"#
        );
        assert!(!shown.contains("s3cret"));
        // What runs reads back through the same parser the editor uses.
        assert!(crate::mongo_cmd::parse(&text).is_ok());
    }

    #[test]
    fn every_action_needs_full() {
        let actions = [
            UserAction::Create {
                account: account("app"),
                password: "pw".into(),
                can_login: true,
            },
            UserAction::GrantRole {
                account: account("app"),
                role: "reporting".into(),
                role_database: String::new(),
            },
            UserAction::Drop {
                account: account("app"),
            },
        ];
        for engine in [Engine::Postgres, Engine::MySql, Engine::Mongo] {
            for action in &actions {
                let Ok(prepared) = prepare(engine, "dev", action) else {
                    continue;
                };
                assert_eq!(prepared.verdict.mode, Mode::Full, "{engine:?} {action:?}");
                assert!(
                    !prepared
                        .verdict
                        .destructive
                        .contains(&Destructive::Unreadable)
                );
                if matches!(action, UserAction::Drop { .. }) {
                    assert!(prepared.verdict.destructive.contains(&Destructive::Drop));
                }
            }
        }
    }
}
