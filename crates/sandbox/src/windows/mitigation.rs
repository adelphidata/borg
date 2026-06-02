//! Process-creation mitigation policies for the sandboxed child.
//!
//! Forces a defense-in-depth set of mitigations on every spawn via
//! `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`. These are independent of
//! the restricted token: even if a sandboxed binary's manifest opts
//! out of ASLR, DEP, or strict-handle checks, the kernel enforces our
//! parent-side values.
//!
//! # What we set
//!
//! The defaults are conservative — every flag is "defense in depth"
//! that shouldn't break legitimate tools:
//!
//! | Flag | Effect | Why safe-by-default |
//! |---|---|---|
//! | `DEP_ENABLE` | W^X enforcement | Universally safe; required since Vista. |
//! | `DEP_ATL_THUNK_ENABLE` | DEP for ATL thunks | Compatibility carrier for DEP_ENABLE. |
//! | `SEHOP_ENABLE` | Structured-exception-handler validation | Generic SEH hardening. |
//! | `BOTTOM_UP_ASLR_ALWAYS_ON` | Randomize VA bottom-up | Compatible with all PE binaries. |
//! | `HIGH_ENTROPY_ASLR_ALWAYS_ON` | 64-bit ASLR entropy | 64-bit only; ignored on 32-bit. |
//! | `STRICT_HANDLE_CHECKS_ALWAYS_ON` | Invalid-handle = exception | Catches handle-confusion bugs. |
//! | `HEAP_TERMINATE_ALWAYS_ON` | Crash on heap corruption | Already the default on x64. |
//! | `EXTENSION_POINT_DISABLE_ALWAYS_ON` | No AppInit DLLs / IAT shims | Blocks injection vectors. |
//! | `IMAGE_LOAD_NO_REMOTE_ALWAYS_ON` | No DLL load from UNC paths | Defeats remote-DLL planting. |
//! | `IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON` | No DLL load from low-IL paths | Blocks cross-IL DLL planting. |
//! | `IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON` | Prefer system32 on lookup | Defeats DLL-hijack via search order. |
//!
//! # What we DON'T set
//!
//! - `BLOCK_NON_MICROSOFT_BINARIES_ALWAYS_ON` — would break every
//!   sandboxed binary that isn't shipped by Microsoft, including the
//!   user's installed Python/Node/etc.
//! - `DYNAMIC_CODE_DISABLE_ALWAYS_ON` — would break JIT in V8, CPython
//!   extensions that use trampolines, JVM, anything `RWX`. Sandboxed
//!   tools commonly need these.
//! - `WIN32K_SYSTEM_CALL_DISABLE_ALWAYS_ON` — blocks win32k.sys
//!   syscalls. Any GUI process would die instantly.
//! - `CONTROL_FLOW_GUARD_ALWAYS_ON` — only meaningful for
//!   CFG-compiled binaries; setting it on others is a no-op.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{FALSE, HANDLE};
use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
};

/// Mitigation flag values. These are the documented
/// `PROCESS_CREATION_MITIGATION_POLICY_*` constants; windows-sys 0.59
/// does not re-export all of them, so we mirror the SDK values here.
/// Each is a bitflag inside a `u64` packed into the first DWORD64 of
/// the attribute value. Mirroring the constants is one of those "the
/// other option is depending on a future SDK revision" trade-offs;
/// the SDK semantics are stable per Windows ABI guarantees.
//
// The literals mirror the SDK's documented DWORD64 bit positions (e.g.
// `0x00000001_0000_0000` = the high-DWORD policy area); the deliberate
// 8-4-4 grouping tracks that layout, so silence the equal-group lint.
#[allow(clippy::unusual_byte_groupings)]
mod flags {
    pub const DEP_ENABLE: u64 = 0x01;
    pub const DEP_ATL_THUNK_ENABLE: u64 = 0x02;
    pub const SEHOP_ENABLE: u64 = 0x04;
    pub const FORCE_RELOCATE_IMAGES_ALWAYS_ON: u64 = 0x00000001_0000_0000;
    pub const HEAP_TERMINATE_ALWAYS_ON: u64 = 0x00010000_0000_0000;
    pub const BOTTOM_UP_ASLR_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 16;
    pub const HIGH_ENTROPY_ASLR_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 20;
    pub const STRICT_HANDLE_CHECKS_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 24;
    pub const EXTENSION_POINT_DISABLE_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 32;
    pub const IMAGE_LOAD_NO_REMOTE_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 52;
    pub const IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 56;
    pub const IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON: u64 = 0x00000001_0000_0000_u64 << 60;
}

