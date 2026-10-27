# Advisory: executable comments on a MariaDB server bypassed the Read-only gate

Date: 2026-10-26

## Summary

On a profile in **Read-only** mode connected to a **MariaDB** server, a
selection holding a MariaDB executable comment (`/*M!100100 ... */`) was
classified as a plain read and passed the mode gate, although the server runs
the statement inside the comment. A comment holding
`SET SESSION TRANSACTION READ WRITE` released the read-only hold DBDelve arms on
the server connection, after which a write succeeded.

This is fixed. The fix and the behaviour change it causes are described under
Remedy.

## Condition

All three had to hold:

1. **A MariaDB server.** The server matters, not the engine label on the
   profile: a MariaDB server reached through a profile of the older MySQL engine
   has the same parser dialect, so the classifier read it the same way. The live
   reproduction used the MariaDB engine label; the MySQL label is the same by
   construction and was not run separately.
2. **A Read-only connection mode.**
3. **A submission that carried the comment**, by one of two routes, both of
   which need the comment to have been typed or pasted into the editor:
   - the comment is **selected on its own**, or after a statement, and Run is
     pressed. A non-empty selection that holds one statement or none is sent to
     the server as selected, on every engine except SQL Server;
   - the comment sits **inside one statement** that is selected, or that the
     cursor is in, such as `SELECT 1 /*M!100100 INTO @y */ FROM t`. The
     comment travels with the statement's text because a token follows it.
     A comment after the last token of a statement is not part of it.

A MySQL server is not affected: it ignores the `/*M!` marker. This was observed,
and the construct was not exercised against any other engine.

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

Two further observations, made separately and not as one run through the
window:

- Before the fix `classify` rated a `DROP TABLE`, `TRUNCATE TABLE` or an
  unfiltered `DELETE` inside an executable comment as a plain read, and the
  recorded gate verdict was no stop and no confirmation in any mode, so the
  confirmation and the Full-mode requirement for those statements did not
  appear. The snapshot recorded before the fix shows the verdicts; a live test
  shows the server executing such a comment and dropping the test's own table.
- A plain `SET` that releases the hold, with no comment (on MariaDB
  `SET SESSION TRANSACTION READ WRITE` or `SET transaction_read_only = 0`), was
  also classified as a read. Postgres, MySQL and MariaDB each have a spelling
  that does it, and a live test showed each one disarming the hold. This was a
  separate gap in the same gate, closed in the same release.

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
- A `SET` that releases the hold, a transaction opened read-write and a setting
  on no per-engine allow-list are stopped on a Read-only profile. Navigation and
  comfort settings, user variables and transaction brackets keep running.

The verdict changes are recorded in the snapshots under
`src/sql_snapshots/snapshots/` (`hardening_comments.snap` for the executable
comments, `hardening_settings.snap` for `SET`), and a composition test in
`src/sql.rs` pins that a selection reaches the classifier whole and is no longer
a plain read.

### What users will see

- A MariaDB executable comment, a nested block comment on Postgres or SQL
  Server, and a `SET` of a setting that is not on the allow-list raise a dialog
  or ask for a higher mode where they ran silently before.
- A `mariadb-dump` file starts with a header comment of the form
  `/*M!999999\- ... */`. It is now rated unreadable and raises one run-once
  dialog. This is an accepted false positive and is recorded in the snapshot.
- On MySQL, which ignores the marker, the same comment is over-classified as
  code. That is the safe direction.

Until the fix is installed, do not run a selection containing a `/*M!` comment
on a Read-only MariaDB profile.

## Affected versions

The repository carries no release tags, so the affected range cannot be derived
from it, and none is given here. The maintainer supplies it before this is
published. What the history does show:

- The classifier first rated statements by mode in commit `6e89523`
  (2026-09-16), when the manifest read version 0.1.0.
- MariaDB became its own engine in commit `89cf902` (2026-10-04), at manifest
  version 0.2.5. A MariaDB server was reachable before that through the MySQL
  engine.
- The manifest stepped through 0.3.8 (2026-10-15), 0.4.0 (2026-10-20), 0.5.0
  (2026-10-21) and 0.5.1 (2026-10-23).
- The fix is in the tree and is not yet part of a released version.
