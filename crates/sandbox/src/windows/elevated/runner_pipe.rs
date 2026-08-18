//! Runner-side named-pipe transport + child broker for Tier 2.
//!
//! Runs inside `borg __windows-sandbox-runner --pipe <name>`, which the
//! operator process launched *as the sandbox account* via
//! `CreateProcessWithLogonW`. This module connects back to the
//! operator's named pipe, receives the [`SpawnRequest`], spawns the
//! wrapped command under the full Tier 1 hardening stack (restricted
//! token + cap-SID DACLs + job object + mitigations + isolated desktop)
//! — now layered on top of the sandbox-account identity — and relays
//! the child's stdio over the pipe using the [`ipc_runner`] protocol.
//!
//! The shared pipe-I/O primitives ([`OwnedPipe`], [`RawPipe`],
//! [`send_frame`]) are also used by the operator-side
//! [`super::runner_client`].

use std::io;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_BROKEN_PIPE, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::TerminateProcess;

use super::ipc_runner::{self, Message, OutputStream};
use crate::windows::acl::apply_for_paths;
use crate::windows::policy_resolve::resolve;
use crate::windows::process::{spawn_with_token, SpawnStdio};
use crate::windows::sid::{generate_random_cap_sid_string, LocalSid};
use crate::windows::token::for_current_with_cap_sid;

const ERROR_PIPE_BUSY: u32 = 231;

/// Owning wrapper for a pipe HANDLE. Closes on drop.
pub struct OwnedPipe(HANDLE);
// SAFETY: a HANDLE is a process-wide kernel reference; moving ownership
// across threads (e.g. the control thread owning the child-stdin write
// end) is well-defined.
unsafe impl Send for OwnedPipe {}

impl OwnedPipe {
    /// Wrap a raw handle, taking ownership.
    pub fn from_raw(h: HANDLE) -> Self {
        Self(h)
    }
    /// A non-owning, `Send`+`Copy` view for use in worker threads and
    /// behind a write mutex. The returned [`RawPipe`] must not outlive
    /// `self`.
    pub fn raw(&self) -> RawPipe {
        RawPipe(self.0)
    }
}

