# The UAT driver

A manual UAT checkpoint can be run by an agent: the `uat` build lets a shell
operate the real window the way a person does, and read back what it drew.
The code is `src/drive.rs`; the GPUI side is the `dbdelve patch` in
`vendor/gpui-pre` (`Window::debug_selectors`, `Window::debug_text`).

## What it is, and what it is not

- Input is real input. A click is a `MouseMove`, `MouseDown` and `MouseUp`
  through `Window::dispatch_event`, hit-tested against the last frame, so a
  control covered by another element is not reached. A chord goes through
  `Window::dispatch_keystroke` and the real keymap. Nothing calls a
  `Workspace` method, so every statement still meets the same gates it meets
  from a person.
- Reading back is what the last frame drew: the `debug_selector` names GPUI
  records under `test-support`, and every line of text painted, clipped to its
  content mask. Text drawn by a GPU primitive other than a shaped line (an
  icon) is not in it; take a screenshot for those.
- Screenshots come from GPUI's own renderer (`Window::render_to_image`), so no
  OS screen-recording permission is needed. macOS and Windows have it; Linux's
  wgpu backend answers that it is not implemented.
- Debug builds only: a release build with `uat` fails to compile. It listens on
  `127.0.0.1` only, on a port the OS picks, and only when the app is started
  with `DBDELVE_DRIVE=1`. Nothing else fences it, so it can click anything a
  person can, an agent approval included.

## Running a checkpoint

```bash
docker compose up -d
cargo build --features uat
```

Run it against a home of its own, so the window cannot see the profiles of
the person's own dev variant (which may name production servers) and writes
nothing into them. `HOME` moves the data directory on macOS and Linux;
on Windows set `APPDATA`. Clear `PG*` too, or an environment profile is put in
front.

```bash
export HOME=/tmp/dbdelve-uat DBDELVE_VARIANT=dev
env -u PGHOST -u PGUSER -u PGPASSWORD -u PGDATABASE -u PGPORT \
  DBDELVE_DRIVE=1 ./target/debug/dbdelve &   # in the background
./target/debug/dbdelve drive size            # once drive.port exists
```

The client reads the port from `drive.port` in the variant's data directory,
so it needs the same `HOME` and `DBDELVE_VARIANT` as the window. Each command
is answered once the window has drawn the frame showing its effect.

| Command | Does |
|---|---|
| `ids [part]` | debug selector names drawn, `x y w h  name`, paint order |
| `text [part]` | painted lines of text, `x y w h  text`, paint order |
| `click <target> [right] [double]` | a real click |
| `move <target>` | hover |
| `scroll <target> <lines>` | wheel; positive scrolls down |
| `keys <chord>...` | e.g. `keys secondary-enter`, `keys cmd-, escape` |
| `type <text>` | types into the focused field |
| `shot <file.png>` | the window as rendered, at device pixels |
| `size` | window size in logical pixels, and the scale |
| `frame` | waits for the next frame |
| `wait <text> [seconds]` | polls `text` until a line contains it (default 10s) |

A target is `x,y` in logical pixels, `text=Label` (the topmost painted line
reading exactly that, else containing it), or a `debug_selector` name such as
`tab-chip-<profile>-q0`, `connections-row-0`, `transaction-bound`. Paint order
is stacking order: a later line is on top, which is the one `text=` picks.
Coordinates from `ids`/`text` are logical pixels; a screenshot is that times
`size`'s scale.

Reading only part of the window is a filter on coordinates, e.g. the status
bar: `dbdelve drive text | awk '$2 > 1060'`.

## Habits that keep a run honest

- Confirm effects in the database, not only in the window: `docker compose exec
  -T postgres psql -U dbdelve -d dbdelve_dev -c "select ... from
  pg_stat_activity"` shows which backend ran what and whether it sits `idle in
  transaction`.
- Put back any row a checkpoint changed, and leave no session in a
  transaction: quitting the window (`keys cmd-q`) ends its connections.
- A connection's own access mode defaults to Read-only in the form; a grid edit
  needs Read-write chosen there.
- Record what the window showed, verbatim, in the report; a UAT file's recorded
  results are the person's and are not edited by the run.
