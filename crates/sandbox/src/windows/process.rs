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
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT,
    INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Owning handle to a spawned process + its main thread.
pub struct SpawnedChild {
    process: HANDLE,
    thread: HANDLE,
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
        // SAFETY: We own both handles. CloseHandle on a process
        // handle does NOT terminate the process; it only releases
        // our reference. The kernel reaps the process record after
        // every handle goes away.
        unsafe {
            if !self.process.is_null() {
                CloseHandle(self.process);
            }
            if !self.thread.is_null() {
                CloseHandle(self.thread);
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

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = stdin_h;
    si.hStdOutput = stdout_h;
    si.hStdError = stderr_h;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let creation_flags = if env_block.is_some() {
        CREATE_UNICODE_ENVIRONMENT
    } else {
        0
    };
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
    // the std handles with the child.
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
            &si,
            &mut pi,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    Ok(SpawnedChild {
        process: pi.hProcess,
        thread: pi.hThread,
        pid: pi.dwProcessId,
    })
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
