# Changelog

Every user-visible change, newest first. The app shows this file under
**What's new**, so write each line for someone using it, not for someone
reading the code. Add to *Unreleased* with each change; a release renames it
to its version and date.

## Unreleased

### Added

- A query builder beside the SQL editor: choose a table, tap the columns,
  add filters (=, ≠, <, ≤, >, ≥, like, is null, not null), a sort and a
  limit, and see the exact statement before it runs. Run it, open it as SQL
  in a tab, or save it as a saved query whose filter values are asked for at
  each run. From the schema, **Build a query** starts it on that table.
- A tab switcher: the tab count beside the editor opens every query tab as a
  card, with its text and whether it ran, returned an error or hasn't run.

### Changed

- Query tabs are chips that show whether each one ran or failed, and the
  open one stays in view.
- Icons throughout: the bottom bar, the editor's save, saved queries and row
  limit, Grid and Cards in results, copy and share, the kind of each schema
  object, and every action sheet. The bottom bar's Results shows the row
  count, a dot while a statement runs, and ! after an error.
- Cancel is now Stop.
- Each connection shows its engine's logo (PostgreSQL, MySQL, MongoDB), in
  the list and beside its name once connected.

## 0.1.0 (2026-11-19)

The first public build.

### Added

- Connections to PostgreSQL, MySQL 5.6.5 and later, and MongoDB, grouped by
  project, with passwords kept in the Android Keystore.
- Three connection modes: Read-only, Read-write and Full. Every statement is
  checked against the mode before it is sent; going above it asks first, and
  Full asks for the phone's own lock.
- A query editor with syntax colours, query tabs that survive the app being
  closed, and a key row for the symbols a phone keyboard hides.
- Saved queries per connection, with `:name` parameters: running one asks for
  each value (text, number or NULL) and shows the statement it makes first.
- Results as a grid or as one card per row; tap a row for every field, and
  share the results as a CSV file to any app.
- Schema browsing with table sizes and a row preview.
- Users and roles: list them and what they may do; create, change passwords,
  grant, revoke and drop, each shown as the exact statement before it runs.
  Postgres and MySQL passwords are hashed on the phone.
- Swipe between Query, Results, Schema and Users; pull down to refresh;
  actions in sheets within reach of a thumb.
- A read past the row limit is stopped, not downloaded in full.
- Share connections as an encrypted `.dbdelve` file (AES-256-GCM under a
  passphrase), with or without their passwords, through any app.
- Import such a file: after the passphrase, review every connection and change
  its name, host, port, user, database, project, TLS and mode, choose whether
  to keep its password, and decide per connection whether a name already here
  is kept beside it, replaced or skipped. Imported connections start
  Read-only.

### Security

- A statement whose meaning depends on server settings the app cannot see
  (any backslash), and any statement the app cannot read, needs Read-write
  and asks before it runs.
- Server-wide settings (`SET GLOBAL`) need Full.
- TLS certificate checks use patched libraries (RUSTSEC-2026-0381,
  RUSTSEC-2026-0382).
