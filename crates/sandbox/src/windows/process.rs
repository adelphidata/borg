//! `CreateProcessAsUserW` wrapper for the Tier 1 Windows sandbox.
//!
//! Spawns a child process under the supplied restricted token,
//! inheriting the trampoline's stdio so the wrapped command's output
//! flows through to the user's terminal without a userspace relay.
//!
//! Returns a [`SpawnedChild`] handle that wraps the process + thread
//! handles and exposes a synchronous `wait_for_exit` for the
//! trampoline to call before unwinding (so the AclGuard cleanup runs
//! after the child has exited and dropped its access references).

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, TRUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use super::conpty::{install_pseudoconsole, OwnedPseudoConsole};
use super::desktop::{create_isolated as create_isolated_desktop, OwnedDesktop};
use super::mitigation::{default_mitigation_flags, install_default_mitigations, AttributeList};

/// How the spawned child's standard streams are wired.
///
/// `Copy` so it can be inspected (`matches!`) and consumed in the same
/// function without an explicit clone — every variant is either a
/// fieldless marker or holds only `Copy` handle/reference values.
#[derive(Clone, Copy)]
pub enum SpawnStdio<'a> {
    /// Inherit the trampoline's own std handles. Tier 1 non-interactive
    /// path — output flows straight to the operator's terminal/pipe.
    Inherit,
    /// Use caller-supplied handles (the Tier 2 runner's anonymous-pipe
    /// ends). The handles MUST already be marked inheritable by the
    /// caller, since the child inherits them via `bInheritHandles=TRUE`.
    Handles {
        /// Child's stdin source.
        stdin: HANDLE,
        /// Child's stdout sink.
        stdout: HANDLE,
        /// Child's stderr sink.
        stderr: HANDLE,
    },
    /// Drive stdio through a pseudo-console (interactive / ConPTY). The
    /// child's stdio is the PTY, so `STARTF_USESTDHANDLES` is omitted and
    /// `bInheritHandles` is `FALSE` — the pseudo-console attribute is the
    /// only stdio wiring.
    Pty(&'a OwnedPseudoConsole),
}

