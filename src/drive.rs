//! The UAT driver: the real window, operated from outside the way a person
//! operates it, so an agent can run a manual UAT checkpoint itself.
//!
//! Built only with the `uat` feature, and listening only when the app is
//! started with `DBDELVE_DRIVE=1`. The window side binds `127.0.0.1` on a
//! port the OS picks and writes it to `drive.port` in the variant's data
//! directory; `dbdelve drive <command>` is the client. One TCP connection is
//! one command and one answer.
//!
//! Input goes through [`Window::dispatch_event`] and
//! [`Window::dispatch_keystroke`], so a click is hit-tested against what was
//! drawn, an element covered by another is not reached, and a chord goes
//! through the real keymap. Reading back uses what the last frame drew: the
//! `debug_selector` bounds GPUI records under its `test-support` feature, and
//! the painted lines of text the `dbdelve patch` in `vendor/gpui-pre` records
//! beside them. Nothing here reaches a database or a gate: every effect is the
//! one the same click or keystroke from a person would have.
//!
//! The socket thread never touches GPUI; it hands each line to a channel that
//! a foreground task drains, as the agent bridge does (`workspace::agent`).

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta,
    ScrollWheelEvent, Window, point, px,
};

// GPUI records `debug_selector` bounds only with debug assertions on, so a
// release build with this feature would answer `ids` with nothing.
#[cfg(not(debug_assertions))]
compile_error!("the `uat` feature is for debug builds only");

const PORT_FILE: &str = "drive.port";
/// How long the client waits for one answer. A window that draws no frame,
/// minimised for one, answers nothing.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

type Request = (String, mpsc::SyncSender<String>);

fn port_path() -> Result<PathBuf, String> {
    Ok(crate::store::dbdelve_directory()?.join(PORT_FILE))
}

/// Start answering driver commands for `window`, when `DBDELVE_DRIVE=1`.
pub(crate) fn serve_if_asked(window: AnyWindowHandle, cx: &mut App) {
    if std::env::var_os("DBDELVE_DRIVE").is_none_or(|value| value != "1") {
        return;
    }
    if let Err(text) = serve(window, cx) {
        // Started on purpose by whoever set the variable, who cannot see it
        // fail any other way.
        eprintln!("UAT driver not started: {text}");
    }
}

fn serve(window: AnyWindowHandle, cx: &mut App) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("Could not listen on 127.0.0.1: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let path = port_path()?;
    std::fs::write(&path, port.to_string())
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let sender = sender.clone();
            std::thread::spawn(move || answer(stream, &sender));
        }
    });
    // Awaits the channel only: no reactor is needed, which GPUI's executor
    // does not have.
    cx.spawn(async move |cx| {
        while let Some((line, reply)) = receiver.recv().await {
            let ran = cx.update_window(window, |_, window, cx| {
                run(&line, reply.clone(), window, cx)
            });
            if ran.is_err() {
                // The window has closed; the client is told rather than left
                // waiting. A client that has gone already makes this send fail,
                // which is nothing to act on.
                let _ = reply.send("error: the window has closed".into());
                break;
            }
        }
    })
    .detach();
    Ok(())
}

/// One connection: read a line, hand it over, write back what comes.
fn answer(stream: TcpStream, sender: &tokio::sync::mpsc::UnboundedSender<Request>) {
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let (reply, answer) = mpsc::sync_channel(1);
    if sender.send((line.trim_end().to_string(), reply)).is_err() {
        return;
    }
    let text = answer
        .recv_timeout(ANSWER_TIMEOUT)
        .unwrap_or_else(|_| "error: the window drew no frame in time".into());
    // The client may have given up; nothing to do about a failed write.
    let _ = (&stream).write_all(text.as_bytes());
}

/// Carry out one command now, then answer once the window has drawn the frame
/// that shows its effect.
fn run(line: &str, reply: mpsc::SyncSender<String>, window: &mut Window, cx: &mut App) {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let (verb, rest) = (verb.to_string(), rest.trim().to_string());
    if let Err(text) = act(&verb, &rest, window, cx) {
        let _ = reply.send(format!("error: {text}"));
        return;
    }
    window.refresh();
    // A callback runs at the start of a frame, before it draws: the second is
    // the one that sees what the first frame drew.
    window.on_next_frame(move |window, _| {
        window.refresh();
        window.on_next_frame(move |window, _| {
            let text = read(&verb, &rest, window).unwrap_or_else(|text| format!("error: {text}"));
            let _ = reply.send(text);
        });
    });
}

