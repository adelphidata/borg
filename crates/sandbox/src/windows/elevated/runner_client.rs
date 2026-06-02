//! Operator-side Tier 2 session driver.
//!
//! Runs inside the trampoline (operator identity). Creates a
//! sandbox-account-scoped named pipe, launches the runner *as the
//! sandbox account* via `CreateProcessWithLogonW`, validates the
//! connecting client PID, ships the [`SpawnRequest`], and relays the
//! child's stdio for the session's lifetime, returning the child's exit
//! code.
//!
//! See [`super`] for the privilege rationale (`CreateProcessWithLogonW`
//! needs only the account password, not caller privileges).

use std::io;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessWithLogonW, GetCurrentProcessId, LOGON_WITH_PROFILE, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use super::ipc_runner::{Message, OutputStream};
use super::runner_pipe::{frame_to_io, send_frame, FrameWriter, OwnedPipe, RawPipe};
use super::{named_pipe, setup};
use crate::windows::ipc::SpawnRequest;
use crate::windows::TrampolineInvocation;

use crate::windows::wide_nul as wide;

const SDDL_REVISION_1: u32 = 1;
const ERROR_PIPE_CONNECTED: u32 = 535;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Outcome classification for a Tier 2 session failure.
///
/// The trampoline falls back to Tier 1 on failure, but that is only safe
/// *before* the runner has launched the wrapped child — re-running a
/// non-idempotent command (e.g. `git push`) under Tier 1 would
/// double-execute it. We therefore tag every failure with whether the
/// child had already been reported spawned.
pub enum SessionError {
    /// Failure before the runner reported a spawned child (pipe/launch/
    /// connect/auth error, or a runner spawn error that arrived before
    /// `SpawnReady`). No child ran, so Tier 1 fallback is safe.
    PreSpawn(io::Error),
    /// Failure after the child was launched (`SpawnReady` seen, then a
    /// relay/exit error). The child ran — falling back would
    /// double-execute, so the error must propagate.
    PostSpawn(io::Error),
}

impl SessionError {
    /// True if the trampoline may safely fall back to Tier 1.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, SessionError::PreSpawn(_))
    }

    /// The underlying error, for propagation/logging.
    pub fn into_inner(self) -> io::Error {
        match self {
            SessionError::PreSpawn(e) | SessionError::PostSpawn(e) => e,
        }
    }
}

/// Drive a full Tier 2 sandboxed session and return the child's exit
/// code. `runner_subcommand` is the hidden subcommand the runner binary
/// dispatches on (see [`crate::windows::RUNNER_SUBCOMMAND`]).
///
/// All failures up to and including a relay error before `SpawnReady`
/// are [`SessionError::PreSpawn`] (safe to fall back); anything after the
/// child is reported launched is [`SessionError::PostSpawn`].
pub fn run_session(
    self_exe: &Path,
    runner_subcommand: &str,
    account_name: &str,
    account_sid: &str,
    invocation: &TrampolineInvocation,
) -> Result<u32, SessionError> {
    let password = load_account_password(account_name).map_err(SessionError::PreSpawn)?;

    // SAFETY: pure value read.
    let pid = unsafe { GetCurrentProcessId() };
    let pipe_name = named_pipe::pipe_name(pid);

    // 1. Create the pipe granting the sandbox account access.
    let server = create_control_server(&pipe_name, account_sid).map_err(SessionError::PreSpawn)?;

    // 2. Launch the runner as the sandbox account.
    let runner_pid = launch_runner(
        self_exe,
        runner_subcommand,
        account_name,
        &password,
        &pipe_name,
    )
    .map_err(SessionError::PreSpawn)?;

    // 3. Wait for it to connect and authenticate the connecting PID.
    connect_server(&server).map_err(SessionError::PreSpawn)?;
    let client_pid = client_process_id(&server).map_err(SessionError::PreSpawn)?;
    if client_pid != runner_pid {
        return Err(SessionError::PreSpawn(io::Error::other(format!(
            "runner pipe client PID {client_pid} != launched runner PID {runner_pid}"
        ))));
    }

    // 4. Ship the spawn request. A send failure leaves the runner with
    //    an incomplete frame, so it never spawns — still PreSpawn.
    let writer: FrameWriter = Arc::new(Mutex::new(server.raw()));
    let request = SpawnRequest {
        policy: invocation.policy.clone(),
        tool_dir: invocation.tool_dir.clone(),
        program: invocation.program.clone(),
        args: invocation.args.clone(),
    };
    send_frame(&writer, &Message::SpawnRequest(Box::new(request)))
        .map_err(SessionError::PreSpawn)?;

    // 5. Forward operator stdin → Stdin frames (detached; reaped at
    //    exit). `writer` isn't used again after this, so move it in.
    let stdin_writer = writer;
    let _stdin = thread::spawn(move || forward_stdin(&stdin_writer));

    // 6. Relay child output until Exit/Error. relay_until_exit classifies
    //    its own errors by whether `SpawnReady` was seen.
    let mut reader = server.raw();
    relay_until_exit(&mut reader)
}

