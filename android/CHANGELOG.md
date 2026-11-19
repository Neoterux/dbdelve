# Changelog

Every user-visible change, newest first. The app shows this file under
**What's new**, so write each line for someone using it, not for someone
reading the code. Add to *Unreleased* with each change; a release renames it
to its version and date.

## Unreleased

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
