//! The app side of the socket: an accept thread, and one thread per
//! connection. No async runtime and nothing from the window: a connection
//! thread sends [`Inbound`] on a channel and blocks until the window answers.
//!
//! Every connection is admitted by its peer's uid before a byte is read, then
//! must open with one frame inside a time and a size. What does not is closed
//! and is neither a call nor a presence.

use std::{
    io::{BufReader, Read, Write},
    net::Shutdown,
    os::unix::{fs::FileTypeExt, fs::MetadataExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use tokio::sync::mpsc::UnboundedSender;

use super::{ConnId, Declined, Inbound, Reply, Status, ipc, tools::Tool, wire};

// ponytail: a first frame is a few bytes written the moment the shim
// connects, so five seconds is patience, not a measurement; a slower peer is
// closed and tries again.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

// ponytail: one agent asks one thing at a time and a person answers each, so
// eight waiting at once is already more than anyone can read; the ninth is
// told so at once. Phase 6 may tune it.
const CALL_CAP: usize = 8;

// ponytail: one presence per attached shim, and an agent runs a handful of
// them; sixteen leaves room without letting a loop hold a thread each forever.
const PRESENCE_CAP: usize = 16;

struct Shared {
    stop: AtomicBool,
    /// Numbers every connection. Calls are numbered from it too.
    next_id: AtomicU64,
    calls: AtomicUsize,
    presences: AtomicUsize,
    first_frame_timeout: Duration,
    /// A handle to every connection that is not a call and has not ended, so
    /// stopping can wake the readers parked on them. A call's stream is never
    /// here: its own thread closes it after its answer, and closing it first
    /// would show the caller an end with no answer.
    open: Mutex<Vec<(ConnId, UnixStream)>>,
}

impl Shared {
    fn open(&self) -> std::sync::MutexGuard<'_, Vec<(ConnId, UnixStream)>> {
        // A thread that panicked holding it left a list that is still a list.
        self.open.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// An entry in [`Shared::open`], removed when it goes out of scope.
struct Registered<'a> {
    shared: &'a Shared,
    conn: ConnId,
}

impl<'a> Registered<'a> {
    fn new(shared: &'a Shared, conn: ConnId, stream: &UnixStream) -> Option<Self> {
        let handle = stream.try_clone().ok()?;
        shared.open().push((conn, handle));
        Some(Self { shared, conn })
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.shared.open().retain(|(conn, _)| *conn != self.conn);
    }
}

/// One of a counted kind of connection, released when it goes out of scope.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    /// A place among at most `cap`, or `None` when they are all taken.
    fn take(counter: &'a AtomicUsize, cap: usize) -> Option<Self> {
        let mut open = counter.load(Ordering::SeqCst);
        loop {
            if open >= cap {
                return None;
            }
            match counter.compare_exchange_weak(open, open + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Self(counter)),
                Err(now) => open = now,
            }
        }
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The listening socket. Dropping it stops the accept thread, wakes every
/// connection that is not waiting on an answer, and removes the socket file.
pub(crate) struct Listener {
    path: PathBuf,
    shared: Arc<Shared>,
}

/// Bind `path` and serve it, sending what arrives on `inbound`.
pub(crate) fn listen(path: &Path, inbound: UnboundedSender<Inbound>) -> Result<Listener, Declined> {
    listen_with(path, inbound, FIRST_FRAME_TIMEOUT)
}

/// [`listen`] with the first-frame wait given, so a test need not sit through
/// the production one.
fn listen_with(
    path: &Path,
    inbound: UnboundedSender<Inbound>,
    first_frame_timeout: Duration,
) -> Result<Listener, Declined> {
    let listener = ipc::bind(path)?;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        calls: AtomicUsize::new(0),
        presences: AtomicUsize::new(0),
        first_frame_timeout,
        open: Mutex::new(Vec::new()),
    });
    let accepting = shared.clone();
    thread::Builder::new()
        .name("dbdelve-agent-accept".to_string())
        .spawn(move || accept(listener, accepting, inbound))
        .map_err(|error| {
            let _ = std::fs::remove_file(path);
            Declined::Failed(format!(
                "The agent socket's accept thread could not start: {error}."
            ))
        })?;
    Ok(Listener {
        path: path.to_path_buf(),
        shared,
    })
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // A listener cannot be closed from another thread, and `accept` does
        // not return for a flag: one connection of our own wakes it, and it
        // sees the flag before it spawns anything for it.
        let _ = UnixStream::connect(&self.path);
        // Only what is still this user's socket: a path a later window bound,
        // or a file that is not ours, is not ours to remove.
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.uid() == ipc::current_euid()
        {
            let _ = std::fs::remove_file(&self.path);
        }
        for (_, stream) in self.shared.open().iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn accept(listener: UnixListener, shared: Arc<Shared>, inbound: UnboundedSender<Inbound>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            // Out of descriptors, most likely: waiting is what lets one close.
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        let shared = shared.clone();
        let inbound = inbound.clone();
        // A connection that cannot get a thread is dropped, which closes it.
        let _ = thread::Builder::new()
            .name("dbdelve-agent-connection".to_string())
            .spawn(move || serve(stream, &shared, &inbound));
    }
}