/// Read inbound frames, writing child output to the operator's real
/// stdout/stderr, until the runner reports `Exit` or `Error`. Tracks
/// whether `SpawnReady` arrived so failures are correctly classified as
/// pre- or post-spawn (see [`SessionError`]).
fn relay_until_exit(reader: &mut RawPipe) -> Result<u32, SessionError> {
    use io::Write as _;
    // SAFETY: documented console constants.
    let mut real_out = RawPipe(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) });
    let mut real_err = RawPipe(unsafe { GetStdHandle(STD_ERROR_HANDLE) });
    let mut spawned = false;
    let classify = |spawned: bool, e: io::Error| {
        if spawned {
            SessionError::PostSpawn(e)
        } else {
            SessionError::PreSpawn(e)
        }
    };
    loop {
        let frame =
            super::ipc_runner::read_frame(reader).map_err(|e| classify(spawned, frame_to_io(e)))?;
        match frame {
            Message::SpawnReady { .. } => spawned = true,
            Message::Output { stream, data } => {
                let sink = match stream {
                    OutputStream::Stdout => &mut real_out,
                    OutputStream::Stderr => &mut real_err,
                };
                // Best-effort: a write failure to the operator's own
                // terminal shouldn't abort the session.
                let _ = sink.write_all(&data);
            }
            Message::Exit { code } => return Ok(code),
            Message::Error { message } => {
                // A runner error before SpawnReady means the child never
                // launched → recoverable; after means it did → not.
                return Err(classify(
                    spawned,
                    io::Error::other(format!("runner error: {message}")),
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_spawn_errors_are_recoverable_post_spawn_are_not() {
        // Spec: the trampoline must fall back to Tier 1 ONLY for failures
        // before the child launched. A PostSpawn fallback would
        // double-execute a non-idempotent command — the exact bug this
        // classification exists to prevent.
        let pre = SessionError::PreSpawn(io::Error::other("x"));
        let post = SessionError::PostSpawn(io::Error::other("y"));
        assert!(pre.is_recoverable(), "pre-spawn must be recoverable");
        assert!(!post.is_recoverable(), "post-spawn must NOT be recoverable");
    }
}

/// Forward operator stdin into `Stdin` frames until EOF, then send
/// `CloseStdin`.
fn forward_stdin(writer: &FrameWriter) {
    use io::Read as _;
    // SAFETY: documented console constant.
    let mut stdin = RawPipe(unsafe { GetStdHandle(STD_INPUT_HANDLE) });
    let mut buf = [0u8; 8192];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if send_frame(
                    writer,
                    &Message::Stdin {
                        data: buf[..n].to_vec(),
                    },
                )
                .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = send_frame(writer, &Message::CloseStdin);
}

/// Load + DPAPI-unseal the sandbox account's password.
fn load_account_password(account: &str) -> io::Result<String> {
    let path = setup::cred_path(account)
        .ok_or_else(|| io::Error::other("no home dir to resolve sandbox cred path"))?;
    let sealed = std::fs::read(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "read sealed sandbox credential {}: {e}; run `borg service install --windows-sandbox`",
                path.display()
            ),
        )
    })?;
    let plain = crate::windows::dpapi::unseal(&sealed, &setup::cred_entropy(account))?;
    String::from_utf8(plain).map_err(|_| io::Error::other("sealed credential was not valid UTF-8"))
}

/// Create the named-pipe server, granting full access to the sandbox
/// account SID (so the runner, which runs as that account, can open the
/// pipe) and no one else.
///
/// The operator does NOT need a DACL ACE: it holds the server handle
/// returned by `CreateNamedPipeW` directly with full access, independent
/// of the DACL (which only gates *other* openers). So the DACL is exactly
/// one ACE for the runner account — anyone else is implicitly denied. An
/// `OW`/owner ACE here would be meaningless (no `O:` is set) and, on an
/// elevated token, could widen access to all Administrators.
fn create_control_server(name: &str, account_sid: &str) -> io::Result<OwnedPipe> {
    let sddl = wide(&format!("D:(A;;GA;;;{account_sid})"));
    let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: sddl null-terminated; out-pointer stack-local.
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
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd,
        bInheritHandle: 0,
    };
    let wname = wide(name);
    // SAFETY: wname null-terminated; sa holds a valid SD for the call.
    let handle = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        )
    };
    // SAFETY: psd was allocated by the conversion API (LocalAlloc).
    unsafe {
        LocalFree(psd as HLOCAL);
    }
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedPipe::from_raw(handle))
}

