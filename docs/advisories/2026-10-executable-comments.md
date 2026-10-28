# Advisory: statements rated as reads could release the Read-only hold

Date: 2026-10-26

Status: DRAFT. Not for publication until the maintainer supplies the affected
version range; see Affected versions.

## Summary

On a profile in **Read-only** mode, DBDelve rated several statements as plain
reads that the server does not treat as reads. The two that need no MariaDB and
no comment:

- A bare `SET` that releases the read-only hold DBDelve arms on the server
  connection, on **Postgres, MySQL and MariaDB**: on Postgres
  `SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE`, on MySQL and MariaDB
  `SET SESSION TRANSACTION READ WRITE` or `SET transaction_read_only = 0`. After
  one of these ran, a write succeeded.
- On **Postgres**, a call to
  `SELECT set_config('default_transaction_read_only', 'off', false)`, which
  releases the same hold from inside a `SELECT`.

A third route needs a MariaDB server: a selection holding a MariaDB executable
comment (`/*M!100100 ... */`) was classified as a plain read and passed the mode
gate, although the server runs the statement inside the comment. A comment
holding `SET SESSION TRANSACTION READ WRITE` released the hold.

This is fixed. The fix and the behaviour changes it causes are described under
Remedy.

## Condition

Each route needed a **Read-only connection mode**, and a statement typed or
pasted into the editor and run.

1. **A bare `SET`, on Postgres, MySQL or MariaDB.** No comment and no
   particular server version: the statement is classified as a read and sent as
   typed.
2. **A function call, on Postgres.** `set_config` with the hold's setting name,
   `'off'` and a third argument of `false` (`true` limits the change to the
   current transaction). The call counts wherever it sits in the statement.
3. **A MariaDB executable comment**, on a MariaDB server, by one of two routes:
   - the comment is **selected on its own**, or after a statement, and Run is
     pressed. A non-empty selection that holds one statement or none is sent to
     the server as selected, on every engine except SQL Server;
   - the comment sits **inside one statement** that is selected, or that the
     cursor is in, such as `SELECT 1 /*M!100100 INTO @y */ FROM t`. The
     comment travels with the statement's text because a token follows it.
     A comment after the last token of a statement is not part of it.

   The server matters, not the engine label on the profile: a MariaDB server
   reached through a profile of the older MySQL engine has the same parser
   dialect, so the classifier read it the same way. The live reproduction used
   the MariaDB engine label; the MySQL label is the same by construction and was
   not run separately.

A MySQL server ignores the MariaDB-only `/*M!` marker, so route 3 does nothing
there. The plain `SET` spellings in route 1 did reach a MySQL server and did
release its hold. The version-gated `/*! ... */` form, which both servers run,
was already read by what it holds before the fix.

## What was observed

Against MariaDB 11.4.13 with the read-only hold armed:

- A comment holding `SET SESSION TRANSACTION READ WRITE` was classified as a
  read, passed the gate, and released the hold. A following `INSERT` then
  succeeded. That takes two runs, the release and then the write. A plain
  `INSERT` is refused by the gate in Read-only mode; inside an executable comment
  it was rated a read, as the snapshot recorded before the fix shows.
- A comment holding a write or a `DROP TABLE` directly, with the hold still
  armed, was **refused by the server** (error 1792). The hold held; what the
  gate missed was the statement that released it.

On Postgres, MySQL and MariaDB, with the hold armed and no comment:

- A plain `SET` that releases the hold was classified as a read. Each server has
  a spelling that does it, and a live test showed each one disarming the hold: a
  write refused a moment before went through after it.
- On Postgres, `SELECT set_config('default_transaction_read_only', 'off', false)`
  was classified as a read and disarmed the hold the same way, shown by a live
  test against Postgres 15.

Also observed:

- Before the fix `classify` rated a `DROP TABLE`, `TRUNCATE TABLE` or an
  unfiltered `DELETE` inside an executable comment as a plain read, and the
  recorded gate verdict was no stop and no confirmation in any mode, so the
  confirmation and the Full-mode requirement for those statements did not
  appear. These two observations were made separately and not as one run through
  the window. The snapshot recorded before the fix shows the verdicts; a live
  test shows the server executing such a comment and dropping the test's own
  table.
