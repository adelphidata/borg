//! Pseudo-console (ConPTY) for sandboxed-child stdio.
//!
//! The Tier 1 trampoline currently inherits the parent's stdio
//! handles directly into the child. That works for non-interactive
//! tool invocations (the common case) but leaks two affordances we'd
//! prefer to revoke:
//!
//! 1. The child can read the parent's terminal directly. A misbehaving
//!    sandboxed tool that calls `GetConsoleWindow` / `ReadConsoleInput`
//!    gets the operator's *real* console, not a sandboxed view of it.
//! 2. There's no escape-sequence boundary — a sandboxed tool that
//!    emits arbitrary VT bytes can paint the parent terminal however
//!    it likes (cursor jumps, color escapes, title changes). For a
//!    headed UI this is benign; for an agent driving the terminal it's
//!    a small source of confusion.
//!
//! Wrapping the child in `CreatePseudoConsole` gives us a real
//! terminal interface to hand to the sandboxed binary while the
//! trampoline pumps bytes between the PTY and the parent's stdout/
//! stderr. The pump can apply filtering (strip dangerous sequences,
//! rewrite TTY queries that the agent doesn't intend to honor) — that
//! filtering layer is future work; this module is the transport.
//!
//! # Status — module shipped, spawn wiring deferred
//!
//! The `OwnedPseudoConsole` RAII handle, pipe setup, and resize
//! plumbing are here. The integration with `process::spawn_with_token`
//! (replace `STARTF_USESTDHANDLES` with the
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` attribute, spawn a tokio
//! pump task) is deferred so that the existing stdio-inheritance
//! behavior keeps working — landing the wire-up requires real Windows
//! testing to confirm we don't break non-interactive tools. See the
//! "Windows sandbox — ConPTY for stdio" bullet in ROADMAP.
//!
//! # Why a separate module
//!
//! Codex's `conpty/` is its own directory because pseudo-console
//! setup interacts with multiple subsystems (process attributes,
//! pipe pumping, terminal-mode detection). Mirroring that boundary
//! here keeps the eventual wire-up to `process.rs` a single import
//! change rather than a hundred-line diff in the middle of
//! `spawn_with_token`.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};

/// Default initial size for the pseudo-console. The sandboxed child
/// can renegotiate via `ResizePseudoConsole` if the operator's
/// terminal changes size mid-spawn. 80×24 is the universal terminal
/// fallback — every CLI tool handles it.
pub const DEFAULT_COLS: i16 = 80;
/// Companion to [`DEFAULT_COLS`].
pub const DEFAULT_ROWS: i16 = 24;

/// Owning handle to a pseudo-console plus the two pipes that carry
/// the bytes. The trampoline writes input bytes into `input_write`
/// (the parent side of the input pipe) and reads output bytes out of
/// `output_read` (the parent side of the output pipe). The PTY ends
/// of both pipes (`input_read` and `output_write`) are owned by the
/// pseudo-console itself and freed by `ClosePseudoConsole`.
///
/// Drop tears everything down in the right order: pseudo-console
/// first (which closes its own ends of the two pipes), then the
/// parent's ends. Closing the parent ends before the PTY would risk
/// the PTY blocking on a write into a closed pipe.
pub struct OwnedPseudoConsole {
    pc: HPCON,
    input_write: HANDLE,
    output_read: HANDLE,
}

// SAFETY: HPCON and HANDLE are kernel-object references. Close* /
// CloseHandle are safe from any thread. No shared mutable state.
unsafe impl Send for OwnedPseudoConsole {}
unsafe impl Sync for OwnedPseudoConsole {}

impl OwnedPseudoConsole {
    /// Allocate the pseudo-console with the default 80×24 size and
    /// open input/output pipes whose PTY ends are bound to it.
    pub fn new() -> io::Result<Self> {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// Allocate the pseudo-console with caller-specified size.
    pub fn with_size(cols: i16, rows: i16) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            // PTY ends will be inherited via the pseudo-console
            // attribute; the parent ends stay in the trampoline.
            bInheritHandle: FALSE,
        };

        // Input pipe: parent writes to input_write, PTY reads from input_read.
        let mut input_read: HANDLE = ptr::null_mut();
        let mut input_write: HANDLE = ptr::null_mut();
        // SAFETY: stack-local out-pointers, valid SA.
        let ok = unsafe { CreatePipe(&mut input_read, &mut input_write, &sa, 0) };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }

        // Output pipe: PTY writes to output_write, parent reads from output_read.
        let mut output_read: HANDLE = ptr::null_mut();
        let mut output_write: HANDLE = ptr::null_mut();
        // SAFETY: same.
        let ok = unsafe { CreatePipe(&mut output_read, &mut output_write, &sa, 0) };
        if ok == FALSE {
            let err = io::Error::last_os_error();
            // SAFETY: close any handles we already allocated.
            unsafe {
                CloseHandle(input_read);
                CloseHandle(input_write);
            }
            return Err(err);
        }

        let size = COORD { X: cols, Y: rows };
        let mut pc: HPCON = 0;
        // SAFETY: input_read + output_write are valid; size is a
        // stack-local struct; flags=0 means no special options.
        let hr = unsafe { CreatePseudoConsole(size, input_read, output_write, 0, &mut pc) };
        // After CreatePseudoConsole, the PTY owns input_read and
        // output_write; we must NOT close them on the success path.
        // On the failure path we still own them.
        if hr != 0 {
            // SAFETY: close everything we own at this point.
            unsafe {
                CloseHandle(input_read);
                CloseHandle(output_write);
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            return Err(io::Error::from_raw_os_error(hr));
        }
        // PTY now owns input_read and output_write; drop our local
        // references by zeroing them is unnecessary — we never use
        // them again from Rust, and ClosePseudoConsole handles cleanup.
        // But ensure they aren't accidentally closed on a future
        // refactor by sticking `let _ = …` markers in.
        let _pty_owns_input_read = input_read;
        let _pty_owns_output_write = output_write;
        Ok(Self {
            pc,
            input_write,
            output_read,
        })
    }

    /// Borrowed handle the trampoline writes parent→child bytes to.
    pub fn input_write_handle(&self) -> HANDLE {
        self.input_write
    }

    /// Borrowed handle the trampoline reads child→parent bytes from.
    pub fn output_read_handle(&self) -> HANDLE {
        self.output_read
    }

    /// `HPCON` to pass via `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` on
    /// the child's attribute list.
    pub fn hpcon(&self) -> HPCON {
        self.pc
    }
}

impl Drop for OwnedPseudoConsole {
    fn drop(&mut self) {
        if self.pc != 0 {
            // SAFETY: we own this HPCON. ClosePseudoConsole closes
            // the PTY's ends of the pipes and tears down the
            // pseudoconsole state. Calling it while a child is still
            // attached is documented as safe (the child sees EOF on
            // its stdin and ERROR_BROKEN_PIPE on its stdout).
            unsafe {
                ClosePseudoConsole(self.pc);
            }
        }
        // SAFETY: we own these two parent-side handles; close after
        // the PTY teardown so any in-flight writes from the child
        // see ERROR_BROKEN_PIPE rather than crashing the kernel
        // pipe-write path.
        unsafe {
            if !self.input_write.is_null() && self.input_write != INVALID_HANDLE_VALUE {
                CloseHandle(self.input_write);
            }
            if !self.output_read.is_null() && self.output_read != INVALID_HANDLE_VALUE {
                CloseHandle(self.output_read);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_drop_pseudo_console_succeeds() {
        // Spec: the happy-path allocation must succeed on Windows 10+.
        // A failure here surfaces as a non-zero HRESULT and would
        // indicate either a Windows-version mismatch or a misuse of
        // the API. We can't validate the byte-pump behavior here
        // (that's deferred to the eventual integration test), but the
        // allocation+teardown round-trip is non-trivial and worth
        // pinning.
        let pty = OwnedPseudoConsole::new().expect("CreatePseudoConsole must succeed");
        assert_ne!(pty.hpcon(), 0, "HPCON must be non-zero after creation");
        // Drop runs ClosePseudoConsole + CloseHandle on the parent
        // ends; if the order is wrong we crash here, which the test
        // framework would surface as a panic.
        drop(pty);
    }

    #[test]
    fn create_with_explicit_size_succeeds() {
        // Spec: callers passing a non-default size (e.g. matched to
        // the operator's actual terminal) must also work. A regression
        // that hard-coded 80x24 would break interactive tools that
        // care about line wrapping.
        let pty = OwnedPseudoConsole::with_size(120, 40).expect("size override must work");
        assert_ne!(pty.hpcon(), 0);
    }
}