/// Build the mitigation policy value to pass via
/// `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`. Returns a u64 the caller
/// stores on the stack and references by pointer.
pub fn default_mitigation_flags() -> u64 {
    // Combine into a single u64 — every flag above fits in the first
    // DWORD64. If a future change adds a Win10-RS5+ flag that lives in
    // the second DWORD64, switch to a [u64; 2] array and update the
    // size argument to UpdateProcThreadAttribute.
    flags::DEP_ENABLE
        | flags::DEP_ATL_THUNK_ENABLE
        | flags::SEHOP_ENABLE
        | flags::FORCE_RELOCATE_IMAGES_ALWAYS_ON
        | flags::HEAP_TERMINATE_ALWAYS_ON
        | flags::BOTTOM_UP_ASLR_ALWAYS_ON
        | flags::HIGH_ENTROPY_ASLR_ALWAYS_ON
        | flags::STRICT_HANDLE_CHECKS_ALWAYS_ON
        | flags::EXTENSION_POINT_DISABLE_ALWAYS_ON
        | flags::IMAGE_LOAD_NO_REMOTE_ALWAYS_ON
        | flags::IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON
        | flags::IMAGE_LOAD_PREFER_SYSTEM32_ALWAYS_ON
}

/// RAII wrapper for a `PROC_THREAD_ATTRIBUTE_LIST`. The buffer
/// allocation and the kernel's `Initialize`/`Delete` calls bracket the
/// caller's `UpdateProcThreadAttribute` calls and the
/// `CreateProcessAsUserW` that consumes the list.
///
/// Drop order matters: the attribute list MUST outlive the
/// `CreateProcessAsUserW` call. In `spawn_with_token` that's
/// guaranteed by binding it to a local that drops after the call
/// returns.
pub struct AttributeList {
    /// Raw allocation that backs the attribute list. The kernel
    /// initializes this in place via
    /// [`InitializeProcThreadAttributeList`].
    buffer: Vec<u8>,
    /// True if [`InitializeProcThreadAttributeList`] succeeded — gates
    /// the matching [`DeleteProcThreadAttributeList`] on Drop.
    initialized: bool,
}

impl AttributeList {
    /// Allocate and initialize an attribute list sized for
    /// `num_attributes` slots.
    pub fn new(num_attributes: u32) -> io::Result<Self> {
        let mut size: usize = 0;
        // First call: NULL buffer, returns FALSE with the required
        // size in `size`. This is the documented two-step pattern.
        // SAFETY: out-param pointers are stack-local; NULL list is
        // documented as legal for the size query.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), num_attributes, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::other(
                "InitializeProcThreadAttributeList size query returned 0",
            ));
        }
        let mut this = Self {
            buffer: vec![0u8; size],
            initialized: false,
        };
        // SAFETY: buffer is at least `size` bytes; pointers are local.
        let ok = unsafe {
            InitializeProcThreadAttributeList(this.as_ptr(), num_attributes, 0, &mut size)
        };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        this.initialized = true;
        Ok(this)
    }

    /// Pointer suitable for `STARTUPINFOEXW::lpAttributeList`.
    pub fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }

    /// Add or replace the value for `attribute`. `value` and `size`
    /// describe a caller-owned memory region that must remain valid
    /// until `CreateProcess*` is called.
    ///
    /// # Safety
    ///
    /// Caller is responsible for the lifetime of the value pointed
    /// to: it must outlive the attribute list's use by
    /// `CreateProcess*`. The standard pattern is to hold it on the
    /// same stack frame.
    pub unsafe fn update(
        &mut self,
        attribute: usize,
        value: *const core::ffi::c_void,
        size: usize,
    ) -> io::Result<()> {
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute,
                value,
                size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: buffer is a valid initialized attribute list.
            // Calling Delete after a successful Initialize is the
            // documented teardown path; calling it on an uninitialized
            // buffer is UB, which the `initialized` flag guards
            // against.
            unsafe {
                DeleteProcThreadAttributeList(
                    self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
                );
            }
        }
    }
}

/// Wire the default mitigation flags into an `AttributeList`. The
/// caller holds the returned list (and the `u64` it points into)
/// alive across the `CreateProcessAsUserW` call.
///
/// # Safety
///
/// The returned `AttributeList`'s `as_ptr()` value is only valid as
/// long as `flags_storage` stays at the same address. Stack-allocate
/// both in the same scope; passing them around by value would move the
/// flags and dangle the pointer the kernel reads from.
pub unsafe fn install_default_mitigations(
    list: &mut AttributeList,
    flags_storage: &u64,
) -> io::Result<()> {
    unsafe {
        list.update(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
            flags_storage as *const u64 as *const core::ffi::c_void,
            std::mem::size_of::<u64>(),
        )
    }
}

// Suppress an "unused HANDLE import on non-Windows" warning that
// surfaces when the file is built on a host that hides Windows types
// behind cfg. The type is referenced in the unsafe-fn signatures via
// LPPROC_THREAD_ATTRIBUTE_LIST.
#[allow(dead_code)]
fn _suppress_handle_unused(_h: HANDLE) {}
