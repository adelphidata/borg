//! Connecting to borgd's unix socket.
//!
//! A `sockaddr_un` holds the socket path inline, and the room is small: 104
//! bytes on macOS, 108 on Linux. A path that does not fit makes `connect`
//! fail with `InvalidInput` before any syscall. That happens in practice when
//! the data dir is reached through a symlink chain (a relocated home, a
//! worktree under a deep checkout) whose real location is short.
//! [`connect`] retries once through the resolved path so those setups work.

use std::io;
use std::path::Path;
use tokio::net::UnixStream;

/// Connect to the unix socket at `path`, retrying through its canonical form
/// when `path` is too long for a socket address.
///
/// The retry fires only on `InvalidInput`, the too-long signal. Every other
/// failure (no socket, refused, permission) is returned as is, and so is the
/// original error when the canonical path is no shorter or cannot be resolved,
/// since that is the error that names the real problem.
pub async fn connect(path: &Path) -> io::Result<UnixStream> {
    match UnixStream::connect(path).await {
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
            let Ok(canonical) = std::fs::canonicalize(path) else {
                return Err(e);
            };
            if canonical.as_os_str().len() >= path.as_os_str().len() {
                return Err(e);
            }
            UnixStream::connect(&canonical).await
        }
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A socket in a short directory, plus a symlink chain that reaches the
    /// same directory through a path longer than any `sun_path`.
    fn short_socket_behind_long_alias() -> (tempfile::TempDir, tokio::net::UnixListener, PathBuf) {
        // `/tmp` rather than the default temp dir: on macOS the default is a
        // long `/var/folders/...` path that would not fit a socket either.
        let dir = tempfile::Builder::new()
            .prefix("uds")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let listener = tokio::net::UnixListener::bind(dir.path().join("s")).expect("bind");

        let mut alias = dir.path().to_path_buf();
        for i in 0..6 {
            let link = alias.join(format!("a_long_symlink_segment_number_{i}"));
            std::os::unix::fs::symlink(dir.path(), &link).expect("symlink");
            alias = link;
        }
        let long = alias.join("s");
        assert!(long.as_os_str().len() > 110, "alias must overflow sun_path");
        (dir, listener, long)
    }

    #[tokio::test]
    async fn connects_through_a_symlinked_path_too_long_for_a_socket_address() {
        let (_dir, listener, long) = short_socket_behind_long_alias();

        // Control: the plain connect really does reject this path.
        let plain = UnixStream::connect(&long)
            .await
            .expect_err("path is too long");
        assert_eq!(plain.kind(), io::ErrorKind::InvalidInput);

        let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        connect(&long).await.expect("connect via canonical path");
        accept.await.expect("join").expect("accept");
    }

    #[tokio::test]
    async fn a_missing_socket_reports_the_original_error() {
        // A too-long path that resolves nowhere must not be masked by the
        // retry: the caller needs the too-long error, not a confusing
        // not-found for a path it never gave.
        let long = PathBuf::from("/tmp").join("x".repeat(200));
        let err = connect(&long).await.expect_err("nothing to connect to");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