/// Block until a client connects to the pipe.
fn connect_server(server: &OwnedPipe) -> io::Result<()> {
    // SAFETY: server holds a valid pipe handle; no overlapped I/O.
    let ok = unsafe { ConnectNamedPipe(server.raw().0, ptr::null_mut()) };
    if ok != 0 {
        return Ok(());
    }
    // SAFETY: pure value read.
    let err = unsafe { GetLastError() };
    if err == ERROR_PIPE_CONNECTED {
        // A client that connected between CreateNamedPipeW and
        // ConnectNamedPipe is already attached — success.
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(err as i32))
    }
}

/// PID of the connected pipe client (for runner authentication).
fn client_process_id(server: &OwnedPipe) -> io::Result<u32> {
    let mut pid: u32 = 0;
    // SAFETY: valid connected pipe handle; out-param stack-local.
    let ok = unsafe { GetNamedPipeClientProcessId(server.raw().0, &mut pid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Launch the runner binary as `account_name` via
/// `CreateProcessWithLogonW`, returning the runner's PID.
fn launch_runner(
    self_exe: &Path,
    runner_subcommand: &str,
    account_name: &str,
    password: &str,
    pipe_name: &str,
) -> io::Result<u32> {
    // Command line: "<exe>" <runner-subcommand> --pipe <name>
    let cmdline = format!(
        "\"{}\" {} --pipe {}",
        self_exe.display(),
        runner_subcommand,
        pipe_name
    );
    let user = wide(account_name);
    let domain = wide("."); // local machine
    let pass = wide(password);
    let mut cmd = wide(&cmdline);

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: all string buffers are null-terminated and live across the
    // call; cmd is mutable per the Win32 contract; null env/cwd inherit.
    let ok = unsafe {
        CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            pass.as_ptr(),
            LOGON_WITH_PROFILE,
            ptr::null(),
            cmd.as_mut_ptr(),
            CREATE_NO_WINDOW,
            ptr::null(),
            ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let runner_pid = pi.dwProcessId;
    // We track the runner via the pipe + Exit frame, so the process/
    // thread handles aren't needed here.
    // SAFETY: both handles are valid and owned by us.
    unsafe {
        if !pi.hProcess.is_null() {
            CloseHandle(pi.hProcess);
        }
        if !pi.hThread.is_null() {
            CloseHandle(pi.hThread);
        }
    }
    Ok(runner_pid)
}
