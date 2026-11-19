# DBDelve for Android

A native Android client for **PostgreSQL, MySQL 5.6.5 or later, and MongoDB**,
built on the same rules as the desktop app: your statement runs exactly as
typed, every statement passes the connection's mode first, and a connection
starts Read-only.

## Target

64-bit ARM phones on Android 15.

- **Only `arm64-v8a`, only Android 15 (`minSdk 35`).** That halves the native
  build and drops every compatibility path. It also means no x86_64 emulator
  image: test on a phone.
- **No CPU-specific tuning of the Rust core.** It waits on the network, not
  the CPU, and rustls's crypto already detects and uses the chip's AES and SHA
  instructions at runtime.
- **A foreground service while a statement runs** (`QueryService`), because
  some Android builds reclaim background apps within minutes. For long
  statements, also allow DBDelve to run in the background in the phone's
  battery settings.
- **The mode prompt takes face unlock, fingerprint or PIN.** It accepts weak
  biometrics, and many phones' face unlock is not a strong biometric.

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
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/<version>
cd android
./gradlew :app:assembleDebug        # or open android/ in Android Studio
```

To install on the phone, turn on *Developer options → USB debugging* (tap
*Settings → About phone → Build number* seven times first), connect it, and run
`./gradlew :app:installDebug`, or `adb install app/build/outputs/apk/debug/app-debug.apk`.

`preBuild` runs `cargo ndk` for `arm64-v8a`, then
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

- A user created here with a password hashed on this side can log in, its
  grant shows, its revoke clears it, its drop asks first, and it can no longer
  log in afterwards.
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

## Users and roles

The **Users** tab lists the server's accounts: Postgres roles, MySQL
`user@host` accounts, or the users of the connected MongoDB database. Tap one to
see what it may do and to change it. From there you can create a user, change
its password, grant or revoke privileges on all tables in a schema or database
or on one table, grant or revoke roles (Postgres, MongoDB), turn login off or
on (Postgres), or drop it.

The same rules as everything else apply:

- **You read the exact statement before it runs.** The core builds it from
  validated parts: names quoted, privileges from a fixed list, nothing typed
  spliced in. It is held where the UI cannot edit it, so the reviewed text is
  the text that runs.
- **Every change needs Full.** It goes through the same gate and the same
  biometric "run once" as a typed statement. Dropping a user is named in the
  review, and Run confirms it.
- **Passwords are hashed on the phone for Postgres and MySQL.** Postgres gets a
  SCRAM-SHA-256 verifier (Postgres 10+), MySQL its `mysql_native_password`
  hash (5.6 and 5.7; 8.0 removed `IDENTIFIED BY PASSWORD`). The server, its
  statement log and the review only ever see the hash. Postgres passwords must
  be printable ASCII, so the hash matches the server's own. **MongoDB hashes
  on the server, so its password crosses the connection: use TLS.** The review
  masks it.
- **Names with a backslash are refused.** Whether a backslash escapes depends
  on server settings the app cannot see.
- **Postgres won't drop a role that still holds privileges.** Revoking on a
  whole schema also revokes the schema's `USAGE` that granting added. If a drop
  is still refused, the server's message says what is left.
- **MySQL 5.6 has no roles and no account locking**, and caps user names at 16
  characters. The form shows the limit.

The typed MongoDB commands `db.getUsers()`, `db.createUser({…})`,
`db.updateUser(name, {pwd})`, `db.grantRolesToUser(…)`,
`db.revokeRolesFromUser(…)` and `db.dropUser(name)` also work in the editor,
classified the same way.

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
- **A session that outlives its screen.** The foreground service keeps the
  process alive, but the session belongs to the view model, so a statement
  still ends if Android destroys the activity outright.
- **Command palette, query history, completion, Explain, and the tablet
  layout from the mockup** (`../design/android/dbdelve-android.html`).

## What has and hasn't been compiled

The Rust core is built, linted and tested, including against MySQL 5.6.51,
Postgres 16 and MongoDB 7. The generated Kotlin bindings were compiled on the
JVM and called through JNA against those same servers.

**The Compose app has not been compiled.** The environment it was written in
could not reach Google's Maven repository, so nothing Android-specific could
resolve. Expect to fix small compile errors on the first `assembleDebug`.
