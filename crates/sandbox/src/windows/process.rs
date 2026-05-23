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

use super::mitigation::{default_mitigation_flags, install_default_mitigations, AttributeList};

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
    pid: u32,
}

impl SpawnedChild {
    /// Process ID of the spawned child.
    pub fn pid(&self) -> u32 {
        self.pid
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

/// Spawn `program + args` under `token`, inheriting the trampoline's
/// stdio. Caller passes `cwd` (working directory; null = inherit) and
/// optionally a UTF-16 environment block (null = inherit parent env).
///
/// # Safety
///
/// `token` must be a valid Windows token HANDLE owned by the caller
/// for the duration of this call. The function calls
/// `CreateProcessAsUserW`, which dereferences the handle to set up the
/// child process's primary token; an invalid or already-closed handle
/// is undefined behavior at the kernel boundary.
pub unsafe fn spawn_with_token(
    token: HANDLE,
    program: &str,
    args: &[String],
    cwd: Option<&std::path::Path>,
    env_block: Option<&[u16]>,
) -> io::Result<SpawnedChild> {
    let mut cmdline = build_command_line(program, args);
    let cwd_w: Option<Vec<u16>> = cwd.map(|p| {
        OsStr::new(p)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });

    // SAFETY: STD_*_HANDLE are documented constants; GetStdHandle
    // returns the trampoline's inherited handles which we want the
    // child to share.
    let (stdin_h, stdout_h, stderr_h) = unsafe {
        (
            GetStdHandle(STD_INPUT_HANDLE),
            GetStdHandle(STD_OUTPUT_HANDLE),
            GetStdHandle(STD_ERROR_HANDLE),
        )
    };

    // Build the extended startup info, with a one-entry attribute list
    // carrying the mitigation policy. The attribute list and the
    // flags_storage `u64` MUST stay on this stack frame until
    // CreateProcessAsUserW returns — UpdateProcThreadAttribute stores
    // pointers into them, not copies.
    let mut attr_list = AttributeList::new(1)?;
    let mitigation_flags: u64 = default_mitigation_flags();
    // SAFETY: mitigation_flags lives on this stack frame until after
    // the CreateProcessAsUserW call below; attr_list outlives it for
    // the same reason. install_default_mitigations stashes the
    // pointer into the kernel-managed attribute list.
    unsafe { install_default_mitigations(&mut attr_list, &mitigation_flags) }?;

    let mut siex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    siex.StartupInfo.hStdInput = stdin_h;
    siex.StartupInfo.hStdOutput = stdout_h;
    siex.StartupInfo.hStdError = stderr_h;
    siex.lpAttributeList = attr_list.as_ptr();

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
            TRUE,
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