fn act(verb: &str, rest: &str, window: &mut Window, cx: &mut App) -> Result<(), String> {
    match verb {
        "ids" | "text" | "shot" | "size" | "frame" => Ok(()),
        "click" => {
            let (target, flags) = split_flags(rest, &["right", "double"]);
            let position = locate(&target, window)?;
            let button = match flags.contains(&"right") {
                true => MouseButton::Right,
                false => MouseButton::Left,
            };
            let clicks = if flags.contains(&"double") { 2 } else { 1 };
            hover(position, window, cx);
            for click_count in 1..=clicks {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers: Modifiers::default(),
                        click_count,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position,
                        modifiers: Modifiers::default(),
                        click_count,
                    }),
                    cx,
                );
            }
            Ok(())
        }
        "move" => {
            hover(locate(rest, window)?, window, cx);
            Ok(())
        }
        "scroll" => {
            let (target, lines) = rest
                .rsplit_once(' ')
                .ok_or("scroll takes a target and a number of lines")?;
            let lines: f32 = lines
                .parse()
                .map_err(|_| format!("`{lines}` is not a number of lines"))?;
            let position = locate(target, window)?;
            hover(position, window, cx);
            window.dispatch_event(
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Lines(point(0., -lines)),
                    ..Default::default()
                }),
                cx,
            );
            Ok(())
        }
        "keys" => {
            for chord in rest.split_whitespace() {
                let keystroke =
                    Keystroke::parse(chord).map_err(|_| format!("`{chord}` is not a keystroke"))?;
                window.dispatch_keystroke(keystroke, cx);
            }
            Ok(())
        }
        "type" => {
            for character in rest.chars() {
                window.dispatch_keystroke(typed(character), cx);
            }
            Ok(())
        }
        _ => Err(format!("`{verb}` is not a driver command")),
    }
}

fn read(verb: &str, rest: &str, window: &mut Window) -> Result<String, String> {
    match verb {
        "ids" => Ok(listing(
            window
                .debug_selectors()
                .iter()
                .filter(|(name, _)| name.contains(rest)),
        )),
        "text" => Ok(listing(
            window
                .debug_text()
                .iter()
                .filter(|(text, _)| text.contains(rest)),
        )),
        "size" => {
            let size = window.viewport_size();
            Ok(format!(
                "{} {} scale {}",
                f32::from(size.width).round(),
                f32::from(size.height).round(),
                window.scale_factor()
            ))
        }
        "shot" => {
            if rest.is_empty() {
                return Err("shot takes the path of the PNG to write".into());
            }
            let image = window
                .render_to_image()
                .map_err(|error| error.to_string())?;
            image
                .save_with_format(rest, image::ImageFormat::Png)
                .map_err(|error| format!("Could not write {rest}: {error}"))?;
            Ok(rest.to_string())
        }
        _ => Ok("ok".into()),
    }
}

