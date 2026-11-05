//! The app side of the socket: an accept thread, and one thread per
//! connection. No async runtime and nothing from the window: a connection
//! thread sends [`Inbound`] on a channel and blocks until the window answers.

use std::{
    io::{BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use tokio::sync::mpsc::UnboundedSender;

use super::{Declined, Inbound, ipc, wire};

// ponytail: a first frame is a few bytes written the moment the shim
// connects, so five seconds is patience, not a measurement; a slower peer is
// closed and tries again.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

struct Shared {
    stop: AtomicBool,
}

/// The listening socket. Dropping it stops the accept thread and removes the
/// socket file.
pub(crate) struct Listener {
    path: PathBuf,
    shared: Arc<Shared>,
}

/// Bind `path` and serve it, sending what arrives on `inbound`.
pub(crate) fn listen(path: &Path, inbound: UnboundedSender<Inbound>) -> Result<Listener, Declined> {
    let listener = ipc::bind(path)?;
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
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
        // sees the flag before it spawns anything.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept(
    listener: std::os::unix::net::UnixListener,
    shared: Arc<Shared>,
    inbound: UnboundedSender<Inbound>,
) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            // Out of descriptors, most likely: waiting is what lets one close.
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        let inbound = inbound.clone();
        // A connection that cannot get a thread is dropped, which closes it.
        let _ = thread::Builder::new()
            .name("dbdelve-agent-connection".to_string())
            .spawn(move || serve(stream, inbound));
    }
}

/// One connection, from its first frame to its answer.
fn serve(stream: UnixStream, inbound: UnboundedSender<Inbound>) {
    if !ipc::admit(&stream, ipc::current_euid()) {
        return;
    }
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_read_timeout(Some(FIRST_FRAME_TIMEOUT));
    let Ok(Some(line)) = wire::read_frame(&mut BufReader::new(reading), wire::FRAME_LIMIT) else {
        return;
    };
    let _ = stream.set_read_timeout(None);
    let Ok(wire::Hello::Call { tool, .. }) = wire::decode_hello(&line) else {
        return;
    };
    let (reply, answer) = mpsc::sync_channel(1);
    if inbound.send(Inbound::Call { tool, reply }).is_err() {
        return;
    }
    // A dropped sender is a call that ended without an answer: nothing is
    // written, and the stream closing is what the caller sees.
    if let Ok(reply) = answer.recv() {
        let mut stream = &stream;
        // The caller may have hung up already, and then nobody reads this.
        let _ = stream.write_all(wire::encode(&reply).as_bytes());
    }
    let _ = stream.shutdown(Shutdown::Both);
}
