//! What sets one engine apart from another, as data: one [`EngineSpec`] per
//! [`Engine`](super::Engine), read through [`Engine::spec`](super::Engine::spec).
//!
//! A field is a fact or a choice among strategies the code already has, never
//! SQL text: the statements themselves are written where they are used, and a
//! spec only says which spelling that code picks. A new engine that fits the
//! existing strategies is one more value here and its driver.

use super::{ColumnDefinition, Fields, RelationKind, Syntax, mongo, mssql, mysql, postgres, redis};

#[derive(Clone, Copy, Debug)]
pub struct EngineSpec {
    /// The spelling stored in `profiles.toml`. Changing one orphans every
    /// profile already written with it.
    pub id: &'static str,
    pub label: &'static str,
    /// Every spelling [`Engine::parse`](super::Engine::parse) reads: the URL
    /// schemes as well as `id`.
    pub schemes: &'static [&'static str],
    /// What the welcome surface tells a pasted path of this engine's by,
    /// whatever its case.
    pub file_extensions: &'static [&'static str],
    pub logo: &'static str,
    /// 0xRRGGBB. The theme reads only its hue.
    pub brand: u32,
    pub syntax: Syntax,
    pub layout: Fields,
    pub default_port: Option<u16>,
    pub takes_options: bool,
    pub resolves_srv: bool,
    pub system_databases: &'static [&'static str],
    /// Exact type names read as bytes beyond the families every engine shares.
    pub binary_types: &'static [&'static str],
    pub hints: Hints,
    /// Meaningful only where `syntax` is [`Syntax::Sql`].
    pub sql: SqlSpec,
    pub abilities: Abilities,
    pub hooks: Hooks,
}

impl EngineSpec {
    pub fn quote_identifier(&self, identifier: &str) -> String {
        let quote = self.sql.identifier_quote.character();
        format!(
            "{quote}{}{quote}",
            identifier.replace(quote, &format!("{quote}{quote}"))
        )
    }

    /// The inverse of [`Self::quote_identifier`]; anything that is not a
    /// quoted identifier comes back unchanged.
    pub fn unquote_identifier(&self, expression: &str) -> String {
        let quote = self.sql.identifier_quote.character();
        match expression
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            Some(inner) => inner.replace(&format!("{quote}{quote}"), &quote.to_string()),
            None => expression.to_string(),
        }
    }

    pub fn qualified(&self, schema: &str, name: &str) -> String {
        format!(
            "{}.{}",
            self.quote_identifier(schema),
            self.quote_identifier(name)
        )
    }

    pub fn quote_literal(&self, value: &str) -> String {
        let value = match self.sql.backslash_escapes {
            true => value.replace('\\', r"\\"),
            false => value.to_string(),
        };
        let national = if self.sql.national_strings { "N" } else { "" };
        format!("{national}'{}'", value.replace('\'', "''"))
    }

    pub fn quote_value(&self, value: &str, data_type: Option<&str>) -> String {
        data_type
            .zip(self.hooks.quote_value)
            .and_then(|(data_type, typed)| typed(value, data_type))
            .unwrap_or_else(|| self.quote_literal(value))
    }

    /// Substrings because the families are open-ended in two directions:
    /// MySQL prefixes its blobs and binaries, and SQLite gives BLOB affinity to
    /// any declared type merely *containing* `blob`.
    pub fn is_binary_type(&self, data_type: &str) -> bool {
        let name = data_type.to_ascii_lowercase();
        name == "bytea"
            || name.contains("blob")
            || name.contains("binary")
            || self.binary_types.contains(&name.as_str())
    }

    /// [`Engine::writes_back`](super::Engine::writes_back). The ability is
    /// asked apart from the types, so a cell nobody typed is no way past it.
    pub fn writes_back<'a>(&self, types: impl IntoIterator<Item = &'a str>) -> bool {
        self.abilities.edits
            && types.into_iter().all(|data_type| match self.syntax {
                Syntax::Redis => crate::redis_cmd::edit::writes_back(data_type),
                Syntax::Sql | Syntax::Mongo => !self.is_binary_type(data_type),
            })
    }

    /// [`Engine::takes_inserts`](super::Engine::takes_inserts).
    pub fn takes_inserts(&self, kind: RelationKind) -> bool {
        self.abilities.inserts && (kind != RelationKind::View || self.abilities.inserts_into_views)
    }

    /// [`Engine::generates`](super::Engine::generates).
    pub fn generates(&self, column: &ColumnDefinition) -> bool {
        let Some(default) = column.default.as_deref() else {
            return false;
        };
        self.sql
            .generated_defaults
            .iter()
            .any(|marker| match *marker {
                DefaultMarker::Exact(exact) => default == exact,
                DefaultMarker::Prefix(prefix) => default.starts_with(prefix),
            })
    }
}

/// What the connection form says a blank field means.
#[derive(Clone, Copy, Debug)]
pub struct Hints {
    pub name: &'static str,
    pub host: &'static str,
    /// What a blank database connects to (`ConnectionConfig::database_name`)
    /// where the engine has one; elsewhere it leaves the server to pick.
    pub database: &'static str,
    pub user: &'static str,
    pub path: &'static str,
}

