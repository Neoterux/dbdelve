//! What a statement may do, and whether the connection's mode lets it.
//!
//! The desktop's `sql::classify` and `sql::gate`, cut down to the engines this
//! app speaks. Anything not listed needs Full, and anything that will not parse
//! is `Unreadable`: a gate that guesses is worse than none.

use sqlparser::ast::{
    AlterTableOperation, Query, Set, SetExpr, Statement, TransactionAccessMode, TransactionMode,
};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::Engine;

/// Variant order is load-bearing: `Ord` derives from it, and the whole check
/// is `required <= allowed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, uniffi::Enum)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Destructive {
    Drop,
    Truncate,
    UnfilteredDelete,
    Unreadable,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Verdict {
    pub mode: Mode,
    /// Every destructive kind the submission carries, not just the first, so
    /// one confirmed kind never masks another.
    pub destructive: Vec<Destructive>,
}

impl Verdict {
    pub const READ: Self = Self {
        mode: Mode::ReadOnly,
        destructive: Vec::new(),
    };
    pub const WRITE: Self = Self {
        mode: Mode::ReadWrite,
        destructive: Vec::new(),
    };
    pub const FULL: Self = Self {
        mode: Mode::Full,
        destructive: Vec::new(),
    };

    pub fn destroys(kind: Destructive) -> Self {
        Self {
            mode: Mode::Full,
            destructive: vec![kind],
        }
    }