/// One connection, from its first frame to its end.
fn serve(stream: UnixStream, shared: &Shared, inbound: &UnboundedSender<Inbound>) {
    if !ipc::admit(&stream, ipc::current_euid()) {
        return;
    }
    let conn = ConnId(shared.next_id.fetch_add(1, Ordering::SeqCst));
    let Some(registered) = Registered::new(shared, conn, &stream) else {
        return;
    };
    // Registered before the flag is read: a listener being dropped either
    // sweeps this stream or is seen here.
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    let _ = stream.set_read_timeout(Some(shared.first_frame_timeout));
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let Ok(Some(line)) = wire::read_frame(&mut BufReader::new(reading), wire::FRAME_LIMIT) else {
        return;
    };
    let _ = stream.set_read_timeout(None);
    match wire::decode_hello(&line) {
        Ok(wire::Hello::Call { tool, .. }) => {
            drop(registered);
            call(&stream, shared, tool, inbound);
        }
        Ok(wire::Hello::Presence { .. }) => presence(&stream, conn, shared, inbound),
        Err(_) => {}
    }
}

fn call(stream: &UnixStream, shared: &Shared, tool: Tool, inbound: &UnboundedSender<Inbound>) {
    let Some(_open) = Counted::take(&shared.calls, CALL_CAP) else {
        answer(
            stream,
            &Reply {
                status: Status::RefusedByGate,
                text: format!(
                    "Too many agent calls are waiting in DBDelve; the limit is {CALL_CAP}."
                ),
            },
        );
        return;
    };
    let (reply, answers) = mpsc::sync_channel(1);
    if inbound.send(Inbound::Call { tool, reply }).is_err() {
        return;
    }
    // A dropped sender is a call that ended without an answer: nothing is
    // written, and the stream closing is what the caller sees.
    match answers.recv() {
        Ok(reply) => answer(stream, &reply),
        Err(_) => {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn answer(stream: &UnixStream, reply: &Reply) {
    let mut writer = stream;
    // The caller may have hung up already, and then nobody reads this.
    let _ = writer.write_all(wire::encode(reply).as_bytes());
    let _ = stream.shutdown(Shutdown::Both);
}

/// Hold a presence open until the peer or the listener ends it.
fn presence(
    stream: &UnixStream,
    conn: ConnId,
    shared: &Shared,
    inbound: &UnboundedSender<Inbound>,
) {
    let Some(_open) = Counted::take(&shared.presences, PRESENCE_CAP) else {
        return;
    };
    if inbound.send(Inbound::PresenceOpened { conn }).is_err() {
        return;
    }
    // Anything the peer sends, an end, or an error: all mean it is done.
    let mut reader = stream;
    let _ = reader.read(&mut [0u8; 1]);
    let _ = inbound.send(Inbound::PresenceClosed { conn });
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, sync::mpsc::RecvTimeoutError};

    use super::*;
    use crate::agent::tests::short_socket_path;

    const QUIET: Duration = Duration::from_millis(300);

    /// A listener on a short path, with what it sends on drained by a plain
    /// thread: `blocking_recv` needs no runtime, so no window is involved.
    struct Rig {
        path: PathBuf,
        listener: Option<Listener>,
        inbound: mpsc::Receiver<Inbound>,
    }

    impl Rig {
        fn new(first_frame_timeout: Duration) -> Self {
            let path = short_socket_path();
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let Ok(listener) = listen_with(&path, sender, first_frame_timeout) else {
                panic!("the socket binds");
            };
            let (forward, inbound) = mpsc::channel();
            thread::spawn(move || {
                while let Some(message) = receiver.blocking_recv() {
                    if forward.send(message).is_err() {
                        break;
                    }
                }
            });
            Self {
                path,
                listener: Some(listener),
                inbound,
            }
        }

        fn client(&self) -> UnixStream {
            let stream = UnixStream::connect(&self.path).expect("the socket takes a client");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("the timeout is set");
            stream
        }

        /// Everything the server wrote to `stream` before it closed it. A reset
        /// counts as a close: the server shut a stream with bytes still unread.
        fn closed_with(stream: &mut UnixStream) -> Vec<u8> {
            let mut bytes = Vec::new();
            match stream.read_to_end(&mut bytes) {
                Ok(_) => {}
                Err(error) => assert_eq!(error.kind(), ErrorKind::ConnectionReset),
            }
            bytes
        }

        fn nothing_arrives(&self) {
            assert!(matches!(
                self.inbound.recv_timeout(QUIET),
                Err(RecvTimeoutError::Timeout)
            ));
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.listener.take();
            let _ = std::fs::remove_dir_all(self.path.parent().expect("the path has a directory"));
        }
    }

    #[test]
    fn a_connection_that_sends_no_frame_is_neither_a_call_nor_a_presence() {
        let rig = Rig::new(Duration::from_millis(200));
        let mut idle = rig.client();
        assert!(Rig::closed_with(&mut idle).is_empty());
        for sent in ["\n", "not json\n"] {
            let mut client = rig.client();
            client
                .write_all(sent.as_bytes())
                .expect("the bytes are sent");
            assert!(Rig::closed_with(&mut client).is_empty(), "{sent:?}");
        }
        rig.nothing_arrives();
    }

    #[test]
    fn an_oversized_first_frame_closes_the_connection() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let mut client = rig.client();
        // The server may close before the last byte is written.
        let _ = client.write_all(&vec![b'a'; wire::FRAME_LIMIT as usize + 1]);
        assert!(Rig::closed_with(&mut client).is_empty());
        rig.nothing_arrives();
    }

    #[test]
    fn a_call_over_the_cap_is_refused_by_gate_naming_the_cap() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let call = br#"{"v":1,"kind":"call","tool":"list_connections"}"#;
        let mut held = Vec::new();
        let mut unanswered = Vec::new();
        for _ in 0..CALL_CAP {
            let mut client = rig.client();
            client.write_all(call).expect("the frame is sent");
            client.write_all(b"\n").expect("the frame is ended");
            held.push(client);
            match rig.inbound.recv_timeout(Duration::from_secs(5)) {
                Ok(Inbound::Call { reply, .. }) => unanswered.push(reply),
                _ => panic!("a call within the cap reaches the window"),
            }
        }

        let mut over = rig.client();
        over.write_all(call).expect("the frame is sent");
        over.write_all(b"\n").expect("the frame is ended");
        assert_eq!(
            String::from_utf8(Rig::closed_with(&mut over)).expect("the answer is text"),
            "{\"status\":\"refused_by_gate\",\"text\":\"Too many agent calls are waiting in DBDelve; the limit is 8.\"}\n"
        );

        // A presence is not a call, and it is still taken; the refused call
        // sent nothing on, so the presence is the next thing to arrive.
        let mut presence = rig.client();
        presence
            .write_all(b"{\"v\":1,\"kind\":\"presence\"}\n")
            .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::PresenceOpened { .. })
        ));
        drop((held, unanswered));
    }

    #[test]
    fn a_presence_connection_opens_and_closes_without_counting_as_a_call() {
        let rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let frame = b"{\"v\":1,\"kind\":\"presence\"}\n";
        let mut client = rig.client();
        client.write_all(frame).expect("the frame is sent");
        let Ok(Inbound::PresenceOpened { conn }) = rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the presence opens");
        };
        drop(client);
        let Ok(Inbound::PresenceClosed { conn: closed }) =
            rig.inbound.recv_timeout(Duration::from_secs(5))
        else {
            panic!("the presence closes");
        };
        assert_eq!(closed, conn);

        // More presences than the call cap leave every call a place.
        let mut presences = Vec::new();
        for _ in 0..=CALL_CAP {
            let mut presence = rig.client();
            presence.write_all(frame).expect("the frame is sent");
            presences.push(presence);
            assert!(matches!(
                rig.inbound.recv_timeout(Duration::from_secs(5)),
                Ok(Inbound::PresenceOpened { .. })
            ));
        }
        let mut call = rig.client();
        call.write_all(b"{\"v\":1,\"kind\":\"call\",\"tool\":\"list_connections\"}\n")
            .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::Call { .. })
        ));
    }

    #[test]
    fn stopping_wakes_the_listener_and_removes_the_socket() {
        let mut rig = Rig::new(FIRST_FRAME_TIMEOUT);
        let mut presence = rig.client();
        presence
            .write_all(b"{\"v\":1,\"kind\":\"presence\"}\n")
            .expect("the frame is sent");
        assert!(matches!(
            rig.inbound.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::PresenceOpened { .. })
        ));

        let stopped = std::time::Instant::now();
        rig.listener.take();
        // The channel closes once the accept thread and every connection
        // thread have let go of it.
        loop {
            match rig.inbound.recv_timeout(Duration::from_secs(1)) {
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("the listener did not stop in time"),
            }
        }
        assert!(stopped.elapsed() < Duration::from_secs(1));
        assert!(!rig.path.exists());
        assert!(Rig::closed_with(&mut presence).is_empty());
    }
}