impl Hints {
    pub const DEFAULT: Self = Self {
        name: "Named after the database",
        host: "localhost",
        database: "Optional",
        user: "Required",
        path: "/path/to/app.sqlite",
    };
}

#[derive(Clone, Copy, Debug)]
pub struct SqlSpec {
    pub identifier_quote: IdentifierQuote,
    /// Whether a backslash in a single-quoted literal escapes the character
    /// after it, so a literal one has to survive as two. Postgres reads none
    /// because `standard_conforming_strings` is on by default, and SQLite has
    /// no such notion at all.
    pub backslash_escapes: bool,
    /// String literals are written `N'…'`, because a bare one is converted to
    /// the database's code page first, and a character it lacks arrives as `?`.
    pub national_strings: bool,
    /// Whether a date literal may carry a UTC offset.
    pub literal_offsets: bool,
    /// How a boolean is spelled as an Import Rows value. MySQL reads `'true'`
    /// into a `tinyint` as an error in strict mode, and SQL Server's `bit` and
    /// SQLite's integers take `1` and `0` only.
    pub boolean_literal: BooleanLiteral,
    pub params: Params,
    pub paging: Paging,
    pub select_top: SelectTop,
    pub truncate: Truncate,
    pub foreign_table: ForeignTable,
    pub count_all: CountAll,
    /// The word that opens a transaction.
    pub transaction: Transaction,
    /// One submission is already one transaction, so a generated batch
    /// needs no brackets.
    pub implicit_transaction: bool,
    pub skip_duplicates: SkipDuplicates,
    /// The markers the structure queries write into a column's default for
    /// one the server fills itself.
    pub generated_defaults: &'static [DefaultMarker],
    pub statement_split: StatementSplit,
    pub explain_plan: Option<ExplainPrefix>,
    pub explain_analyze: Option<ExplainPrefix>,
    pub typed_explain: TypedExplain,
    pub parser_dialect: ParserDialect,
    pub format_dialect: FormatDialect,
    pub regex: RegexMatch,
    pub substring: SubstringMatch,
    pub read_only_session: ReadOnlySession,
    /// Nothing outlives one request, so no statement leaves state behind.
    pub stateless_requests: bool,
    /// Functions whose effect the server-side Read-only hold does not stop, so
    /// the verdict has to: a call to one needs more than Read-only wherever it
    /// sits in the statement. A deny-list of named functions, not an allow-list
    /// of safe ones -- a function that is not here, a user-defined one included,
    /// stays a read. Lower-case, matched against the name as written.
    pub denied_functions: &'static [&'static str],
    /// Session settings that change nothing a Read-only profile protects, so
    /// setting one stays a read: navigation and comfort. Every other setting a
    /// user's own `SET` names is stopped, because the hold's whole value is
    /// being hard to remove and each dialect has more settings than anyone has
    /// listed. Lower-case, matched case-insensitively against the name once its
    /// `@@` prefix and scope part are gone.
    pub plain_settings: &'static [&'static str],
}

impl SqlSpec {
    pub const ANSI: Self = Self {
        identifier_quote: IdentifierQuote::Double,
        backslash_escapes: false,
        national_strings: false,
        literal_offsets: true,
        boolean_literal: BooleanLiteral::TrueFalse,
        params: Params::Question,
        paging: Paging::LimitOffset,
        select_top: SelectTop::Limit,
        truncate: Truncate::Truncate,
        foreign_table: ForeignTable::Foreign,
        count_all: CountAll::Count,
        transaction: Transaction::Begin,
        implicit_transaction: false,
        skip_duplicates: SkipDuplicates::None,
        generated_defaults: &[],
        statement_split: StatementSplit::Semicolon,
        explain_plan: None,
        explain_analyze: None,
        typed_explain: TypedExplain::None,
        parser_dialect: ParserDialect::Generic,
        format_dialect: FormatDialect::Generic,
        regex: RegexMatch::None,
        substring: SubstringMatch::Like,
        read_only_session: ReadOnlySession::None,
        stateless_requests: false,
        denied_functions: &[],
        plain_settings: &[],
    };

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` where one submission is already atomic.
    pub fn transaction_opening(&self) -> Option<&'static str> {
        (!self.implicit_transaction).then(|| self.transaction.opening())
    }
}

/// No brackets, not even for SQL Server: the grammar every gate in `sql.rs`
/// parses with has none, and the pin does not move. The standard quote names
/// an identifier there under `QUOTED_IDENTIFIER`, which the driver's login
/// turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentifierQuote {
    Double,
    /// MySQL's, which it accepts whether or not `ANSI_QUOTES` is set — a
    /// double quote there is a *string literal*, so quoting a MySQL identifier
    /// the standard way produces a statement that runs and means something
    /// else.
    Backtick,
}

impl IdentifierQuote {
    fn character(self) -> char {
        match self {
            Self::Double => '"',
            Self::Backtick => '`',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BooleanLiteral {
    TrueFalse,
    OneZero,
}