/// `x y w h  label` per line, rounded to whole pixels, in paint order: what
/// is drawn later is drawn on top.
fn listing<'a, S: AsRef<str> + 'a>(rows: impl Iterator<Item = &'a (S, Bounds<Pixels>)>) -> String {
    rows.map(|(label, bounds)| {
        format!(
            "{} {} {} {}  {}",
            f32::from(bounds.origin.x).round(),
            f32::from(bounds.origin.y).round(),
            f32::from(bounds.size.width).round(),
            f32::from(bounds.size.height).round(),
            label.as_ref().replace('\n', "\\n")
        )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// `x,y` in window pixels; `text=Label`, the centre of the topmost painted line
/// that reads exactly that, else of the topmost one containing it; or a
/// `debug_selector` name, the centre of the last element drawn under it.
fn locate(target: &str, window: &Window) -> Result<Point<Pixels>, String> {
    let target = target.trim();
    if let Some((x, y)) = target.split_once(',')
        && let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>())
    {
        return Ok(point(px(x), px(y)));
    }
    if let Some(wanted) = target.strip_prefix("text=") {
        let lines = window.debug_text();
        return lines
            .iter()
            .rev()
            .find(|(text, _)| text.as_ref() == wanted)
            .or_else(|| lines.iter().rev().find(|(text, _)| text.contains(wanted)))
            .map(|(_, bounds)| bounds.center())
            .ok_or_else(|| format!("no painted text reads `{wanted}`"));
    }
    window
        .debug_selectors()
        .iter()
        .rev()
        .find(|(name, _)| name == target)
        .map(|(_, bounds)| bounds.center())
        .ok_or_else(|| format!("nothing drawn is named `{target}`"))
}

fn split_flags<'a>(rest: &'a str, known: &[&'a str]) -> (String, Vec<&'a str>) {
    let mut words: Vec<&str> = rest.split(' ').collect();
    let mut flags = Vec::new();
    while let Some(last) = words.last()
        && let Some(flag) = known.iter().find(|flag| *flag == last)
    {
        flags.push(*flag);
        words.pop();
    }
    (words.join(" "), flags)
}

fn hover(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

/// The keystroke that types `character`: text reaches the focused field's
/// input handler, and Enter and Tab go through the keymap as keys.
fn typed(character: char) -> Keystroke {
    let key = match character {
        ' ' => "space".to_string(),
        '\n' => return Keystroke::parse("enter").unwrap_or_default(),
        '\t' => return Keystroke::parse("tab").unwrap_or_default(),
        other => other.to_lowercase().to_string(),
    };
    Keystroke {
        modifiers: Modifiers {
            shift: character.is_uppercase(),
            ..Modifiers::default()
        },
        key,
        key_char: Some(character.to_string()),
    }
}

/// `dbdelve drive <command> [args]`: send one command to the window of this
/// variant and print its answer. `wait <text> [seconds]` is the client's own:
/// it asks for `text` until a painted line contains it.
pub(crate) fn client(args: Vec<String>) -> i32 {
    let Some((verb, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let rest = rest.join(" ");
    let result = match verb.as_str() {
        "wait" => wait(&rest),
        _ => ask(&format!("{verb} {rest}")),
    };
    match result {
        Ok(text) if !text.starts_with("error: ") => {
            if !text.is_empty() {
                println!("{text}");
            }
            0
        }
        Ok(text) => {
            eprintln!("{}", text.trim_start_matches("error: "));
            1
        }
        Err(text) => {
            eprintln!("{text}");
            1
        }
    }
}

const USAGE: &str = "usage: dbdelve drive <command>
  ids [part]              debug_selector names drawn, with x y w h
  text [part]             painted lines of text, with x y w h
  click <target> [right] [double]
  move <target>           hover
  scroll <target> <lines> positive scrolls down
  keys <chord>...         e.g. keys secondary-enter escape
  type <text>             types into the focused field
  shot <file.png>         the window as GPUI rendered it
  size                    window size in pixels and scale
  frame                   wait for the next frame
  wait <text> [seconds]   until a painted line contains <text>
targets: x,y | text=Label | a debug_selector name";

fn ask(line: &str) -> Result<String, String> {
    let path = port_path()?;
    let port = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "No driver port at {}: the window was not started with DBDELVE_DRIVE=1",
            path.display()
        )
    })?;
    let address = format!("127.0.0.1:{}", port.trim());
    let mut stream = TcpStream::connect(&address)
        .map_err(|_| format!("Connection refused: nothing is listening on {address}"))?;
    stream
        .set_read_timeout(Some(ANSWER_TIMEOUT + Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(format!("{}\n", line.trim()).as_bytes())
        .map_err(|error| error.to_string())?;
    let mut text = String::new();
    stream
        .read_to_string(&mut text)
        .map_err(|error| format!("No answer from the window: {error}"))?;
    Ok(text)
}

fn wait(rest: &str) -> Result<String, String> {
    let (wanted, seconds) = match rest.rsplit_once(' ') {
        Some((wanted, seconds)) if seconds.parse::<u64>().is_ok() => {
            (wanted, seconds.parse::<u64>().unwrap_or(10))
        }
        _ => (rest, 10),
    };
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let text = ask(&format!("text {wanted}"))?;
        if text.starts_with("error: ") || !text.is_empty() {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "No painted text contained `{wanted}` within {seconds}s"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
