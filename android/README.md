# DBDelve for Android

A native Android client for **PostgreSQL, MySQL 5.6.5 or later, and MongoDB**,
built on the same rules as the desktop app: your statement runs exactly as
typed, every statement passes the connection's mode first, and a connection
starts Read-only.

## How it is built

```text
android/
  core/   Rust: the three drivers, the gate, statement splitting (UniFFI)
  app/    Kotlin + Jetpack Compose: the UI, Keystore, the biometric prompt
```

**Kotlin and Compose for everything you touch, Rust for everything that talks
to a database.** Compose is what the platform's keyboard, accessibility,
biometrics and Keystore are built for. The database side is Rust because:

- It runs the desktop's drivers at the desktop's exact versions
  (`tokio-postgres` 0.7.18, `mysql` 28, `mongodb` 3.9.1) and the same
  `sqlparser` 0.63. A statement is classified on a phone exactly as on desktop.
  A Kotlin rewrite of the gate would be a second gate, and two gates disagree.
- MongoDB doesn't support its Java driver on Android, and MySQL Connector/J
  doesn't target Android. Neither does Rust officially, but these Rust drivers
  are plain Rust, with no JVM classes Android lacks.

`core` is its own Cargo workspace, kept out of the desktop crate, so the
desktop build and upstream merges are untouched. Its lockfile was seeded from
the desktop's: `mysql` 28.0.0 has since been yanked from crates.io, and only a
lockfile that already holds it can still resolve it.

`Session::run` is the only way a statement reaches a server, and it classifies
and gates before sending. When the gate stops a statement, the app shows the
statement verbatim and calls `run` again with the user's answer. Kotlin
receives rows as strings and type tags, never a driver type.

## Building

Prerequisites: Android SDK 35 and an NDK, Rust with the Android targets, and
`cargo-ndk`.

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/<version>
cd android
./gradlew :app:assembleDebug        # or open android/ in Android Studio
```

`preBuild` runs `cargo ndk` for `arm64-v8a` and `x86_64` (the emulator), then
generates the Kotlin bindings from the built library itself, so they always
describe the core that is packaged.

## Checks

```sh
cd android/core
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                   # gate, Mongo parser, splitting
cargo test -- --include-ignored              # plus live servers, see below
```

The live tests read `DBDELVE_ANDROID_POSTGRES`, `DBDELVE_ANDROID_MYSQL` and
`DBDELVE_ANDROID_MONGO`, each `host:port:user:password:database`, and skip
any that is unset:

```sh
docker run -d -p 53356:3306 -e MYSQL_ROOT_PASSWORD=pw -e MYSQL_DATABASE=dev mysql:5.6
docker run -d -p 55416:5432 -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=dev postgres:16-alpine
docker run -d -p 57007:27017 mongo:7
DBDELVE_ANDROID_MYSQL=127.0.0.1:53356:root:pw:dev \
DBDELVE_ANDROID_POSTGRES=127.0.0.1:55416:postgres:pw:dev \
DBDELVE_ANDROID_MONGO=127.0.0.1:57007:::dev \
cargo test -- --include-ignored
```

They prove, on each engine, that:

- Read-only refuses a write before sending it.
- The row limit keeps two of three rows and reports that it capped.
- A drop asks for Full and then for confirmation, and a run once leaves the
  mode where it was.
- On Postgres and MySQL, the server's own hold refuses a write the parser
  cannot read when it is run once in Read-only.
- Cancel stops a 30-second sleep.

## MySQL 5.6

- **5.6.5 or later.** That release added `SET SESSION TRANSACTION READ ONLY`,
  which holds a Read-only session on the server. Older servers fail to connect
  with that reason, rather than connecting without the hold.
- **TLS.** The app accepts TLS 1.2 or later only. MySQL 5.6 builds linked
  against yaSSL offer only TLS 1.0 and 1.1, so "Verified" fails and names the
  cause. Either enable TLS 1.2 on the server, or choose TLS "Off" over a
  network you trust (VPN or SSH tunnel). It never falls back to plaintext
  silently.
- No statement timeout is set: `max_execution_time` arrived in 5.7. Cancel
  sends `KILL QUERY` on a side connection.
- Auth is `mysql_native_password`, 5.6's default.

## MongoDB

Commands use the shell's spelling with JSON5 arguments, one per statement:
`db.orders.find({status: 'open'})`, or `db.getCollection('a.b')…` for names
that aren't identifiers. Supported methods: `find`, `aggregate`,
`countDocuments`, `insertOne`/`insertMany`, `updateOne`/`updateMany`,
`deleteOne`/`deleteMany`, `drop`. Types go in Extended JSON:
`{_id: {$oid: '…'}}`.

MongoDB has no server-side read-only hold, so in Read-only the gate is the only
thing that stops a write. Because of that, a command DBDelve cannot read needs
Read-write even to run once. Use a read-only role for anything that matters.

## Not here yet

Deliberate cuts, not oversights; those with a ceiling in the code are marked
`ponytail:` there.

- **Row editing.** The grid is read-only. Desktop's generated `UPDATE` with its
  write gate is the next step.
- **Stopping a capped read.** A read past the limit is drained to its end
  instead of being cancelled at the limit; Cancel still stops it.
- **Column types on Postgres.** The simple protocol sends every value as text
  and doesn't say its type.
- **SSH tunnels.** Desktop shells out to the system `ssh`, which Android
  doesn't have.
- **Background runs.** A long statement can be killed by Android if you leave
  the app. A foreground service is the fix.
- **Command palette, query history, completion, Explain, and the tablet
  layout from the mockup** (`../design/android/dbdelve-android.html`).

## What has and hasn't been compiled

The Rust core is built, linted and tested, including against MySQL 5.6.51,
Postgres 16 and MongoDB 7. The generated Kotlin bindings were compiled on the
JVM and called through JNA against those same servers.

**The Compose app has not been compiled.** The environment it was written in
could not reach Google's Maven repository, so nothing Android-specific could
resolve. Expect to fix small compile errors on the first `assembleDebug`.
