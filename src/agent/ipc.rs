//! The socket's directory, its bind, and who is on the other end of it.
//!
//! The directory is the lock and the euid check is the second one: a local
//! process of another user cannot reach a `0700` directory, and if it got
//! through, [`admit`] refuses it before a byte is read. Only a filesystem path
//! is ever bound. The Linux abstract namespace has no permissions, so it is
//! never used.

use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
};

use super::Declined;

/// The effective uid this process runs as.
pub(super) fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// The effective uid of the process on the other end of `stream`.
///
/// std's own call for this is unstable, so the platform's is used: the peer's
/// credentials as the kernel recorded them when the connection was made.
pub(super) fn peer_euid(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `credentials` and `length` outlive the call, and `length` is
        // the size of the buffer the option is written into.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast::<libc::c_void>(),
                &mut length,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        // SAFETY: both out-pointers are to live locals.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Whether the peer of `stream` runs as `expected`. A peer whose credentials
/// cannot be read is not admitted. `expected` is a parameter so a test can
/// give the wrong uid and watch the connection refused.
pub(super) fn admit(stream: &UnixStream, expected: u32) -> bool {
    peer_euid(stream).is_ok_and(|uid| uid == expected)
}

/// Make `dir` exist, be a directory of this user's own, and be reachable by
/// nobody else. A symbolic link, a file, and a directory another user owns are
/// refused; one with group or other bits is tightened.
pub(super) fn secure_dir(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| {
            format!("The agent socket directory {shown} could not be created: {error}.")
        })?;
    let metadata = std::fs::symlink_metadata(dir).map_err(|error| {
        format!("The agent socket directory {shown} could not be read: {error}.")
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "The agent socket directory {shown} is a symbolic link."
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "The agent socket directory {shown} is not a directory."
        ));
    }
    if metadata.uid() != current_euid() {
        return Err(format!(
            "The agent socket directory {shown} is owned by another user."
        ));
    }
    if metadata.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!("The agent socket directory {shown} could not be made private: {error}.")
        })?;
    }
    Ok(())
}

/// Bind the socket at `path`, in a private directory, readable and writable by
/// this user only.
pub(super) fn bind(path: &Path) -> Result<UnixListener, Declined> {
    let shown = path.display();
    let Some(parent) = path.parent() else {
        return Err(Declined::Failed(format!(
            "The agent socket path {shown} has no directory."
        )));
    };
    secure_dir(parent).map_err(Declined::Failed)?;
    let listener = UnixListener::bind(path).map_err(|error| {
        Declined::Failed(format!(
            "The agent socket {shown} could not be bound: {error}."
        ))
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        Declined::Failed(format!(
            "The agent socket {shown} could not be made private: {error}."
        ))
    })?;
    Ok(listener)
}