- The same class holds other calls whose effect the server keeps: the advisory
  locks, the backend terminate and cancel calls, the notification call, the
  large-object calls, the remote-query calls, the named-lock calls and the
  sequence advance. They were rated plain reads. Not every one was shown to
  defeat the hold. The large-object writers were: on Postgres 15 under the armed
  hold, a large object was written and read back. The armed hold does not stop
  the named-lock calls on MySQL and MariaDB.

## Remedy

The classifier now judges a submission by what the server will run.

- The tokens of each MariaDB executable comment are expanded into the statement
  around them and the result is parsed once, so a comment holding a `DROP` is a
  `DROP`, one holding a release of the hold is stopped on a Read-only profile,
  and a fragment is judged as part of the statement it joins. This is the
  classifier's own reading: the text sent to the server is still exactly what was
  typed. A denied function inside such a comment is stopped as well.
- A submission the statement splitter reads as code and the typed parser reads
  as nothing is refused by name instead of being run as a read. This covers a
  nested block comment on Postgres and SQL Server, whose residue the parser read
  as nothing.
- A `SET` that releases the hold, a transaction opened read-write, a `SET` with
  a server-wide scope and a setting on no per-engine allow-list are stopped on a
  Read-only profile. Navigation and comfort settings, user variables and
  transaction brackets keep running.
- A call to a function on the engine's deny-list is stopped on a Read-only
  profile, wherever it sits in the statement, including a Postgres name spelled
  as a unicode escape. A function that is not on the list, a user-defined one
  included, stays a read.

The verdict changes are recorded in the snapshots under
`src/sql_snapshots/snapshots/` (`hardening_comments.snap` for the executable
comments, `hardening_settings.snap` for `SET`, `hardening_functions.snap` for the
function calls), and a composition test in `src/sql.rs` pins that a selection
reaches the classifier whole and is no longer a plain read.

### What users will see

- A MariaDB executable comment, a nested block comment on Postgres or SQL
  Server, and a `SET` of a setting that is not on the allow-list raise a dialog
  or ask for a higher mode where they ran silently before.
- `SET` needs Read-write on Snowflake, SQLite and Turso, which have no
  allow-list. On the other engines a `SET` of any setting on no per-engine
  allow-list is stopped the same way.
- `SET GLOBAL`, `SET PERSIST` and `SET @@global.x`, and `SET GLOBAL TRANSACTION`
  among them, ask for Read-write.
- A call to a listed function (`set_config`, `nextval`, the advisory locks, the
  large-object writers, the named-lock calls and the rest of the class above)
  asks for Read-write.
- MongoDB `$where`, `$function` and `$accumulator` need Full.
- Redis `CONFIG GET` and `CLIENT LIST` need Read-write: neither writes anything,
  and both return what a Read-only profile should not hand out, such as a
  configured password or the address of every connected client.
- A `mariadb-dump` file starts with a header comment of the form
  `/*M!999999\- ... */`. It is now rated unreadable and raises one run-once
  dialog. This is an accepted false positive and is recorded in the snapshot.
- On MySQL, which ignores the marker, the same comment is over-classified as
  code. That is the safe direction.

Until the fix is installed, do not run a `SET`, a call to `set_config` or
another function in the class above, or a selection containing a `/*M!`
comment, on a Read-only profile.

## Findings made after the first draft

Four further gaps were found by code review and verification after the first
draft of this document. Each is closed in the same release.

- **A Postgres identifier spelled as a unicode escape**, such as
  `SELECT U&"set_c\006fnfig"('default_transaction_read_only', 'off', false)`,
  hid a listed function name from the deny-list. Reproduced live on Postgres 15:
  with the hold armed it read `on`, and `off` after the call. Closed by plan
  01-06.
- **A partial large-object list.** `lo_from_bytea`, `lo_put` and `lo_truncate`
  were not on the deny-list and wrote large objects under the armed hold.
  Reproduced live on Postgres 15, where a large object persisted and read back
  as written; the one created was removed. Closed by plan 01-06.