impl BooleanLiteral {
    pub fn spelled(self, value: bool) -> &'static str {
        match (self, value) {
            (Self::TrueFalse, true) => "true",
            (Self::TrueFalse, false) => "false",
            (Self::OneZero, true) => "1",
            (Self::OneZero, false) => "0",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Params {
    Question,
    Dollar,
    AtP,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paging {
    LimitOffset,
    /// Needs an `ORDER BY`, and one that orders nothing lets a parallel plan
    /// repeat or skip rows from one page to the next, so a preview's first
    /// page waits for the structure that names the key.
    OffsetFetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectTop {
    Limit,
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncate {
    Truncate,
    /// SQLite has no `TRUNCATE`.
    DeleteFrom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignTable {
    Foreign,
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountAll {
    Count,
    /// T-SQL's `COUNT` is an `int` and fails past 2,147,483,647 rows; every
    /// other engine's is already 64-bit.
    CountBig,
}

impl CountAll {
    pub fn aggregate(self) -> &'static str {
        match self {
            Self::Count => "COUNT(*)",
            Self::CountBig => "COUNT_BIG(*)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transaction {
    /// MySQL's own spelling is `START TRANSACTION`, and `BEGIN` is its
    /// documented alias outside a stored program. The alias is what dbdelve
    /// writes because the brackets go into the statement text, where
    /// `sql::is_generated_write` has to read them back: the tree-sitter
    /// grammar has no `START TRANSACTION`, so the gate would refuse dbdelve's
    /// own batch.
    Begin,
    BeginTransaction,
}

impl Transaction {
    pub fn opening(self) -> &'static str {
        match self {
            Self::Begin => "BEGIN",
            Self::BeginTransaction => "BEGIN TRANSACTION",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipDuplicates {
    /// SQL Server and Snowflake have no such clause short of `MERGE`, and
    /// MongoDB's `insertMany` would stop at the first collision.
    None,
    OnConflictDoNothing {
        /// DuckDB refuses `ON CONFLICT DO NOTHING` on a table with no primary
        /// key, unique constraint or unique index, where the others take it as
        /// a clause with nothing to do.
        needs_unique_key: bool,
    },
    OnDuplicateKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultMarker {
    Exact(&'static str),
    Prefix(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementSplit {
    Semicolon,
    /// T-SQL batches separated by `GO` lines.
    Batches,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainPrefix {
    Explain,
    ExplainAnalyze,
    /// `ANALYZE <statement>` runs the statement, so `sql::classify` reads it as
    /// the statement it runs.
    Analyze,
    ExplainQueryPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedExplain {
    /// Turso and DuckDB have no Explain at all, and Snowflake's and SQL
    /// Server's plans are shapes the parser does not read.
    None,
    Postgres,
    MySql,
    Sqlite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserDialect {
    Generic,
    PostgreSql,
    MySql,
    Sqlite,
    DuckDb,
    Snowflake,
    MsSql,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatDialect {
    Generic,
    SqlServer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexMatch {
    /// The filter bar does not offer the operator. SQLite ships no `REGEXP`
    /// implementation, so the operator is a syntax error until an application
    /// registers the function (spec §7); SQL Server has none before 2025's
    /// `REGEXP_LIKE`.
    None,
    Tilde,
    RegexpLike,
    RegexpInstr,
    RegexpMatches,
    RegexpCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstringMatch {
    Like,
    LikeBracketEscape,
    Instr,
    /// DuckDB, unlike SQLite, reads no other type as text on its own: a date
    /// or a number has no `instr`.
    InstrCast,
    Contains,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnlySession {
    None,
    DefaultTransaction,
    SessionTransaction,
}

#[derive(Clone, Copy, Debug)]
pub struct Abilities {
    pub edits: bool,
    pub inserts: bool,
    /// A SQL view can take an insert (an updatable view, or a trigger behind
    /// it), so the server is left to say; a MongoDB view never does.
    pub inserts_into_views: bool,
    pub imports: bool,
    pub assigns_default: bool,
    pub copies_ddl: bool,
    pub switches_database: bool,
    pub switches_database_in_session: bool,
    pub picks_schema: bool,
    pub exact_row_estimates: bool,
    pub schema_switch_waits_for_transaction: bool,
    /// Why no transaction spans an import's chunks.
    pub chunk_refusal: Option<&'static str>,
}

impl Abilities {
    /// The read-only tier.
    pub const NONE: Self = Self {
        edits: false,
        inserts: false,
        inserts_into_views: false,
        imports: false,
        assigns_default: false,
        copies_ddl: false,
        switches_database: false,
        switches_database_in_session: false,
        picks_schema: false,
        exact_row_estimates: false,
        schema_switch_waits_for_transaction: false,
        chunk_refusal: None,
    };
}

/// The answers no strategy expresses, as code.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// A literal for a value bound for a column of `data_type`, or `None` to
    /// fall back to the plain literal.
    pub quote_value: Option<fn(value: &str, data_type: &str) -> Option<String>>,
}

impl Hooks {
    pub const NONE: Self = Self { quote_value: None };
}

pub(crate) const POSTGRES: EngineSpec = EngineSpec {
    id: "postgres",
    label: "Postgres",
    schemes: &["postgres", "postgresql"],
    file_extensions: &[],
    logo: "icons/engine-postgres.svg",
    brand: 0x4169E1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(postgres::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    // `postgres` is an ordinary database people work in.
    system_databases: &[],
    binary_types: &[],
    hints: Hints {
        database: "postgres",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        params: Params::Dollar,
        // One simple-query submission is already one implicit transaction.
        implicit_transaction: true,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        generated_defaults: &[DefaultMarker::Prefix("GENERATED ALWAYS AS ")],
        explain_plan: Some(ExplainPrefix::Explain),
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::Postgres,
        parser_dialect: ParserDialect::PostgreSql,
        regex: RegexMatch::Tilde,
        read_only_session: ReadOnlySession::DefaultTransaction,
        // The armed `default_transaction_read_only` stops none of these, and
        // `set_config` turns it off. Reading a file the role may read is not
        // here: that is a read, and how DBDelve opens its own files.
        //
        // The large-object writers persist and rewrite stored data under the
        // armed hold (reproduced live: `lo_from_bytea`, `lo_put` and
        // `lo_truncate` each took effect). `lo_get` and `loread` are absent
        // because reading an object is a read. The recovery and replication
        // controls (`pg_promote`, `pg_wal_replay_pause`, the replication slot
        // calls) are absent too: they need a superuser or a replication role,
        // none was reproduced against the dev server, and they control
        // recovery rather than stored data. The live differential asks the
        // server's catalog for the large-object family, so a writer a later
        // version adds fails there by name, provided the server it probes is
        // that later version: the family was checked against Postgres 15.13.
        //
        // A function that executes its text argument as a query reaches any
        // listed call through a name the token scan cannot read, because the
        // call sits inside a string: `query_to_xml('select set_config(...)')`
        // released the hold on the dev server, and `query_to_xml('select
        // lo_from_bytea(...)')` wrote a large object with it armed. SPI's
        // read-only mode refuses DML inside that text and refuses none of the
        // listed functions, which is why the carrier works where
        // `query_to_xml('delete ...')` does not. `query_to_xml`,
        // `query_to_xml_and_xmlschema`, `query_to_xmlschema` and `ts_stat`
        // are carriers found so far, because the server's catalog names their
        // text argument `query` or `sqlquery`; the carrier is listed, never
        // the text it carries. The live differential asks the catalog for
        // that shape, so a function a later version adds with such an
        // argument fails there by name.
        //
        // `ts_rewrite(tsquery, text)` is another, and the catalog does not
        // name its text argument: both overloads carry no argument names.
        // Its second argument is run as a query through SPI, so a listed call
        // carried there reaches the server the same way (reproduced live,
        // once releasing the hold and once creating a large object with it
        // armed). The shape probe cannot see it, so an explicit pin in the
        // differential holds it by name. The three-argument
        // `ts_rewrite(tsquery, tsquery, tsquery)` overload runs no text, and
        // the name stops it as well: an over-classification whose direction
        // is an upgrade prompt, on the accepted-false-positive list the
        // product sign-off reads.
        //
        // These are the carriers found so far, not a closed set, and the
        // probe sees only those whose text argument the catalog names. A
        // type-shape probe (every `pg_catalog` function taking a `text`
        // argument the catalog leaves unnamed) was measured against Postgres
        // 15.13 and returned 222 distinct names where this list then held 47;
        // dispositioning that many by hand would be a second deny-list with
        // no evidence behind its entries, so the probe keeps its
        // named-argument query.
        //
        // `tablefunc`'s `crosstab` runs its source query through SPI, so a
        // listed call carried there reaches the server (reproduced on the
        // compose Postgres 15.13 in a scratch database, once releasing the
        // hold and once writing a large object with it armed). It lives in an
        // extension outside `pg_catalog`, so neither catalog probe in
        // `conformance.rs` can see it; `live_extension_carriers_agree_with_the_server`
        // in `src/db/postgres.rs` pins it against the extension's own catalog
        // instead. Listing it means a `crosstab` pivot asks for Read-write on
        // a Read-only profile, an accepted cost recorded in the advisory. The
        // catalog lists were checked against Postgres 15.13 (Debian
        // 15.13-1.pgdg110+1).
        denied_functions: &[
            "set_config",
            "nextval",
            "setval",
            "pg_advisory_lock",
            "pg_advisory_lock_shared",
            "pg_advisory_xact_lock",
            "pg_advisory_xact_lock_shared",
            "pg_try_advisory_lock",
            "pg_try_advisory_lock_shared",
            "pg_try_advisory_xact_lock",
            "pg_try_advisory_xact_lock_shared",
            "pg_advisory_unlock",
            "pg_advisory_unlock_shared",
            "pg_advisory_unlock_all",
            "pg_notify",
            "pg_terminate_backend",
            "pg_cancel_backend",
            "pg_reload_conf",
            "pg_rotate_logfile",
            "pg_switch_wal",
            "pg_create_restore_point",
            "pg_stat_reset",
            "pg_stat_reset_shared",
            "pg_stat_reset_single_table_counters",
            "pg_stat_reset_single_function_counters",
            "pg_stat_reset_slru",
            "pg_stat_reset_replication_slot",
            "lo_create",
            "lo_import",
            "lo_export",
            "lo_unlink",
            "lo_from_bytea",
            "lo_put",
            "lo_creat",
            "lowrite",
            "lo_truncate",
            "lo_truncate64",
            "query_to_xml",
            "query_to_xml_and_xmlschema",
            "query_to_xmlschema",
            "ts_stat",
            "ts_rewrite",
            "crosstab",
            "dblink",
            "dblink_exec",
            "dblink_connect",
            "dblink_connect_u",
            "dblink_send_query",
        ],
        // What a user's own `SET` may change on a Read-only profile without
        // asking. Left off on purpose: the hold's own two names, `role` and
        // `session_authorization`, the switches that turn the server's checks
        // off, and everything that changes how a quote or a backslash is read.
        plain_settings: &[
            "search_path",
            "timezone",
            "statement_timeout",
            "lock_timeout",
            "idle_in_transaction_session_timeout",
            "idle_session_timeout",
            "application_name",
            "extra_float_digits",
            "datestyle",
            "intervalstyle",
            "client_min_messages",
            "bytea_output",
            "lc_monetary",
            "lc_numeric",
            "lc_time",
            "default_text_search_config",
            "work_mem",
            "jit",
            "random_page_cost",
            "max_parallel_workers_per_gather",
            "enable_seqscan",
            "default_transaction_isolation",
            "default_transaction_deferrable",
            "transaction_isolation",
            "xmloption",
        ],
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        picks_schema: true,
        schema_switch_waits_for_transaction: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const MYSQL: EngineSpec = EngineSpec {
    id: "mysql",
    label: "MySQL",
    schemes: &["mysql"],
    file_extensions: &[],
    logo: "icons/engine-mysql.svg",
    brand: 0x4479A1,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mysql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        // Unless `NO_BACKSLASH_ESCAPES` is set, which is not dbdelve's to know
        // or to set.
        backslash_escapes: true,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
        // Since 8.0.18. Server versions are not detected: an older server
        // refuses the statement and says so, which is the error the user needs
        // and is hard rule 6's business rather than a version check's.
        explain_analyze: Some(ExplainPrefix::ExplainAnalyze),
        typed_explain: TypedExplain::MySql,
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpLike,
        read_only_session: ReadOnlySession::SessionTransaction,
        // The armed read-only transaction stops none of the named-lock calls.
        // No sequences, so no `nextval`.
        denied_functions: &["get_lock", "release_lock", "release_all_locks"],
        // Left off on purpose: both spellings of the hold, the server-wide
        // switches, and `sql_mode`, whose `NO_BACKSLASH_ESCAPES` and
        // `ANSI_QUOTES` change how every later statement is lexed and are not
        // stopped by the armed hold.
        plain_settings: &[
            "time_zone",
            "autocommit",
            "max_execution_time",
            "sql_select_limit",
            "wait_timeout",
            "interactive_timeout",
            "lock_wait_timeout",
            "innodb_lock_wait_timeout",
            "net_read_timeout",
            "net_write_timeout",
            "group_concat_max_len",
            "lc_time_names",
            "div_precision_increment",
            "default_week_format",
            "transaction_isolation",
            "sql_big_selects",
            "max_join_size",
        ],
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        // No `picks_schema`: the database is the schema.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const MARIADB: EngineSpec = EngineSpec {
    id: "mariadb",
    label: "MariaDB",
    schemes: &["mariadb"],
    file_extensions: &[],
    logo: "icons/engine-mariadb.svg",
    // The sea lion's colour rather than the wordmark's near-black teal.
    brand: 0xC0765A,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mysql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["information_schema", "mysql", "performance_schema", "sys"],
    binary_types: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        identifier_quote: IdentifierQuote::Backtick,
        backslash_escapes: true,
        // Refused even into a `timestamp`, which MariaDB reads in the
        // session's zone.
        literal_offsets: false,
        boolean_literal: BooleanLiteral::OneZero,
        skip_duplicates: SkipDuplicates::OnDuplicateKey,
        generated_defaults: &[
            DefaultMarker::Exact("AUTO_INCREMENT"),
            DefaultMarker::Prefix("GENERATED ALWAYS AS ("),
        ],
        explain_plan: Some(ExplainPrefix::Explain),
        // MariaDB has no `EXPLAIN ANALYZE` and spells the same thing `ANALYZE`
        // with no `EXPLAIN`.
        explain_analyze: Some(ExplainPrefix::Analyze),
        parser_dialect: ParserDialect::MySql,
        regex: RegexMatch::RegexpInstr,
        read_only_session: ReadOnlySession::SessionTransaction,
        // The armed read-only transaction stops none of the named-lock calls;
        // it does refuse a sequence advance, but a held session is not the
        // boundary.
        denied_functions: &[
            "get_lock",
            "release_lock",
            "release_all_locks",
            "nextval",
            "setval",
        ],
        // The same list as MySQL's with the names MariaDB spells differently:
        // `max_statement_time` for the statement timeout, and `tx_isolation`
        // beside `transaction_isolation`. Its `tx_read_only` is a hold name and
        // stays off, as `transaction_read_only` does.
        plain_settings: &[
            "time_zone",
            "autocommit",
            "max_statement_time",
            "sql_select_limit",
            "wait_timeout",
            "interactive_timeout",
            "lock_wait_timeout",
            "innodb_lock_wait_timeout",
            "net_read_timeout",
            "net_write_timeout",
            "group_concat_max_len",
            "lc_time_names",
            "div_precision_increment",
            "default_week_format",
            "transaction_isolation",
            "tx_isolation",
            "sql_big_selects",
            "max_join_size",
        ],
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SQLITE: EngineSpec = EngineSpec {
    id: "sqlite",
    label: "SQLite",
    schemes: &["sqlite", "sqlite3", "file"],
    file_extensions: &["db", "sqlite", "sqlite3", "db3"],
    logo: "icons/engine-sqlite.svg",
    brand: 0x003B57,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        // Its bare `EXPLAIN` lists bytecode rather than a plan, and no form
        // reports what a run actually cost, so there is no `explain_analyze`:
        // its absence keeps the menu from offering a mode that would only
        // produce an error.
        explain_plan: Some(ExplainPrefix::ExplainQueryPlan),
        typed_explain: TypedExplain::Sqlite,
        parser_dialect: ParserDialect::Sqlite,
        substring: SubstringMatch::Instr,
        // No `generated_defaults`: `table_info` leaves generated columns out
        // altogether.
        // Inert while `rusqlite` is built without its `load_extension`
        // feature, and listed so enabling it cannot quietly open a hole.
        denied_functions: &["load_extension"],
        // No `plain_settings`, deliberately: SQLite has no `SET` statement. It
        // configures through `PRAGMA`, a different statement type that already
        // needs Full, so a `SET` here is one the server rejects and stopping it
        // costs nobody a working statement.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        // No `assigns_default`: `UPDATE` takes an expression on the right and
        // `DEFAULT` is not one there.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const TURSO: EngineSpec = EngineSpec {
    id: "turso",
    label: "Turso",
    schemes: &["turso"],
    file_extensions: &[],
    logo: "icons/engine-turso.svg",
    brand: 0x4FF8D2,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        boolean_literal: BooleanLiteral::OneZero,
        truncate: Truncate::DeleteFrom,
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: false,
        },
        parser_dialect: ParserDialect::Sqlite,
        substring: SubstringMatch::Instr,
        // Inert for the same reason as SQLite's. The rest of Turso's function
        // set has not been surveyed.
        denied_functions: &["load_extension"],
        // No `plain_settings`, for the reason SQLite has none: configuration is
        // `PRAGMA`, which is already Full, and there is no `SET` to allow.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        copies_ddl: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const DUCKDB: EngineSpec = EngineSpec {
    id: "duckdb",
    label: "DuckDB",
    schemes: &["duckdb"],
    file_extensions: &["duckdb", "ddb"],
    logo: "icons/engine-duckdb.svg",
    brand: 0xFFF000,
    syntax: Syntax::Sql,
    layout: Fields::File,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    hints: Hints {
        path: "Blank for a database in memory",
        ..Hints::DEFAULT
    },
    sql: SqlSpec {
        skip_duplicates: SkipDuplicates::OnConflictDoNothing {
            needs_unique_key: true,
        },
        parser_dialect: ParserDialect::DuckDb,
        regex: RegexMatch::RegexpMatches,
        substring: SubstringMatch::InstrCast,
        // No Explain: a plan comes as box-drawn text in one column, a shape
        // `explain.rs` does not read yet. No `read_only_session`: an open
        // database has no session switch; read-only is a way to open the
        // file, which a profile does not ask for yet.
        // Nothing holds a session read-only, so an advance on a sequence runs,
        // and the classifier is the only thing between a statement and its
        // effect. The text executors were observed, not assumed, on the
        // embedded build `SELECT version()` reports as `v2.0.0-dev88614`
        // (crate `duckdb` 1.20000.0), with the sequence counter as the canary:
        // `SELECT * FROM query('select nextval(...)')` moved it from 3 to 4,
        // and `SELECT * FROM json_execute_serialized_sql(json_serialize_sql(
        // 'select nextval(...)'))` moved it from 4 to 5, so both are listed.
        // `query_table` is not: it resolves its argument as a table name and
        // refuses the rest (`query_table('select 1')` answers "Table with name
        // select 1 does not exist"), so it runs no text and listing it would
        // only stop a read. `json_serialize_sql` and `json_deserialize_sql`
        // turn text into JSON and back and run nothing. `duckdb_functions()`
        // shows no other function with an argument named like a query. A
        // listed name followed by a parenthesis is a call and stopped; one
        // after a period is a column reference and a read, except
        // `nextval`, which `MEMBER_FORMS` keeps for Snowflake's spelling and
        // which therefore also stops a column called `t.nextval` here. What
        // is left of the cost of listing `query` is the name used as a CTE
        // name or a derived-table alias with a column list, which is followed
        // by a parenthesis and stopped until the profile is upgraded; both
        // are recorded in the corpus as accepted false positives.
        denied_functions: &["nextval", "setval", "query", "json_execute_serialized_sql"],
        // Names verified in `duckdb_settings()`. The configuration ratchets
        // (`enable_external_access`, `lock_configuration`) are left off on
        // purpose: once set they cannot be undone, and one in a user's buffer
        // would end DBDelve's own CSV, Parquet and Excel opening for the rest
        // of the session. The extension, directory and secret settings are off
        // for the same reason.
        plain_settings: &[
            "threads",
            "memory_limit",
            "max_memory",
            "timezone",
            "calendar",
            "search_path",
            "schema",
            "default_order",
            "default_null_order",
            "preserve_insertion_order",
            "enable_progress_bar",
        ],
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        // The databases attached to the instance the profile opened.
        switches_database: true,
        // Reopening a file detaches everything attached to it, and an
        // in-memory database opened again is a new, empty one.
        switches_database_in_session: true,
        picks_schema: true,
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SNOWFLAKE: EngineSpec = EngineSpec {
    id: "snowflake",
    label: "Snowflake",
    schemes: &["snowflake"],
    file_extensions: &[],
    logo: "icons/engine-snowflake.svg",
    brand: 0x29B5E8,
    syntax: Syntax::Sql,
    layout: Fields::Account,
    default_port: None,
    takes_options: false,
    resolves_srv: false,
    // The account usage share every account has. `SNOWFLAKE_SAMPLE_DATA` is a
    // share too, but one people query.
    system_databases: &["SNOWFLAKE"],
    binary_types: &[],
    hints: Hints {
        host: "Implied by the account",
        database: "Required",
        ..Hints::DEFAULT
    },
    // The standard quote: Snowflake folds an unquoted name to upper case and
    // reads a quoted one exactly, and the catalog reports names as stored --
    // so quoting what the catalog said is always the name it meant.
    sql: SqlSpec {
        // Unconditionally.
        backslash_escapes: true,
        foreign_table: ForeignTable::External,
        parser_dialect: ParserDialect::Snowflake,
        regex: RegexMatch::RegexpCount,
        substring: SubstringMatch::Contains,
        stateless_requests: true,
        // Every statement autocommits unless the submission brackets it, so
        // there is no `implicit_transaction`. No Explain: its plan is a fourth
        // shape `explain.rs` does not read yet. No `read_only_session`: there
        // is no session to set anything on. No `generated_defaults`: an
        // identity takes a supplied value.
        // `seq.nextval` is no call at all, which the scan reads off the period
        // before the name. Procedures are `CALL` statements, already Full.
        // Unlike the Postgres and DuckDB lists, this one was built by documentation:
        // the `SYSTEM$` control entries come from Snowflake's published function
        // reference, and none was run, because no live Snowflake account is
        // reachable from this repository's test environment and the mock in
        // `snowflake/mock.rs` carries no function catalog. There is no read-only
        // session to hold the rest, so on this engine the list is the whole
        // boundary, and it is not an enumerated family: `SYSTEM$` has further
        // members this list does not name. Listing them from documentation would
        // be a long list with no evidence behind any entry, so it stays best-effort
        // in the sense `calls_denied_function` states.
        denied_functions: &[
            "system$cancel_all_queries",
            "system$cancel_query",
            "system$abort_session",
            "system$abort_transaction",
            "nextval",
            "getnextval",
        ],
        // No `plain_settings`, deliberately: nothing a `SET` sets outlives one
        // request on the SQL API, which answers even `USE` with a refusal, so
        // an allow-list would protect nothing. A `SET` here prompts and
        // changes nothing; the one extra arm is cheaper than a special case.
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        picks_schema: true,
        // An exact `ROW_COUNT` per table, where the others' numbers are as of
        // the last time statistics were gathered, and SQLite keeps none.
        exact_row_estimates: true,
        chunk_refusal: Some(
            "Snowflake runs each request on its own, so no transaction can span the chunks.",
        ),
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const SQL_SERVER: EngineSpec = EngineSpec {
    id: "mssql",
    label: "SQL Server",
    schemes: &["mssql", "sqlserver"],
    file_extensions: &[],
    // A generic server glyph, because Simple Icons carries no Microsoft logos.
    logo: "icons/engine-sqlserver.svg",
    brand: 0xCC2927,
    syntax: Syntax::Sql,
    layout: Fields::Server,
    default_port: Some(mssql::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &["master", "tempdb", "model", "msdb"],
    // `image` is the legacy binary type here; elsewhere it is a name anyone
    // may give a column or a domain, and a SQLite `image` column holds text.
    binary_types: &["image", "rowversion", "timestamp"],
    hints: Hints::DEFAULT,
    sql: SqlSpec {
        national_strings: true,
        boolean_literal: BooleanLiteral::OneZero,
        params: Params::AtP,
        paging: Paging::OffsetFetch,
        select_top: SelectTop::Top,
        count_all: CountAll::CountBig,
        // A bare `BEGIN` opens a statement block in T-SQL, not a transaction.
        // The gate's grammar reads this spelling too.
        transaction: Transaction::BeginTransaction,
        // A default constraint's definition always comes wrapped in
        // parentheses, so neither prefix can be one.
        generated_defaults: &[
            DefaultMarker::Prefix("IDENTITY("),
            DefaultMarker::Prefix("AS "),
        ],
        statement_split: StatementSplit::Batches,
        parser_dialect: ParserDialect::MsSql,
        format_dialect: FormatDialect::SqlServer,
        substring: SubstringMatch::LikeBracketEscape,
        // No Explain: there is no prefix form. A plan comes from `SET
        // SHOWPLAN_XML ON`, a session switch that has to be a batch of its
        // own, so Explain would mean changing the session around the user's
        // statement rather than putting a word on a copy of it. No
        // `read_only_session`: no session-level switch exists.
        // `ApplicationIntent=ReadOnly` routes a login to a readable replica
        // and is ignored by a primary.
        // The pass-through table functions run a statement on another server,
        // and nothing holds a session read-only. A scalar function cannot have
        // a side effect in T-SQL, so none is listed.
        denied_functions: &["openquery", "openrowset", "opendatasource"],
        // Names from the engine's own category table. Left off on purpose:
        // `quoted_identifier`, which is read at parse time and changes how a
        // double quote is read, `implicit_transactions`, which DBDelve's
        // auto-commit switch relies on, and `identity_insert`. `set statistics`
        // and `set transaction isolation level` are read from the parse
        // instead, not from here.
        plain_settings: &[
            "nocount",
            "rowcount",
            "textsize",
            "language",
            "dateformat",
            "datefirst",
            "deadlock_priority",
            "lock_timeout",
            "arithabort",
            "arithignore",
            "numeric_roundabort",
            "ansi_nulls",
            "ansi_padding",
            "ansi_warnings",
            "concat_null_yields_null",
            "xact_abort",
        ],
        ..SqlSpec::ANSI
    },
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        imports: true,
        assigns_default: true,
        copies_ddl: true,
        switches_database: true,
        // No `picks_schema`: the default schema is the login's.
        ..Abilities::NONE
    },
    hooks: Hooks {
        quote_value: Some(mssql::typed_literal),
    },
};

pub(crate) const MONGODB: EngineSpec = EngineSpec {
    id: "mongodb",
    label: "MongoDB",
    schemes: &["mongodb", "mongodb+srv"],
    file_extensions: &[],
    logo: "icons/engine-mongodb.svg",
    brand: 0x47A248,
    syntax: Syntax::Mongo,
    layout: Fields::Server,
    default_port: Some(mongo::DEFAULT_PORT),
    takes_options: true,
    resolves_srv: true,
    system_databases: &["admin", "local", "config"],
    binary_types: &["bindata"],
    hints: Hints {
        user: "Optional",
        ..Hints::DEFAULT
    },
    // No `denied_functions` or `plain_settings`: this is not SQL, and its own
    // closed tables decide what a Read-only profile may run.
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        imports: true,
        copies_ddl: true,
        switches_database: true,
        chunk_refusal: Some(
            "MongoDB has no transaction on a standalone server, so each chunk is kept as it goes.",
        ),
        // No `assigns_default`: a document has no column defaults to fall
        // back to.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};

pub(crate) const REDIS: EngineSpec = EngineSpec {
    id: "redis",
    label: "Redis",
    schemes: &["redis", "rediss"],
    file_extensions: &[],
    logo: "icons/engine-redis.svg",
    brand: 0xFF4438,
    syntax: Syntax::Redis,
    layout: Fields::Server,
    default_port: Some(redis::DEFAULT_PORT),
    takes_options: false,
    resolves_srv: false,
    system_databases: &[],
    binary_types: &[],
    hints: Hints {
        name: "Named after the host",
        database: "0",
        user: "Optional",
        ..Hints::DEFAULT
    },
    // No `denied_functions` or `plain_settings`: this is not SQL, and its own
    // closed tables decide what a Read-only profile may run.
    sql: SqlSpec::ANSI,
    abilities: Abilities {
        edits: true,
        inserts: true,
        inserts_into_views: true,
        switches_database: true,
        chunk_refusal: Some(
            "Redis has no transaction that rolls back, so each chunk is kept as it goes.",
        ),
        // No `imports`: its new rows are keys that each name their own value,
        // not rows under a file's columns. No `copies_ddl`: a keyspace is
        // declared nowhere.
        ..Abilities::NONE
    },
    hooks: Hooks::NONE,
};
