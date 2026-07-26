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
//! # Status — wired into the spawn path
//!
//! [`install_pseudoconsole`] adds the `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`
//! attribute in [`super::process::spawn_with_token`] (the
//! [`super::process::SpawnStdio::Pty`] variant, which omits
//! `STARTF_USESTDHANDLES`), and [`pump_parent_stdio`] copies bytes
//! between the parent's stdio and the PTY on std threads. The Tier 1
//! trampoline ([`super::runtime`]) selects the PTY path only when its
//! stdout is a real console, so non-interactive tool calls keep the
//! proven inherited-handle behavior. Real-host validation of the
//! interactive path is tracked in `docs/ROADMAP.md`.
//!
//! # Why a separate module
//!
//! Kept as its own directory because pseudo-console setup interacts
//! with multiple subsystems (process attributes, pipe pumping,
//! terminal-mode detection). Isolating it here keeps the eventual
//! wire-up to `process.rs` a single import
//! change rather than a hundred-line diff in the middle of
//! `spawn_with_token`.

use std::io;
use std::ptr;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GetStdHandle, ResizePseudoConsole, COORD, HPCON,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE;

use super::mitigation::AttributeList;

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
        // Named bindings mark the ownership transfer for readers: from here on
        // ClosePseudoConsole is the only thing allowed to close these two.
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

    /// Resize the pseudo-console to `cols`×`rows`. Called when the
    /// operator's terminal changes size so the child re-wraps output
    /// correctly. A failure surfaces the `ResizePseudoConsole` HRESULT.
    pub fn resize(&self, cols: i16, rows: i16) -> io::Result<()> {
        let size = COORD { X: cols, Y: rows };
        // SAFETY: self.pc is a live HPCON we own; size is a stack-local.
        let hr = unsafe { ResizePseudoConsole(self.pc, size) };
        if hr != 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        Ok(())
    }
}

/// Install the pseudo-console attribute into a child's attribute list.
///
/// Mirrors [`super::mitigation::install_default_mitigations`]. Per
/// Microsoft's ConPTY guidance the attribute *value* is the `HPCON`
/// handle itself (passed as `lpValue`, with `cbSize = size_of::<HPCON>()`),
/// not a pointer to it.
///
/// # Safety
///
/// `pty` must outlive the `CreateProcess*` call that consumes the
/// attribute list.
pub unsafe fn install_pseudoconsole(
    list: &mut AttributeList,
    pty: &OwnedPseudoConsole,
) -> io::Result<()> {
    unsafe {
        list.update(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            pty.hpcon() as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
        )
    }
}

/// HANDLE wrapper that can cross a thread boundary. The pump moves raw
/// kernel handles into worker threads; `HANDLE` is `!Send` by default
/// because it's a raw pointer, but a kernel handle value is safe to use
/// from another thread (the kernel object is process-wide).
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// SAFETY: a HANDLE is a process-wide kernel-object reference; using it
// from a different thread than the one that created it is well-defined.
unsafe impl Send for SendHandle {}

impl SendHandle {
    /// Consume the wrapper and yield the raw handle. Taking `self`
    /// (rather than reading the `.0` field directly inside a closure)
    /// forces edition-2021 disjoint capture to move the whole `Send`
    /// wrapper into the thread instead of the bare `!Send` handle.
    fn raw(self) -> HANDLE {
        self.0
    }
}

/// Running byte-pump between the parent's real stdio and a
/// pseudo-console. Drop/`join` after the child exits.
pub struct PtyPump {
    output: Option<JoinHandle<()>>,
}

impl PtyPump {
    /// Wait for the output drain thread to finish. Call after the child
    /// has exited — the PTY closes its output write-end on child exit,
    /// so the drain thread sees EOF and returns.
    pub fn join(mut self) {
        if let Some(h) = self.output.take() {
            let _ = h.join();
        }
    }
}

/// Start pumping bytes between the parent's real stdin/stdout and the
/// pseudo-console for the lifetime of the child.
///
/// Two threads: parent-stdout drain (PTY → real stdout), joined via the
/// returned [`PtyPump`]; and a detached parent-stdin feed (real stdin →
/// PTY) that blocks on the console read and is reaped at process exit.
/// The handles are copied by value into the threads, so the threads do
/// not borrow `pty`; the caller must keep `pty` alive until after
/// [`PtyPump::join`].
pub fn pump_parent_stdio(pty: &OwnedPseudoConsole) -> PtyPump {
    let out_src = SendHandle(pty.output_read_handle());
    let in_dst = SendHandle(pty.input_write_handle());

    // PTY → real stdout: drains child output until EOF (child exit).
    let output = std::thread::spawn(move || {
        // SAFETY: STD_OUTPUT_HANDLE is a documented constant.
        let dst = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        copy_handle(out_src.raw(), dst);
    });

    // real stdin → PTY: feeds operator keystrokes. Detached — it blocks
    // on console reads and we never join it; the kernel reaps it when
    // the trampoline process exits.
    let _input = std::thread::spawn(move || {
        // SAFETY: STD_INPUT_HANDLE is a documented constant.
        let src = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        copy_handle(src, in_dst.raw());
    });

    PtyPump {
        output: Some(output),
    }
}

/// Copy bytes from `src` to `dst` until `src` reaches EOF or either
/// side errors. Used by the pump threads.
fn copy_handle(src: HANDLE, dst: HANDLE) {
    let mut buf = [0u8; 8192];
    loop {
        let mut read: u32 = 0;
        // SAFETY: src is a valid handle; buf/read are stack-local.
        let ok = unsafe {
            ReadFile(
                src,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            return;
        }
        let mut off: usize = 0;
        while off < read as usize {
            let mut written: u32 = 0;
            // SAFETY: dst is a valid handle; the slice is in-bounds.
            let ok = unsafe {
                WriteFile(
                    dst,
                    buf[off..read as usize].as_ptr(),
                    (read as usize - off) as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return;
            }
            off += written as usize;
        }
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
