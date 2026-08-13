//! Windows-only pipe transport for the trampoline's `SpawnRequest`.
//!
//! The policy travels over an anonymous pipe rather than on argv. The
//! parent creates a pipe, writes the framed
//! [`SpawnRequest`](super::ipc::SpawnRequest), marks the read-end
//! inheritable, and embeds the read-end's HANDLE value on the
//! trampoline argv. The child inherits the read-end via
//! `bInheritHandles=TRUE` on `CreateProcessAsUserW` and reads the
//! request before applying any policy.
//!
//! Closing the parent's copy is the responsibility of the
//! [`PipeSession`] returned from [`send`] — it owns the parent's
//! handle and runs `CloseHandle` on Drop. Callers stash the session in
//! `SandboxCommand::_resources`, which keeps it alive until just after
//! the caller's `Command::spawn()` returns. That ordering is what makes
//! the inheritance race-free: the read-end is in the parent's handle
//! table at the moment `CreateProcess` snapshots inheritable handles.
//!
//! # SECURITY
//!
//! This transport defeats *passive* command-line scraping only; it does
//! not stop an active same-session `DuplicateHandle` attacker. The full
//! threat model lives in [`super`] — kept in one place so the two copies
//! cannot drift.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, FALSE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::CreatePipe;

use super::ipc::{deserialize_request, serialize_request, RequestParseError, SpawnRequest};

/// Owning handle to the parent's copy of the pipe read-end. Closed on
/// Drop. Held inside `SandboxCommand::_resources` as a type-erased
/// `Arc<dyn Any + Send + Sync>` so the cross-platform `SandboxCommand`
/// type doesn't have to know about Win32.
pub struct PipeSession {
    read_handle: HANDLE,
}

// SAFETY: Windows HANDLE is a kernel-object reference. CloseHandle is
// the only operation we perform on the contained handle, and it is
// documented as safe to call from any thread. We never read or write
// the handle once stored — the child has its own inherited copy.
unsafe impl Send for PipeSession {}
unsafe impl Sync for PipeSession {}

impl Drop for PipeSession {
    fn drop(&mut self) {
        if !self.read_handle.is_null() && self.read_handle != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle (CreatePipe returned it; we
            // never duplicated or transferred ownership). CloseHandle
            // on an already-inherited handle does NOT affect the
            // child's copy.
            unsafe {
                CloseHandle(self.read_handle);
            }
        }
    }
}

impl PipeSession {
    /// HANDLE value to embed on the trampoline argv. The child reads
    /// from this number after parsing argv.
    pub fn handle_value(&self) -> HANDLE {
        self.read_handle
    }

    /// Decimal-string form of the handle, ready for argv.
    pub fn handle_decimal(&self) -> String {
        // HANDLE is `*mut c_void` which is pointer-sized; the
        // numeric value is what gets passed across process boundaries.
        // `usize` is the kernel's documented serialization type for
        // HANDLE values on Windows.
        (self.read_handle as usize).to_string()
    }
}

/// Set up the pipe and write the framed request. Returns a
/// [`PipeSession`] whose handle should be referenced on the trampoline
/// argv and whose Drop closes the parent's read-end copy after the
/// child has been spawned.
///
/// # Buffer sizing
///
/// The pipe buffer is sized to `MAX_REQUEST_BYTES + 4` so the whole
/// framed payload always fits without WriteFile blocking. A
/// blocking WriteFile here would deadlock: we close the write-end
/// before spawning, so there's no reader yet at write time.
pub fn send(request: &SpawnRequest) -> io::Result<PipeSession> {
    let bytes = serialize_request(request);
    let buf_size: u32 = (super::ipc::MAX_REQUEST_BYTES as u32).saturating_add(8);

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        // bInheritHandle=TRUE on the SA is necessary but not sufficient
        // — we still call SetHandleInformation on the read-end below to
        // make the inherit flag explicit (defense against future Windows
        // versions that interpret SA differently).
        bInheritHandle: TRUE,
    };

    let mut read_h: HANDLE = ptr::null_mut();
    let mut write_h: HANDLE = ptr::null_mut();
    // SAFETY: all pointers refer to local stack data that outlives the
    // call; CreatePipe writes through them. The SA is a documented
    // optional input.
    let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, &sa, buf_size) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    let session = PipeSession {
        read_handle: read_h,
    };
    // write_h must be closed on every exit path — including early
    // errors — or we leak one HANDLE per failed spawn. WriteHandleGuard
    // closes on Drop unless explicitly disarmed on the success path.
    let mut write_guard = WriteHandleGuard { h: write_h };
    // The write-end must NOT be inheritable — only the child should
    // hold a reference to it, and only because we're about to close
    // it in the parent before spawn. Set the flag explicitly.
    set_inherit(write_h, false)?;
    set_inherit(read_h, true)?;

    write_all(write_h, &bytes)?;
    write_guard.disarm();
    close(write_h);
    // session keeps read_h alive; caller embeds handle_decimal() on
    // argv and stashes session in SandboxCommand::_resources.
    Ok(session)
}