/// Owning handle to a spawned process + its main thread + the
/// containing job object. Dropping closes all three handles. With
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set on the job (see
/// [`spawn_with_token`]), the kernel terminates every process inside
/// the job the moment the parent's last handle to the job closes —
/// so a crash of the trampoline reliably kills the wrapped child
/// instead of leaving an orphan running under a restricted token
/// that no longer has a supervisor.
pub struct SpawnedChild {
    process: HANDLE,
    thread: HANDLE,
    job: HANDLE,
    /// Isolated desktop the child runs on. Held so it outlives the
    /// child — `CloseDesktop` would fail on a desktop still in use.
    /// Drop order is process/thread/job first, then desktop, which
    /// matches the kernel's lifetime requirements (handles to the
    /// dead process can close any time; the desktop closes after the
    /// last thread bound to it exits).
    #[allow(dead_code)]
    desktop: OwnedDesktop,
    pid: u32,
}

impl SpawnedChild {
    /// Process ID of the spawned child.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Borrowed process HANDLE. Used by the Tier 2 runner's control
    /// thread to `TerminateProcess` on a `Terminate` frame. The handle
    /// remains owned by this `SpawnedChild`; the caller must not close
    /// it and must not use it after this `SpawnedChild` drops.
    pub fn process_handle(&self) -> HANDLE {
        self.process
    }

    /// Block until the child exits and return its exit code.
    ///
    /// Uses `INFINITE` wait — the trampoline already inherits the
    /// parent's job-object lifetime, so an external kill propagates.
    /// We do NOT add a timeout here because the wrapped command's
    /// timeout policy lives in the parent's tokio supervisor, not in
    /// the trampoline.
    pub fn wait_for_exit(&self) -> io::Result<u32> {
        // SAFETY: self.process is a valid handle owned by us. INFINITE
        // is a documented valid timeout. WaitForSingleObject is safe
        // to call from any thread.
        let rc = unsafe { WaitForSingleObject(self.process, INFINITE) };
        if rc != WAIT_OBJECT_0 {
            return Err(io::Error::other(format!(
                "WaitForSingleObject returned 0x{rc:x} (expected WAIT_OBJECT_0)"
            )));
        }
        let mut code: u32 = 0;
        // SAFETY: Wait returned WAIT_OBJECT_0 so the process has
        // exited; GetExitCodeProcess is now well-defined.
        let ok = unsafe { GetExitCodeProcess(self.process, &mut code) };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        // SAFETY: we own all three handles. Drop order matters when
        // KILL_ON_JOB_CLOSE is in effect: closing the job handle
        // terminates the child. The trampoline reaches Drop only
        // after wait_for_exit returns, by which point the child has
        // already exited — so closing the job handle is the
        // unambiguous "done" signal to the kernel. Closing process /
        // thread handles does NOT terminate the child; it releases
        // our reference and lets the kernel reap the records.
        unsafe {
            if !self.process.is_null() {
                CloseHandle(self.process);
            }
            if !self.thread.is_null() {
                CloseHandle(self.thread);
            }
            if !self.job.is_null() {
                CloseHandle(self.job);
            }
        }
    }
}

/// Build the Win32 command-line string for `program + args`. We
/// include the program in argv[0] (Windows convention) and quote any
/// argument containing spaces, double quotes, or backslashes per the
/// Microsoft "Parsing C Command-Line Arguments" rules.
fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut s = String::new();
    push_quoted(&mut s, program);
    for a in args {
        s.push(' ');
        push_quoted(&mut s, a);
    }
    OsStr::new(&s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn push_quoted(out: &mut String, arg: &str) {
    let needs_quote = arg.is_empty() || arg.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quote {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Each backslash before a quote must be doubled, then
                // escape the quote.
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes before the closing quote also need doubling.
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
}

/// Spawn `program + args` under `token` with the requested `stdio`
/// wiring. Caller passes `cwd` (working directory; null = inherit) and
/// optionally a UTF-16 environment block (null = inherit parent env).
///
/// `stdio` selects the stdio path: inherit the trampoline's handles
/// (Tier 1 non-interactive), use caller-supplied pipe handles (the
/// Tier 2 runner), or drive a pseudo-console (interactive / ConPTY).
///
/// # Safety
///
/// `token` must be a valid Windows token HANDLE owned by the caller
/// for the duration of this call. The function calls
/// `CreateProcessAsUserW`, which dereferences the handle to set up the
/// child process's primary token; an invalid or already-closed handle
/// is undefined behavior at the kernel boundary. For
/// [`SpawnStdio::Handles`] / [`SpawnStdio::Pty`], the supplied handles /
/// pseudo-console must outlive this call.
pub unsafe fn spawn_with_token(
    token: HANDLE,
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    env_block: Option<&[u16]>,
    stdio: SpawnStdio<'_>,
) -> io::Result<SpawnedChild> {
    let mut cmdline = build_command_line(program, args);
    let cwd_w: Option<Vec<u16>> = cwd.map(|p| {
        OsStr::new(p)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });

    // Build the extended startup info. The attribute list carries the
    // mitigation policy always, plus the pseudo-console attribute on the
    // PTY path (hence two slots there). The attribute list and the
    // flags_storage `u64` MUST stay on this stack frame until
    // CreateProcessAsUserW returns — UpdateProcThreadAttribute stores
    // pointers into them, not copies.
    let attr_count = if matches!(stdio, SpawnStdio::Pty(_)) {
        2
    } else {
        1
    };
    let mut attr_list = AttributeList::new(attr_count)?;
    let mitigation_flags: u64 = default_mitigation_flags();
    // SAFETY: mitigation_flags lives on this stack frame until after
    // the CreateProcessAsUserW call below; attr_list outlives it for
    // the same reason. install_default_mitigations stashes the
    // pointer into the kernel-managed attribute list.
    unsafe { install_default_mitigations(&mut attr_list, &mitigation_flags) }?;
    if let SpawnStdio::Pty(pty) = stdio {
        // SAFETY: pty outlives this call (borrowed for the function);
        // the HPCON it installs is consumed by CreateProcessAsUserW.
        unsafe { install_pseudoconsole(&mut attr_list, pty) }?;
    }

    // Isolated desktop. Holds its UTF-16 name buffer alive for the
    // duration of CreateProcessAsUserW (kernel reads STARTUPINFO
    // synchronously, but defensive lifetime-on-stack is the easy win).
    let desktop = create_isolated_desktop()?;

    let mut siex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    siex.StartupInfo.lpDesktop = desktop.lp_desktop();
    siex.lpAttributeList = attr_list.as_ptr();

    // Wire stdio per the requested mode. The PTY path deliberately omits
    // STARTF_USESTDHANDLES: the pseudo-console attribute is the child's
    // stdio, and inheriting handles on top would confuse the console
    // host. Inherit/Handles set explicit std handles and require
    // bInheritHandles=TRUE so the child receives them.
    let inherit_handles = match stdio {
        SpawnStdio::Inherit => {
            // SAFETY: STD_*_HANDLE are documented constants; GetStdHandle
            // returns the trampoline's handles which we want the child
            // to share.
            let (stdin_h, stdout_h, stderr_h) = unsafe {
                (
                    GetStdHandle(STD_INPUT_HANDLE),
                    GetStdHandle(STD_OUTPUT_HANDLE),
                    GetStdHandle(STD_ERROR_HANDLE),
                )
            };
            siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            siex.StartupInfo.hStdInput = stdin_h;
            siex.StartupInfo.hStdOutput = stdout_h;
            siex.StartupInfo.hStdError = stderr_h;
            TRUE
        }
        SpawnStdio::Handles {
            stdin,
            stdout,
            stderr,
        } => {
            siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            siex.StartupInfo.hStdInput = stdin;
            siex.StartupInfo.hStdOutput = stdout;
            siex.StartupInfo.hStdError = stderr;
            TRUE
        }
        SpawnStdio::Pty(_) => {
            // No STARTF_USESTDHANDLES; the pseudo-console attribute
            // installed above is the child's stdio. Per Microsoft's
            // ConPTY guidance, bInheritHandles is FALSE on this path.
            FALSE
        }
    };

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // CREATE_SUSPENDED so we can AssignProcessToJobObject *before* the
    // child runs any code. A child that escapes the job before
    // assignment defeats KILL_ON_JOB_CLOSE — the assignment must
    // race-freely precede ResumeThread.
    //
    // EXTENDED_STARTUPINFO_PRESENT is required for the kernel to
    // honor the attribute list we built above — without it, mitigation
    // policies are silently ignored.
    let mut creation_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
    if env_block.is_some() {
        creation_flags |= CREATE_UNICODE_ENVIRONMENT;
    }
    let env_ptr = match env_block {
        Some(b) => b.as_ptr() as *const core::ffi::c_void,
        None => ptr::null(),
    };
    let cwd_ptr = match &cwd_w {
        Some(v) => v.as_ptr(),
        None => ptr::null(),
    };

    // SAFETY: All pointers refer to local stack data that outlives the
    // call. cmdline is mutable per Win32 docs (the function may
    // tokenize in place). bInheritHandles=TRUE is required to share
    // the std handles with the child. The siex.StartupInfo cast is the
    // documented pattern — STARTUPINFOEXW is layout-compatible at its
    // head with STARTUPINFOW because the latter is the first field.
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            inherit_handles,
            creation_flags,
            env_ptr,
            cwd_ptr,
            &siex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    // From here we must clean up the suspended child on any error path
    // — leaving a suspended child behind would defeat the sandbox.
    let job = match create_kill_on_close_job() {
        Ok(j) => j,
        Err(e) => {
            // SAFETY: pi handles are valid; the suspended child is
            // explicitly torn down via TerminateProcess in the helper.
            unsafe {
                terminate_and_close_suspended_child(pi.hProcess, pi.hThread);
            }
            return Err(e);
        }
    };
    // SAFETY: job and pi.hProcess are both valid handles owned by us.
    let ok = unsafe { AssignProcessToJobObject(job, pi.hProcess) };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        // SAFETY: same as the previous teardown.
        unsafe {
            CloseHandle(job);
            terminate_and_close_suspended_child(pi.hProcess, pi.hThread);
        }
        return Err(err);
    }
    // Now safe to resume — the kernel will enforce job containment.
    // SAFETY: thread handle is valid and the child is in the job.
    let resume_rc = unsafe { ResumeThread(pi.hThread) };
    if resume_rc == u32::MAX {
        let err = io::Error::last_os_error();
        // SAFETY: same as above.
        unsafe {
            CloseHandle(job);
            terminate_and_close_suspended_child(pi.hProcess, pi.hThread);
        }
        return Err(err);
    }

    Ok(SpawnedChild {
        process: pi.hProcess,
        thread: pi.hThread,
        job,
        desktop,
        pid: pi.dwProcessId,
    })
}

/// Create a kill-on-job-close job object. The job is anonymous (no
/// name) and not inheritable (no SA), so only the trampoline holds a
/// handle to it.
///
/// `BREAKAWAY_OK` and `SILENT_BREAKAWAY_OK` are NOT set: children
/// inside the job cannot escape it via `CREATE_BREAKAWAY_FROM_JOB`.
/// Together with the restricted token (which prevents many escape
/// vectors), this is the load-bearing part of "the trampoline crashing
/// reliably kills the wrapped child."
fn create_kill_on_close_job() -> io::Result<HANDLE> {
    // SAFETY: name pointer is null (anonymous job), SA pointer is null
    // (default DACL, not inheritable). CreateJobObjectW returns NULL on
    // failure with GetLastError set.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        ..unsafe { std::mem::zeroed() }
    };
    // Verify at compile time that the two breakaway flags are NOT in
    // our LimitFlags — if a future refactor accidentally OR's them in,
    // this assertion will trip on the very first sandbox spawn.
    debug_assert_eq!(
        info.BasicLimitInformation.LimitFlags
            & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK),
        0,
        "breakaway flags must not be set — they let children escape the job",
    );
    // SAFETY: info is a stack-local fully-initialized struct; size is
    // the actual struct size.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        return Err(err);
    }
    Ok(job)
}