- **A server-wide transaction scope on MySQL and MariaDB.**
  `SET GLOBAL TRANSACTION READ ONLY` and
  `SET GLOBAL TRANSACTION ISOLATION LEVEL ...` were rated plain reads: the
  parser drops the scope of a `SET ... TRANSACTION`, and a transaction that
  asks for no write access is a read. The server's effect was not reproduced:
  the development account was refused with error 1227 on both servers for want
  of the privilege. The statement does not release the session hold. Closed by
  plan 01-07, task 1, which reads the scope from the tokens.
- **A Postgres function that executes its text argument as a query.**
  `query_to_xml`, `query_to_xml_and_xmlschema`, `query_to_xmlschema`,
  `ts_stat` and `ts_rewrite` take a string and run it, so a listed call carried
  inside that string reached the server through a literal the name scan never
  reads. The verifier ran three statements on the live Postgres 15, each with
  `default_transaction_read_only` set to `on`:
  - `SELECT query_to_xml('select set_config(''default_transaction_read_only'',''off'',false)', true, false, '')`
    followed by `SHOW default_transaction_read_only` answered `on`, then `off`.
  - `SELECT count(*) FROM ts_stat($q$select to_tsvector(set_config('default_transaction_read_only','off',false))$q$)`
    left the setting at `off`.
  - `SELECT query_to_xml('select lo_from_bytea(970973, ''abc''::bytea)', true, false, '')`
    created a large object while the hold was armed. That is a write on a
    Read-only profile, not only a release of the hold: with the hold off
    afterwards, `pg_largeobject_metadata` held one row for it.

  The large object was removed with `lo_unlink` afterwards.

  `ts_rewrite(tsquery, text)` runs its second argument as a query in the same
  way, and the server's catalog gives it no argument names, so the probe built
  to catch a further carrier by the names the catalog records could not see it.
  The verifier ran two statements on the live Postgres 15.13, each with
  `default_transaction_read_only` set to `on`:
  - `SELECT ts_rewrite('a & b'::tsquery, 'select set_config(''default_transaction_read_only'',''off'',false)::tsquery, ''c''::tsquery')`
    followed by `SHOW default_transaction_read_only` answered `off`: the hold
    was released.
  - `SELECT ts_rewrite('a'::tsquery, 'select lo_from_bytea(970981, ''abc''::bytea)::text::tsquery, ''c''::tsquery')`
    created a large object while the hold was armed, with nothing released to
    do it. That is a write on a Read-only profile, not only a release of the
    hold: `pg_largeobject_metadata` held one row for it.

  The large object was removed with `lo_unlink` afterwards. Closed by plan
  01-10, which lists `ts_rewrite`.

  The carriers listed are the ones found so far, and the probe that checks the
  list against the server's catalog sees only those whose text argument the
  catalog names, which is why `ts_rewrite` was missed by it; it is held by name
  instead. Extension carriers such as `tablefunc.crosstab` and
  `xml2.xpath_table` live outside `pg_catalog` and are outside that probe by
  design. Plan 01-08 listed the executors the catalog names; the record of the changed verdicts
  is `hardening_functions` in `src/sql_snapshots/snapshots/`. Plan 01-09 asked
  the same question of DuckDB on the embedded build the tests use
  (`v2.0.0-dev88614`): `query` and `json_execute_serialized_sql` ran their text
  and advanced a sequence, and are listed, while `query_table` resolves a table
  name, runs no text and is not.

The deny-list names functions. A call it does not name stays a read, so the
list is best-effort inside a `SELECT` and not a boundary. Where an engine holds
Read-only on the server, that hold is the boundary, and the list is what stops
the statements that would release it. DuckDB has no such hold, so there the
classifier's verdict is all there is.

## Affected versions

The repository carries no release tags, so the affected range cannot be derived
from it, and none is given here. The status line at the top records that the
maintainer supplies it before this is published. What the history does show:

- The classifier first rated statements by mode in commit `6e89523`
  (2026-09-16), when the manifest read version 0.1.0.
- MariaDB became its own engine in commit `89cf902` (2026-10-04), at manifest
  version 0.2.5. A MariaDB server was reachable before that through the MySQL
  engine.
- The manifest stepped through 0.3.8 (2026-10-15), 0.4.0 (2026-10-20), 0.5.0
  (2026-10-21) and 0.5.1 (2026-10-23).
- The fix is in the tree and is not yet part of a released version.
