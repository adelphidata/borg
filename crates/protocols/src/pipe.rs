//! borgd's local transport on Windows: a named pipe.
//!
//! Tokio cannot serve a unix socket on Windows, so borgd listens on a named
//! pipe and writes the pipe's name into the file a unix build binds its socket
//! at (`borgd.sock`). Every "is the daemon there?" check that looks at that
//! path keeps working, and each client — the CLI, the desktop app — finds the
//! pipe by reading one line.
//!
//! The first start picks a random name; later starts reuse the name already in
//! the file, so a long-lived client (the desktop app's gRPC channel) reaches a
//! restarted daemon at the same address, as it would a unix socket. If that
//! name is taken by another process, borgd picks a fresh one rather than
//! share it. The pipe's DACL admits only the user that started borgd (and
//! SYSTEM), and remote clients are refused.

use std::io;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use futures_util::Stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_BUSY, HANDLE, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Every borgd pipe name starts with this; anything else in the name file is
/// refused rather than dialed.
const PIPE_PREFIX: &str = r"\\.\pipe\borgd-";

/// How long [`connect`] keeps retrying while every pipe instance is busy.
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// Read the pipe name borgd wrote to `path`.
pub fn read_pipe_name(path: &Path) -> io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let name = raw.trim();
    if !name.starts_with(PIPE_PREFIX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} does not name a borgd pipe", path.display()),
        ));
    }
    Ok(name.to_string())
}

/// Connect to the pipe named in the file at `path`.
///
/// A pipe with every instance mid-handshake answers `ERROR_PIPE_BUSY`; that is
/// retried briefly because the server always has a fresh instance on the way.
pub async fn connect(path: &Path) -> io::Result<NamedPipeClient> {
    let name = read_pipe_name(path)?;
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        match ClientOptions::new().open(&name) {
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < deadline => {}
            other => return other,
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A named-pipe server that hands out one connected instance per client.
pub struct PipeListener {
    name: String,
    security: SecurityDescriptor,
    next: NamedPipeServer,
}

impl PipeListener {
    /// Create the pipe and publish its name at `path`.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let security = SecurityDescriptor::owner_only()?;
        // `first_pipe_instance` fails when any process already owns the name,
        // so a reused name is never shared with a squatter.
        let reused = read_pipe_name(path)
            .ok()
            .and_then(|name| Some((security.create(&name, true).ok()?, name)));
        let (next, name) = match reused {
            Some(found) => found,
            None => {
                let name = format!("{PIPE_PREFIX}{}", nonce_hex());
                (security.create(&name, true)?, name)
            }
        };
        write_name_file(path, &name)?;
        Ok(Self {
            name,
            security,
            next,
        })
    }

    /// The pipe's full `\\.\pipe\...` name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Wait for the next client. A fresh instance is created before the
    /// connected one is returned, so a client never finds the pipe missing.
    pub async fn accept(&mut self) -> io::Result<PipeConnection> {
        let connected = self.next.connect().await;
        let fresh = self.security.create(&self.name, false)?;
        let done = std::mem::replace(&mut self.next, fresh);
        connected.map(|()| PipeConnection(done))
    }

    /// Accepted connections as a stream, for `serve_with_incoming`.
    pub fn incoming(self) -> impl Stream<Item = io::Result<PipeConnection>> {
        futures_util::stream::unfold(self, |mut listener| async move {
            let next = listener.accept().await;
            Some((next, listener))
        })
    }
}

/// One accepted pipe instance.
pub struct PipeConnection(NamedPipeServer);

impl AsyncRead for PipeConnection {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for PipeConnection {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl tonic::transport::server::Connected for PipeConnection {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

/// Write the pipe name next to the rest of the daemon's state, replacing a
/// stale file from a previous run in one step.
fn write_name_file(path: &Path, name: &str) -> io::Result<()> {
    let tmp = path.with_extension("sock.tmp");
    std::fs::write(&tmp, name)?;
    std::fs::rename(&tmp, path)
}

fn nonce_hex() -> String {
    use rand::Rng;
    let bytes: [u8; 12] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A self-relative security descriptor allocated by the SDDL conversion API.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by
// `CreateNamedPipeW`, so sharing the pointer across threads is sound.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Full access for the current user and SYSTEM; everyone else is denied.
    /// `P` stops inherited ACEs from widening it.
    fn owner_only() -> io::Result<Self> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", current_user_sid()?));
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: sddl is NUL-terminated; psd is a stack out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(psd))
    }

    fn create(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        // SAFETY: attrs and the descriptor it points at outlive the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated with LocalAlloc by the conversion API.
        unsafe {
            LocalFree(self.0 as HLOCAL);
        }
    }
}

/// The string SID of the user this process runs as.
fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo-handle needs no close; token is a stack out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = token_user_sid(token);
    // SAFETY: token was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    result
}

fn token_user_sid(token: HANDLE) -> io::Result<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the required size.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len) };
    // u64 storage keeps the TOKEN_USER header suitably aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buf holds at least `len` bytes.
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled buf with a TOKEN_USER.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: sid points into buf, which is alive; out is a stack out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: out is a NUL-terminated wide string from LocalAlloc.
    let s = unsafe {
        let len = (0..).take_while(|&i| *out.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
        LocalFree(out as HLOCAL);
        s
    };
    Ok(s)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_client_reaches_the_listener_through_the_name_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("borgd.sock");
        let mut listener = PipeListener::bind(&path).expect("bind");
        assert_eq!(read_pipe_name(&path).expect("name"), listener.name());

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).await.expect("read");
            conn.write_all(&buf).await.expect("echo");
            // A second client finds the fresh instance.
            listener.accept().await.expect("second accept");
        });

        let mut client = connect(&path).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"ping");
        let _second = connect(&path).await.expect("second connect");
        server.await.expect("join");
    }

    #[test]
    fn a_name_file_that_names_another_pipe_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("borgd.sock");
        std::fs::write(&path, r"\\.\pipe\someone-else").expect("write");
        let err = read_pipe_name(&path).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // Creating a pipe registers it with the tokio reactor, so every pipe test
    // runs on a runtime.
    #[tokio::test]
    async fn a_restart_reuses_the_published_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("borgd.sock");
        let first = PipeListener::bind(&path).expect("bind");
        let name = first.name().to_string();
        drop(first);
        let second = PipeListener::bind(&path).expect("rebind");
        assert_eq!(second.name(), name);
    }

    #[tokio::test]
    async fn a_taken_name_is_replaced_not_shared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("borgd.sock");
        let held = PipeListener::bind(&path).expect("bind");
        let other = PipeListener::bind(&path).expect("second bind");
        assert_ne!(other.name(), held.name());
        assert_eq!(read_pipe_name(&path).expect("name"), other.name());
    }
}
