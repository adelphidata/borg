//! Windows job objects: containment for a spawned child's descendants.
//!
//! Unix contains a subprocess tree with a process group (see
//! [`crate::process_group`]). Windows has no such thing — `kill_on_drop` reaches
//! only the direct child, so a stdio MCP server or a timed-out hook that
//! spawned helpers leaves them running after Borg is done with it.
//!
//! A job object is the equivalent boundary. A child assigned to one dies when
//! the job is terminated or its last handle closes, and — because the breakaway
//! limits are deliberately *not* set — a descendant cannot escape by asking to.
//!
//! ## The assignment window
//!
//! The child is assigned immediately after spawn rather than being created
//! suspended, so a child that spawns a grandchild in its first instants could
//! in principle produce one outside the job. Closing that window needs a
//! hand-rolled `CreateProcessW` with `CREATE_SUSPENDED`, which would mean not
//! using `Command` at all. The trade is deliberate: this turns "descendants
//! always leak" into "descendants leak only in a sub-millisecond race", and the
//! MCP servers and hooks in question spawn helpers after their own startup, not
//! during it.
//!
//! Every failure path degrades to the previous behaviour rather than refusing
//! to run: a server we cannot contain is still better than no server.

/// A job object handle that terminates its members when dropped.
///
/// Unix builds get an inert placeholder so callers need no `cfg`.
#[derive(Debug)]
pub struct JobObject {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a job object HANDLE is a kernel object reference with no thread
// affinity; the Win32 calls used here are all thread-safe.
#[cfg(windows)]
unsafe impl Send for JobObject {}
#[cfg(windows)]
unsafe impl Sync for JobObject {}

/// Whether closing the last handle to a job kills its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnClose {
    /// Members die when the job handle closes. Right for a subprocess whose
    /// whole tree should go when it does — a stdio MCP server.
    Kill,
    /// Members survive the handle closing; only an explicit
    /// [`JobObject::terminate`] kills them. Right for a hook, where a helper
    /// the script deliberately detached is meant to keep running after a
    /// *successful* exit, and only a timeout should take the tree down.
    Leave,
}

/// The job's limit flags for a close mode.
///
/// The breakaway flags are never set: with either one a descendant can leave
/// the job with `CREATE_BREAKAWAY_FROM_JOB`, which is exactly the leak this
/// module exists to close.
#[cfg(windows)]
fn limit_flags(on_close: OnClose) -> u32 {
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    match on_close {
        OnClose::Kill => JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        OnClose::Leave => 0,
    }
}

#[cfg(windows)]
impl JobObject {
    /// Create an anonymous, non-inheritable job.
    pub fn new(on_close: OnClose) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        };

        // SAFETY: null name (anonymous) and null security attributes (default
        // DACL, not inheritable). Returns NULL on failure with the error set.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = limit_flags(on_close);

        // SAFETY: `info` is a fully-initialized stack local and the size passed
        // is its actual size.
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: we own `handle` and have not handed it out.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(err);
        }
        Ok(Self { handle })
    }

    /// Put an already-spawned process into this job by PID.
    ///
    /// Opens the process with only the rights `AssignProcessToJobObject`
    /// needs, and always closes that handle.
    pub fn assign_pid(&self, pid: u32) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        // SAFETY: FFI. Returns NULL on failure with the error set.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both handles are valid and owned by us.
        let ok = unsafe { AssignProcessToJobObject(self.handle, process) };
        let result = if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        };
        // SAFETY: our handle, not shared.
        unsafe { CloseHandle(process) };
        result
    }

    /// Kill every process still in the job.
    pub fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: FFI on a handle we own. A failure means the job is already
        // gone, which is the state we wanted.
        unsafe { TerminateJobObject(self.handle, 1) };
    }
}

#[cfg(windows)]
impl Drop for JobObject {
    fn drop(&mut self) {
        // Under `OnClose::Kill` this close is the containment guarantee, not
        // mere tidiness: it is what reaps the members.
        // SAFETY: our handle, closed exactly once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
impl JobObject {
    /// Unix builds contain descendants with a process group instead, so this
    /// is deliberately inert.
    pub fn new(_on_close: OnClose) -> std::io::Result<Self> {
        Ok(Self {})
    }

    /// No-op on unix. See [`crate::process_group`] for the real mechanism.
    pub fn assign_pid(&self, _pid: u32) -> std::io::Result<()> {
        Ok(())
    }

    /// No-op on unix.
    pub fn terminate(&self) {}
}

/// Create a job and put `pid` in it, logging and returning `None` if either
/// step fails.
///
/// Callers use this to *add* containment: a `None` return means the caller
/// keeps whatever cleanup it already had, never that it should refuse to run.
pub fn contain(pid: u32, label: &str, on_close: OnClose) -> Option<JobObject> {
    if cfg!(not(windows)) {
        return None;
    }
    let job = match JobObject::new(on_close) {
        Ok(job) => job,
        Err(e) => {
            tracing::warn!(label, error = %e, "could not create a job object; descendants of this process will not be contained");
            return None;
        }
    };
    if let Err(e) = job.assign_pid(pid) {
        tracing::warn!(label, pid, error = %e, "could not assign the process to its job object; descendants will not be contained");
        return None;
    }
    Some(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec: containment is best-effort by contract. On a platform without job
    /// objects the helpers must stay inert rather than panic, because the
    /// caller treats a failure as "keep the old cleanup path".
    ///
    /// Unix-only by necessity: on Windows this would put the *test process*
    /// into a kill-on-close job and then terminate it, taking the whole test
    /// runner down with it.
    #[cfg(not(windows))]
    #[test]
    fn the_helpers_are_inert_where_jobs_do_not_apply() {
        for mode in [OnClose::Kill, OnClose::Leave] {
            let job = JobObject::new(mode).expect("constructing a job never fails on unix");
            assert!(job.assign_pid(std::process::id()).is_ok());
            job.terminate();
        }

        // `contain` reports None on unix, which is the signal that the caller
        // must keep relying on its process group.
        assert!(contain(std::process::id(), "test", OnClose::Kill).is_none());
    }

    /// Spec: `Kill` sets kill-on-close and `Leave` sets nothing, and neither
    /// ever sets a breakaway flag — a breakaway flag lets a descendant leave
    /// the job with `CREATE_BREAKAWAY_FROM_JOB`, which is exactly the leak
    /// this module exists to close.
    ///
    /// Asserted on the flag words rather than on a live job so the invariant
    /// is checked by the Windows cross-compile lane too.
    #[cfg(windows)]
    #[test]
    fn the_limit_flags_kill_on_close_only_when_asked_and_never_break_away() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        };
        assert_eq!(
            limit_flags(OnClose::Kill),
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        );
        assert_eq!(limit_flags(OnClose::Leave), 0);
        for mode in [OnClose::Kill, OnClose::Leave] {
            assert_eq!(
                limit_flags(mode)
                    & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK),
                0,
                "{mode:?} must not let descendants escape the job"
            );
        }
    }
}