impl Drop for OwnedPipe {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Non-owning pipe handle implementing [`io::Read`] + [`io::Write`] via
/// `ReadFile`/`WriteFile`, so the [`ipc_runner`] framing can drive it.
#[derive(Clone, Copy)]
pub struct RawPipe(pub HANDLE);
// SAFETY: handle value is process-wide; safe to use from any thread.
unsafe impl Send for RawPipe {}

impl io::Read for RawPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n: u32 = 0;
        // SAFETY: self.0 is a valid pipe handle; buf is a valid slice.
        let ok = unsafe {
            ReadFile(
                self.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            // A closed write-end surfaces as ERROR_BROKEN_PIPE; map to
            // EOF so framed readers see a clean end-of-stream.
            if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

impl io::Write for RawPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut n: u32 = 0;
        // SAFETY: self.0 is a valid pipe handle; buf is a valid slice.
        let ok = unsafe {
            WriteFile(
                self.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut n,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Shared, mutex-guarded frame writer. Multiple runner threads (the two
/// output drains plus the main thread's `Exit`) serialize their frames
/// through one of these so they never interleave on the wire.
pub type FrameWriter = Arc<Mutex<RawPipe>>;

/// Serialize and send one [`Message`] through a [`FrameWriter`].
pub fn send_frame(w: &FrameWriter, msg: &Message) -> io::Result<()> {
    let mut guard = w
        .lock()
        .map_err(|_| io::Error::other("frame writer mutex poisoned"))?;
    ipc_runner::write_frame(&mut *guard, msg).map_err(frame_to_io)
}

/// Map a framing error to an `io::Error` for the `io::Result`-returning
/// callers.
pub fn frame_to_io(e: ipc_runner::FrameError) -> io::Error {
    match e {
        ipc_runner::FrameError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// Create an anonymous pipe with exactly one end inheritable. When
/// `child_is_read` is true the read end is the child's (stdin); else the
/// write end is the child's (stdout/stderr). Returns `(parent, child)`.
fn anon_pipe(child_is_read: bool) -> io::Result<(OwnedPipe, OwnedPipe)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    let mut read_h: HANDLE = ptr::null_mut();
    let mut write_h: HANDLE = ptr::null_mut();
    // SAFETY: out-pointers stack-local; sa is valid.
    let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, &sa, 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let (child, parent) = if child_is_read {
        (read_h, write_h)
    } else {
        (write_h, read_h)
    };
    // Clear inheritance on the parent's end so the child never inherits
    // the side the runner keeps.
    // SAFETY: parent is a valid handle we own.
    let ok = unsafe { SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0) };
    if ok == 0 {
        let e = io::Error::last_os_error();
        unsafe {
            CloseHandle(read_h);
            CloseHandle(write_h);
        }
        return Err(e);
    }
    Ok((OwnedPipe(parent), OwnedPipe(child)))
}

/// Connect (as client) to the operator's named pipe, retrying briefly
/// while the server is busy. Returns the connected pipe.
pub fn connect_control_client(name: &str) -> io::Result<OwnedPipe> {
    let wname = crate::windows::wide_nul(name);
    for _ in 0..50 {
        // SAFETY: wname null-terminated; null SA; OPEN_EXISTING for a
        // client connect. GENERIC_READ|WRITE for the duplex channel.
        //
        // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION caps what the pipe
        // server may do with our identity. Omitting SQOS lets the server call
        // `ImpersonateNamedPipeClient` and then *act as* this caller; at the
        // identification level it can only read who we are, which is all the
        // peer check needs. Cheap to state explicitly rather than inherit
        // whatever the default turns out to be.
        let h = unsafe {
            CreateFileW(
                wname.as_ptr(),
                0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
                0,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            return Ok(OwnedPipe(h));
        }
        // SAFETY: pure value read.
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY {
            // SAFETY: wname null-terminated; 2s wait for a free instance.
            unsafe {
                WaitNamedPipeW(wname.as_ptr(), 2000);
            }
            continue;
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "runner could not connect to operator pipe",
    ))
}

/// Runner entry point: connect to `pipe_name`, broker one sandboxed
/// child, and return its exit code.
pub fn serve(pipe_name: &str) -> io::Result<u32> {
    let pipe = connect_control_client(pipe_name)?;
    let mut reader = pipe.raw();

    // First frame must be the spawn request.
    let req = match ipc_runner::read_frame(&mut reader).map_err(frame_to_io)? {
        Message::SpawnRequest(req) => *req,
        other => {
            return Err(io::Error::other(format!(
                "runner: expected SpawnRequest, got {other:?}"
            )))
        }
    };

    // Child stdio via anonymous pipes (the runner has no console to
    // inherit — it was launched detached by CreateProcessWithLogonW).
    let (stdin_parent, stdin_child) = anon_pipe(true)?;
    let (stdout_parent, stdout_child) = anon_pipe(false)?;
    let (stderr_parent, stderr_child) = anon_pipe(false)?;

    // Tier 1 hardening, now under the sandbox-account identity: a fresh
    // restricting cap SID, a restricted primary token from the runner's
    // own token, and the policy ACEs applied for the child's lifetime.
    let cap_sid_str = generate_random_cap_sid_string();
    let cap_sid = LocalSid::from_str_sid(&cap_sid_str)
        .map_err(|e| io::Error::other(format!("cap SID resolve failed: {e}")))?;
    let token = for_current_with_cap_sid(&cap_sid)
        .map_err(|e| io::Error::other(format!("restricted token build failed: {e}")))?;
    let aces = resolve(&req.policy);
    let _acl_guard = apply_for_paths(&cap_sid_str, &cap_sid, &aces)
        .map_err(|e| io::Error::other(format!("ACE apply failed: {e}")))?;

    // SAFETY: token + the child-end handles all outlive this call; the
    // child inherits its stdio ends (marked inheritable by anon_pipe).
    let child = unsafe {
        spawn_with_token(
            token.as_handle(),
            &req.program,
            &req.args,
            None,
            None,
            SpawnStdio::Handles {
                stdin: stdin_child.raw().0,
                stdout: stdout_child.raw().0,
                stderr: stderr_child.raw().0,
            },
        )
    }
    .map_err(|e| io::Error::other(format!("spawn failed: {e}")))?;
    // The child now holds its own copies; the runner's child-end handles
    // are no longer needed and must close so EOF propagates correctly.
    drop(stdin_child);
    drop(stdout_child);
    drop(stderr_child);

    let writer: FrameWriter = Arc::new(Mutex::new(pipe.raw()));
    send_frame(&writer, &Message::SpawnReady { pid: child.pid() })?;

    // Drain child stdout/stderr → Output frames.
    let out_writer = writer.clone();
    let out_src = stdout_parent.raw();
    let out_thread = thread::spawn(move || drain(out_src, OutputStream::Stdout, &out_writer));
    let err_writer = writer.clone();
    let err_src = stderr_parent.raw();
    let err_thread = thread::spawn(move || drain(err_src, OutputStream::Stderr, &err_writer));

    // Control thread: sole reader of inbound frames (Stdin / CloseStdin /
    // Terminate). Owns the child-stdin write end so CloseStdin can drop
    // (close) it. Holds the child's process handle (non-owning) so
    // Terminate can kill it.
    let ctrl_reader = pipe.raw();
    let proc = SendHandle(child.process_handle());
    let _ctrl = thread::spawn(move || control_loop(ctrl_reader, stdin_parent, proc));

    // Wait for the child, drain remaining output, report exit.
    let code = child
        .wait_for_exit()
        .map_err(|e| io::Error::other(format!("wait failed: {e}")))?;
    // Drain threads return (); only a panic makes join() Err, and the exit code above already stands.
    let _ = out_thread.join();
    let _ = err_thread.join();
    send_frame(&writer, &Message::Exit { code })?;
    Ok(code)
    // `pipe` drops here, closing the handle; the detached control thread's
    // next read fails and it exits. `_acl_guard` reverts the ACEs.
}

/// Copy child output from `src` into framed `Output` messages until EOF.
fn drain(mut src: RawPipe, stream: OutputStream, writer: &FrameWriter) {
    use io::Read as _;
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let msg = Message::Output {
                    stream,
                    data: buf[..n].to_vec(),
                };
                if send_frame(writer, &msg).is_err() {
                    break;
                }
            }
        }
    }
}

/// Read inbound control frames and act on them. Owns `stdin_parent` (the
/// child's stdin write end) so `CloseStdin` can close it.
fn control_loop(mut reader: RawPipe, mut stdin_parent: OwnedPipe, proc: SendHandle) {
    use io::Write as _;
    let mut have_stdin = true;
    loop {
        match ipc_runner::read_frame(&mut reader) {
            Ok(Message::Stdin { data }) => {
                if have_stdin {
                    let mut w = stdin_parent.raw();
                    if w.write_all(&data).is_err() {
                        have_stdin = false;
                    }
                }
            }
            Ok(Message::CloseStdin) => {
                // Replace with a closed handle so the child sees stdin
                // EOF. Dropping the old OwnedPipe closes its handle.
                stdin_parent = OwnedPipe::from_raw(ptr::null_mut());
                have_stdin = false;
            }
            Ok(Message::Terminate) => {
                // SAFETY: proc.0 is the child's process handle, valid
                // until the SpawnedChild in serve() drops (after this
                // loop is torn down).
                unsafe {
                    TerminateProcess(proc.0, 1);
                }
                break;
            }
            Ok(_) => {} // ignore unexpected inbound variants
            Err(_) => {
                // The operator pipe broke (operator died / session lost).
                // Kill the child so it isn't orphaned and so serve()'s
                // INFINITE wait_for_exit unblocks and the runner exits.
                // (In the normal path the child has already exited by the
                // time the operator drops the pipe, so this is a no-op
                // TerminateProcess on a dead handle.)
                // SAFETY: proc.0 is valid until serve()'s SpawnedChild
                // drops, which happens after this loop is torn down.
                unsafe {
                    TerminateProcess(proc.0, 1);
                }
                break;
            }
        }
    }
}

/// `Send` wrapper for a process HANDLE moved into the control thread.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: a process HANDLE is a process-wide kernel reference; using it
// from another thread to call TerminateProcess is well-defined.
unsafe impl Send for SendHandle {}