/// RAII guard that closes a write-end HANDLE on Drop unless explicitly
/// disarmed. Used in `send` so that early-error paths between
/// CreatePipe and the final write_all don't leak the write-end.
struct WriteHandleGuard {
    h: HANDLE,
}

impl WriteHandleGuard {
    fn disarm(&mut self) {
        self.h = ptr::null_mut();
    }
}

impl Drop for WriteHandleGuard {
    fn drop(&mut self) {
        if !self.h.is_null() && self.h != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle (CreatePipe returned it; the
            // success path zeroes self.h before this Drop runs).
            unsafe {
                CloseHandle(self.h);
            }
        }
    }
}

/// Read a `SpawnRequest` from the inherited pipe read-end. Called by
/// the trampoline runtime in the spawned `borg.exe` after parsing the
/// handle value from argv.
pub fn recv(handle: HANDLE) -> Result<SpawnRequest, RecvError> {
    // Read the 4-byte length prefix.
    let mut header = [0u8; 4];
    read_exact(handle, &mut header)?;
    let declared = u32::from_le_bytes(header) as usize;
    if declared > super::ipc::MAX_REQUEST_BYTES {
        return Err(RecvError::Oversize(declared));
    }
    let mut body = vec![0u8; declared];
    read_exact(handle, &mut body)?;
    // Re-frame to reuse the parser's invariants (length check, body
    // decode). The deserialize call cross-checks declared vs got and
    // surfaces the same error taxonomy as the in-memory test path.
    let mut framed = Vec::with_capacity(4 + declared);
    framed.extend_from_slice(&header);
    framed.extend_from_slice(&body);
    deserialize_request(&framed).map_err(RecvError::Parse)
}

/// Errors reading a `SpawnRequest` from the pipe.
#[derive(Debug, thiserror::Error)]
pub enum RecvError {
    /// `ReadFile` failed at the OS level. Most often EOF (parent
    /// crashed before writing) or pipe closed.
    #[error("pipe ReadFile failed: {0}")]
    Io(#[from] io::Error),
    /// Length prefix exceeded the cap; refused without allocating.
    #[error("framed request length {0} exceeds cap")]
    Oversize(usize),
    /// Body parsed but failed the `SpawnRequest` schema check.
    #[error(transparent)]
    Parse(RequestParseError),
}

fn set_inherit(h: HANDLE, inherit: bool) -> io::Result<()> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: h is a valid handle from CreatePipe. The flag mask and
    // value are documented constants.
    let ok = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, flags) };
    if ok == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_all(h: HANDLE, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut written: u32 = 0;
        // SAFETY: h is a valid pipe write-end. Buffer pointer + length
        // are local stack data that outlives the call.
        let ok: BOOL = unsafe {
            WriteFile(
                h,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "WriteFile reported zero bytes written",
            ));
        }
        buf = &buf[written as usize..];
    }
    Ok(())
}

fn read_exact(h: HANDLE, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut read: u32 = 0;
        // SAFETY: h is a valid pipe read-end. Buffer pointer + length
        // are local stack data that outlives the call.
        let ok: BOOL = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            let err = unsafe { GetLastError() };
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        if read == 0 {
            // EOF before we got the bytes we wanted.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pipe closed mid-read",
            ));
        }
        buf = &mut buf[read as usize..];
    }
    Ok(())
}

fn close(h: HANDLE) {
    if !h.is_null() && h != INVALID_HANDLE_VALUE {
        // SAFETY: h is our owned handle; we are the only caller of
        // CloseHandle on it.
        unsafe {
            CloseHandle(h);
        }
    }
}