    pub fn max(mut self, other: Self) -> Self {
        self.mode = self.mode.max(other.mode);
        for kind in other.destructive {
            if !self.destructive.contains(&kind) {
                self.destructive.push(kind);
            }
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Stop {
    /// The mode carried is exactly what the statement needs, never higher.
    Upgrade {
        mode: Mode,
    },
    Confirm {
        kind: Destructive,
    },
    /// Unreadable: runs once on confirmation, and changes no mode.
    RunOnce,
}

/// Whether the mode stops this verdict. `None` means run it.
pub fn gate(verdict: &Verdict, mode: Mode, confirmed: &[Destructive]) -> Option<Stop> {
    // Before the mode comparison: every typo lands here, and asking for Full to
    // get a syntax error back would teach people to live in Full.
    if verdict.destructive.contains(&Destructive::Unreadable) {
        if verdict.mode > mode {
            return Some(Stop::Upgrade { mode: verdict.mode });
        }
        return Some(Stop::RunOnce);
    }
    if verdict.mode > mode {
        return Some(Stop::Upgrade { mode: verdict.mode });
    }
    verdict
        .destructive
        .iter()
        .find(|kind| !confirmed.contains(kind))
        .map(|&kind| Stop::Confirm { kind })
}

pub fn classify(engine: Engine, sql: &str) -> Verdict {
    match engine {
        Engine::Mongo => crate::mongo_cmd::classify(sql),
        Engine::Postgres => classify_sql(&PostgreSqlDialect {}, sql),
        Engine::MySql => {
            // `/*! … */` is a comment to the parser and a statement to MySQL.
            if sql.contains("/*!") {
                return unreadable();
            }
            classify_sql(&MySqlDialect {}, sql)
        }
    }
}

/// What the gate cannot read needs Read-write even to run once, on every
/// engine. The server's read-only hold is a backstop, not a boundary: the
/// statement itself can lift it (`BEGIN READ WRITE`), and some writes ignore
/// it (`SELECT … INTO OUTFILE`, signalling another backend). So Read-only
/// runs only what the gate has read as a read.
pub fn unreadable() -> Verdict {
    Verdict {
        mode: Mode::ReadWrite,
        destructive: vec![Destructive::Unreadable],
    }
}

fn classify_sql(dialect: &dyn Dialect, sql: &str) -> Verdict {
    // Whether a backslash escapes the next character is the server's own
    // setting (`standard_conforming_strings`, `NO_BACKSLASH_ESCAPES`), which
    // the parser cannot see. Read the other way, `'a\'; DROP TABLE t; -- '`
    // is one string to the parser and a DROP to the server. The tokenizer has
    // already unescaped a literal by the time its text could be checked, so
    // the raw text is: any backslash, and the gate cannot vouch for it.
    if sql.contains('\\') {
        return unreadable();
    }
    let statements = match Parser::parse_sql(dialect, sql) {
        Ok(statements) if !statements.is_empty() => statements,
        _ => return unreadable(),
    };
    statements
        .iter()
        .map(statement_verdict)
        .fold(Verdict::READ, Verdict::max)
        .max(calls_verdict(dialect, sql))
}

fn statement_verdict(statement: &Statement) -> Verdict {
    let verdict = variant_verdict(statement);
    match owned_query(statement) {
        Some(query) => verdict.max(query_verdict(query)),
        None => verdict,
    }
}

fn owned_query(statement: &Statement) -> Option<&Query> {
    match statement {
        Statement::Query(query) => Some(query),
        Statement::CreateTable(create) => create.query.as_deref(),
        Statement::CreateView(create) => Some(&create.query),
        Statement::Insert(insert) => insert.source.as_deref(),
        _ => None,
    }
}

fn variant_verdict(statement: &Statement) -> Verdict {
    match statement {
        Statement::Query(_) => Verdict::READ,
        // `EXPLAIN ANALYZE` runs the statement for real.
        Statement::Explain {
            analyze,
            options,
            statement,
            ..
        } => {
            let analyze = *analyze
                || options.iter().flatten().any(|option| {
                    let name = option.name.value.to_ascii_lowercase();
                    (name == "analyze" || name == "analyse")
                        && option.arg.as_ref().is_none_or(|arg| {
                            !matches!(
                                arg.to_string().to_ascii_lowercase().as_str(),
                                "false" | "off" | "0"
                            )
                        })
                });
            if analyze {
                statement_verdict(statement)
            } else {
                Verdict::READ
            }
        }
        Statement::ExplainTable { .. } => Verdict::READ,
        Statement::StartTransaction {
            statements,
            exception,
            ..
        } if !statements.is_empty() || exception.is_some() => {
            if exception.is_some() {
                Verdict::FULL
            } else {
                statements
                    .iter()
                    .map(statement_verdict)
                    .fold(Verdict::READ, Verdict::max)
            }
        }
        // A server-wide setting outlives the session and reaches every other
        // one; a read-only transaction does not stop it.
        Statement::Set(set) if sets_globally(set) => Verdict::FULL,
        Statement::Set(set) if set_lifts_hold(set) => Verdict::WRITE,
        Statement::StartTransaction { modes, .. } if opens_read_write(modes) => Verdict::WRITE,

        Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowProcessList { .. }
        | Statement::Use { .. }
        | Statement::Set { .. }
        | Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => Verdict::READ,

        Statement::CreateTable(create) if create.or_replace => Verdict::destroys(Destructive::Drop),
        Statement::Insert(insert) if insert.overwrite => Verdict::destroys(Destructive::Truncate),
        Statement::Insert { .. }
        | Statement::Update { .. }
        | Statement::CreateTable { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateView { .. }
        | Statement::CreateSchema { .. }
        | Statement::Comment { .. }
        | Statement::Analyze { .. }
        | Statement::Vacuum { .. } => Verdict::WRITE,

        Statement::Delete(delete) => {
            if delete.selection.is_some() {
                Verdict::WRITE
            } else {
                Verdict::destroys(Destructive::UnfilteredDelete)
            }
        }
        Statement::Drop { .. } => Verdict::destroys(Destructive::Drop),
        Statement::Truncate { .. } => Verdict::destroys(Destructive::Truncate),
        Statement::AlterTable(alter) => alter
            .operations
            .iter()
            .map(|operation| match operation {
                AlterTableOperation::AddColumn { .. } => Verdict::WRITE,
                _ => Verdict::FULL,
            })
            .fold(Verdict::WRITE, Verdict::max),

        // A statement nobody has decided is safe. `Statement` is not
        // `#[non_exhaustive]`, so a variant a later sqlparser adds lands here
        // silently, and this arm has to be the conservative one.
        _ => Verdict::FULL,
    }
}

/// A CTE body can be a `DELETE`.
fn query_verdict(query: &Query) -> Verdict {
    let mut verdict = set_expr_verdict(&query.body);
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            verdict = verdict.max(query_verdict(&cte.query));
        }
    }
    verdict
}

fn set_expr_verdict(body: &SetExpr) -> Verdict {
    match body {
        // `SELECT * INTO newt FROM t` creates a table.
        SetExpr::Select(select) if select.into.is_some() => Verdict::WRITE,
        SetExpr::Select(_) | SetExpr::Values(_) | SetExpr::Table(_) => Verdict::READ,
        SetExpr::Query(query) => query_verdict(query),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_verdict(left).max(set_expr_verdict(right))
        }
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_verdict(statement),
    }
}

const READ_ONLY_SETTINGS: &[&str] = &[
    "default_transaction_read_only",
    "transaction_read_only",
    "tx_read_only",
];

/// A `SET` that may lift the server's read-only hold is a write: anything but
/// plainly switching one of [`READ_ONLY_SETTINGS`] on.
fn set_lifts_hold(set: &Set) -> bool {
    let lifts = |name: String, values: &[String]| {
        let name = name.to_ascii_lowercase();
        let setting = name
            .rsplit('.')
            .next()
            .unwrap_or(&name)
            .trim_start_matches('@')
            .trim_matches(['"', '`'])
            .to_owned();
        let on = matches!(
            values,
            [value] if ["on", "true", "1", "yes"]
                .contains(&value.trim_matches('\'').to_ascii_lowercase().as_str())
        );
        READ_ONLY_SETTINGS.contains(&setting.as_str()) && !on
    };
    match set {
        Set::SingleAssignment {
            variable, values, ..
        } => lifts(
            variable.to_string(),
            &values.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ),
        Set::ParenthesizedAssignments { variables, .. } => variables
            .iter()
            .any(|variable| lifts(variable.to_string(), &[])),
        Set::MultipleAssignments { assignments } => assignments
            .iter()
            .any(|assignment| lifts(assignment.name.to_string(), &[assignment.value.to_string()])),
        Set::SetTransaction { modes, .. } => opens_read_write(modes),
        _ => false,
    }
}

fn sets_globally(set: &Set) -> bool {
    let text = set.to_string().to_ascii_lowercase();
    ["global", "persist", "@@global.", "@@persist."]
        .iter()
        .any(|word| {
            text.split(|c: char| c.is_whitespace() || c == '(')
                .any(|part| part.starts_with(word))
        })
}

fn opens_read_write(modes: &[TransactionMode]) -> bool {
    modes.contains(&TransactionMode::AccessMode(
        TransactionAccessMode::ReadWrite,
    ))
}

/// Calls that act outside the transaction they run in, so no read-only hold
/// stops them.
const HOLD_IGNORING_CALLS: &[&str] = &[
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_promote",
    "pg_switch_wal",
    "pg_create_restore_point",
    "pg_file_write",
    "pg_file_rename",
    "pg_file_unlink",
    "lo_import",
    "lo_export",
    "dblink",
    "dblink_exec",
    "dblink_send_query",
];

/// What the function calls in a submission do that its statements do not say.
// ponytail: a user-defined function that writes is held by the server in
// Read-only and runs unasked in Read-write, as on desktop. The desktop also
// reads Db2's and Snowflake's `s.NEXTVAL`; neither dialect is spoken here.
fn calls_verdict(dialect: &dyn Dialect, sql: &str) -> Verdict {
    let Ok(tokens) = Tokenizer::new(dialect, sql).tokenize() else {
        return Verdict::READ;
    };
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    let called = |at: usize| match (tokens.get(at), tokens.get(at + 1)) {
        (Some(Token::Word(word)), Some(Token::LParen)) => Some(word.value.to_ascii_lowercase()),
        _ => None,
    };
    let names: Vec<String> = (0..tokens.len()).filter_map(called).collect();
    if names
        .iter()
        .any(|name| HOLD_IGNORING_CALLS.contains(&name.as_str()))
    {
        return Verdict::FULL;
    }
    if names
        .iter()
        .any(|name| matches!(name.as_str(), "nextval" | "setval" | "set_config"))
    {
        return Verdict::WRITE;
    }
    Verdict::READ
}

/// Whether a read may be stopped once it passes the row limit, instead of
/// being drained to its end: one plain query that writes nothing and leaves
/// nothing behind in the session, so stopping it undoes nothing. Locks and
/// session variables are what a query can leave behind: an advisory or named
/// lock, `set_config`, or a MySQL `@x :=`.
pub fn stoppable(engine: Engine, sql: &str) -> bool {
    let dialect: Box<dyn Dialect> = match engine {
        Engine::Postgres => Box::new(PostgreSqlDialect {}),
        Engine::MySql => Box::new(MySqlDialect {}),
        // A MongoDB read stops by dropping its cursor, whatever it is.
        Engine::Mongo => return false,
    };
    if classify(engine, sql) != Verdict::READ {
        return false;
    }
    match Parser::parse_sql(dialect.as_ref(), sql) {
        Ok(statements) if matches!(statements.as_slice(), [Statement::Query(_)]) => {}
        _ => return false,
    }
    let Ok(tokens) = Tokenizer::new(dialect.as_ref(), sql).tokenize() else {
        return false;
    };
    let tokens: Vec<&Token> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    !tokens.iter().enumerate().any(|(at, token)| match token {
        Token::Assignment => true,
        Token::Word(word) if tokens.get(at + 1) == Some(&&Token::LParen) => {
            let name = word.value.to_ascii_lowercase();
            name.contains("advisory")
                || matches!(
                    name.as_str(),
                    "get_lock" | "release_lock" | "release_all_locks" | "set_config"
                )
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(engine: Engine, sql: &str) -> Verdict {
        classify(engine, sql)
    }

    #[test]
    fn reads_writes_and_destruction() {
        assert_eq!(verdict(Engine::Postgres, "SELECT 1"), Verdict::READ);
        assert_eq!(
            verdict(Engine::MySql, "UPDATE t SET a = 1 WHERE id = 2"),
            Verdict::WRITE
        );
        assert_eq!(
            verdict(Engine::Postgres, "DELETE FROM t"),
            Verdict::destroys(Destructive::UnfilteredDelete)
        );
        assert_eq!(
            verdict(Engine::Postgres, "DROP TABLE a; TRUNCATE b").destructive,
            vec![Destructive::Drop, Destructive::Truncate]
        );
        assert_eq!(
            verdict(Engine::MySql, "ALTER TABLE t ADD COLUMN c int"),
            Verdict::WRITE
        );
        assert_eq!(
            verdict(Engine::MySql, "ALTER TABLE t DROP COLUMN c").mode,
            Mode::Full
        );
    }

    #[test]
    fn hidden_writes_are_found() {
        let cte = "WITH x AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM x";
        assert_eq!(verdict(Engine::Postgres, cte), Verdict::WRITE);
        assert_eq!(
            verdict(Engine::Postgres, "EXPLAIN (ANALYZE) DELETE FROM t").mode,
            Mode::Full
        );
        assert_eq!(
            verdict(Engine::Postgres, "EXPLAIN DELETE FROM t"),
            Verdict::READ
        );
        assert_eq!(
            verdict(Engine::Postgres, "SELECT pg_terminate_backend(1)"),
            Verdict::FULL
        );
        assert_eq!(
            verdict(Engine::Postgres, "SELECT nextval('s')"),
            Verdict::WRITE
        );
        assert_eq!(
            verdict(
                Engine::Postgres,
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
            )
            .mode,
            Mode::ReadWrite,
        );
        assert_eq!(
            verdict(Engine::MySql, "SET SESSION tx_read_only = 0"),
            Verdict::WRITE
        );
        assert_eq!(
            verdict(Engine::MySql, "/*! DROP TABLE t */ SELECT 1"),
            unreadable()
        );
        assert_eq!(
            verdict(Engine::Postgres, "GRANT SELECT ON t TO u"),
            Verdict::FULL
        );
    }

    #[test]
    fn only_plain_reads_are_stoppable() {
        assert!(stoppable(Engine::Postgres, "SELECT * FROM t"));
        assert!(stoppable(Engine::MySql, "SELECT a FROM t WHERE b = ':='"));
        assert!(!stoppable(Engine::Postgres, "SELECT 1; SELECT 2"));
        assert!(!stoppable(Engine::Postgres, "SELECT pg_advisory_lock(1)"));
        assert!(!stoppable(Engine::MySql, "SELECT GET_LOCK('a', 1)"));
        assert!(!stoppable(Engine::MySql, "SELECT @x := 1"));
        assert!(!stoppable(
            Engine::Postgres,
            "SELECT set_config('a', 'b', false)"
        ));
        assert!(!stoppable(
            Engine::Postgres,
            "UPDATE t SET a = 1 WHERE b = 2 RETURNING *"
        ));
        assert!(!stoppable(Engine::Postgres, "SHOW ALL"));
        assert!(!stoppable(Engine::Postgres, "SELEC 1"));
    }

    #[test]
    fn what_the_server_could_read_otherwise_is_unreadable() {
        // A backslash escapes or not by a server setting the parser cannot
        // see; read the other way, each of these hides a DROP.
        for (engine, sql) in [
            (
                Engine::Postgres,
                r"SELECT 'x\' AS a, '; COMMIT; BEGIN READ WRITE; DROP TABLE t; -- '",
            ),
            (Engine::MySql, r"SELECT 'a\'; DROP TABLE t; -- '"),
            (Engine::Postgres, r"SELECT E'\\'"),
        ] {
            assert_eq!(classify(engine, sql), unreadable(), "{sql}");
        }
        // Writes the server's read-only hold does not stop: unreadable, so
        // never run in Read-only.
        assert_eq!(
            classify(Engine::MySql, "SELECT * FROM t INTO OUTFILE '/tmp/x'").mode,
            Mode::ReadWrite
        );
        assert_eq!(
            classify(
                Engine::Postgres,
                "DO $$BEGIN PERFORM pg_terminate_backend(1); END$$"
            )
            .mode,
            Mode::ReadWrite
        );
    }

    #[test]
    fn server_wide_settings_need_full() {
        for sql in ["SET GLOBAL read_only = 0", "SET @@global.general_log = 1"] {
            assert_eq!(classify(Engine::MySql, sql).mode, Mode::Full, "{sql}");
        }
        // MySQL 8's PERSIST does not parse: unreadable, so not in Read-only.
        assert_eq!(
            classify(Engine::MySql, "SET PERSIST max_connections = 10"),
            unreadable()
        );
        assert_eq!(
            classify(Engine::MySql, "SET SESSION sql_select_limit = 5"),
            Verdict::READ
        );
        assert_eq!(classify(Engine::MySql, "SET @globally = 1"), Verdict::READ);
    }

    #[test]
    fn gate_order() {
        let typo = unreadable();
        // Read-only runs only what the gate has read as a read.
        assert_eq!(
            gate(&typo, Mode::ReadOnly, &[]),
            Some(Stop::Upgrade {
                mode: Mode::ReadWrite
            })
        );
        assert_eq!(gate(&typo, Mode::ReadWrite, &[]), Some(Stop::RunOnce));
        assert_eq!(
            gate(
                &verdict(Engine::Postgres, "DROP TABLE t"),
                Mode::ReadWrite,
                &[]
            ),
            Some(Stop::Upgrade { mode: Mode::Full })
        );
        assert_eq!(
            gate(&verdict(Engine::Postgres, "DROP TABLE t"), Mode::Full, &[]),
            Some(Stop::Confirm {
                kind: Destructive::Drop
            })
        );
        assert_eq!(gate(&Verdict::READ, Mode::ReadOnly, &[]), None);
    }
}