/// Helper to tear down a suspended child when an error happens between
/// `CreateProcessAsUserW` and `ResumeThread`. We must NOT leave the
/// suspended process behind — its restricted token + inherited handles
/// would be a small but real resource leak per failed spawn.
///
/// # Safety
///
/// Caller must own both handles. TerminateProcess on a suspended child
/// is well-defined; the child never ran any code.
unsafe fn terminate_and_close_suspended_child(process: HANDLE, thread: HANDLE) {
    use windows_sys::Win32::System::Threading::TerminateProcess;
    if !process.is_null() {
        unsafe {
            TerminateProcess(process, 1);
            CloseHandle(process);
        }
    }
    if !thread.is_null() {
        unsafe {
            CloseHandle(thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_line_quotes_args_with_spaces() {
        // Spec: paths with spaces (the common Windows case) survive
        // round-trip through Win32 argv parsing. A naive build that
        // joined with `" "` would produce `cmd /c echo hello world`
        // which the child reads as three args, not two.
        let cl = build_command_line("cmd.exe", &["/c".into(), "echo hello world".into()]);
        // Convert back to String for inspection.
        let s = String::from_utf16(&cl[..cl.len() - 1]).unwrap();
        assert!(s.contains(r#""echo hello world""#), "got: {s}");
    }

    #[test]
    fn build_command_line_escapes_embedded_quotes() {
        // Spec: an arg containing a literal `"` must be escaped per
        // Microsoft's command-line parser rules: doubled backslashes
        // before the embedded quote, then `\"` for the quote itself.
        let cl = build_command_line("cmd.exe", &[r#"a"b"#.into()]);
        let s = String::from_utf16(&cl[..cl.len() - 1]).unwrap();
        assert!(s.ends_with(r#""a\"b""#), "got: {s}");
    }

    #[test]
    fn build_command_line_doubles_trailing_backslashes_before_close_quote() {
        // Spec: trailing backslashes before the closing quote must
        // be doubled, otherwise the parser sees `\"` as an escaped
        // quote and misinterprets the arg boundary. Subtle bug that
        // only fires on Windows path arguments ending in `\`.
        let cl = build_command_line("x", &[r"path with space\".into()]);
        let s = String::from_utf16(&cl[..cl.len() - 1]).unwrap();
        assert!(s.ends_with(r#""path with space\\""#), "got: {s}");
    }
}