/// Parse the handle decimal embedded on the trampoline argv back to
/// a `HANDLE`. Defensive: rejects empty input and obvious overflow.
/// Returning a sentinel error instead of `0` on parse failure means a
/// malformed argv never accidentally points at the standard input
/// handle.
pub fn parse_handle_decimal(s: &str) -> Result<HANDLE, ParseHandleError> {
    if s.is_empty() {
        return Err(ParseHandleError::Empty);
    }
    let n: usize = s.parse().map_err(|_| ParseHandleError::NotANumber)?;
    Ok(n as HANDLE)
}

/// Errors parsing an argv-encoded HANDLE.
#[derive(Debug, thiserror::Error)]
pub enum ParseHandleError {
    /// Empty string passed where a handle was expected.
    #[error("handle argument was empty")]
    Empty,
    /// Argument did not parse as a non-negative integer.
    #[error("handle argument was not a non-negative integer")]
    NotANumber,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxPolicy;
    use std::path::PathBuf;

    fn sample_request() -> SpawnRequest {
        SpawnRequest {
            policy: SandboxPolicy {
                fs_read: vec!["C:\\tmp".into()],
                ..Default::default()
            },
            tool_dir: PathBuf::from("C:\\tools"),
            program: "cmd.exe".into(),
            args: vec!["/c".into(), "echo hi".into()],
        }
    }

    #[test]
    fn parse_handle_decimal_round_trips_realistic_values() {
        // Spec: HANDLE values are pointer-sized integers. The argv
        // encoding must preserve the value losslessly for any value
        // CreatePipe might realistically produce.
        for v in [4_usize, 1024, 0x7fff_ffff, 0xdead_beef] {
            let s = v.to_string();
            let h = parse_handle_decimal(&s).expect("parse");
            assert_eq!(h as usize, v, "round-trip failed for {v}");
        }
    }

    #[test]
    fn parse_handle_decimal_rejects_empty() {
        assert!(matches!(
            parse_handle_decimal(""),
            Err(ParseHandleError::Empty)
        ));
    }

    #[test]
    fn parse_handle_decimal_rejects_non_numeric() {
        // Spec: a non-numeric arg must error, not silently parse to 0
        // (which is a valid HANDLE value — the standard input handle).
        // Falling back to 0 would mean the trampoline reads its own
        // stdin instead of the policy.
        assert!(matches!(
            parse_handle_decimal("not-a-number"),
            Err(ParseHandleError::NotANumber)
        ));
        assert!(matches!(
            parse_handle_decimal("-1"),
            Err(ParseHandleError::NotANumber)
        ));
        assert!(matches!(
            parse_handle_decimal("0x1234"),
            Err(ParseHandleError::NotANumber)
        ));
    }

    #[test]
    fn send_writes_framed_request_and_recv_reads_it_back() {
        // Spec: end-to-end pipe round-trip. send creates the pipe and
        // pushes the request bytes; recv (driven against the parent's
        // own read-handle for test purposes, mimicking what the child
        // would do via inherited handle) gets the same SpawnRequest
        // back. This validates the framing wire format under real
        // CreatePipe/Write/Read semantics, not just the in-memory
        // serialize/deserialize.
        let req = sample_request();
        let session = send(&req).expect("send must succeed");
        let parsed = recv(session.handle_value()).expect("recv must succeed");
        assert_eq!(parsed.policy.fs_read, req.policy.fs_read);
        assert_eq!(parsed.tool_dir, req.tool_dir);
        assert_eq!(parsed.program, req.program);
        assert_eq!(parsed.args, req.args);
        drop(session); // explicit: closes read-end.
    }

    #[test]
    fn recv_on_closed_pipe_returns_unexpected_eof() {
        // Spec: if the parent crashed before writing the header, the
        // trampoline must see UnexpectedEof, not a JSON parse error.
        // We simulate by creating + immediately closing both ends.
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: FALSE,
        };
        let mut read_h: HANDLE = ptr::null_mut();
        let mut write_h: HANDLE = ptr::null_mut();
        // SAFETY: stack-local out params; CreatePipe contract met.
        let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, &sa, 4096) };
        assert_eq!(ok, TRUE);
        close(write_h); // signal EOF immediately.
        match recv(read_h) {
            Err(RecvError::Io(e)) => {
                assert_eq!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof,
                    "expected UnexpectedEof, got {e:?}"
                );
            }
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
        close(read_h);
    }
}
