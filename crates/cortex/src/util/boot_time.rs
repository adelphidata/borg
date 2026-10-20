//! System boot time, for telling a reused PID from a surviving process.
//!
//! A PID number alone cannot prove that the process which wrote a lock row is
//! still the one running under it: after a reboot the kernel hands PIDs out
//! again from the start, and a root-owned process on the recycled number
//! answers `kill(pid, 0)` with `EPERM`, which the liveness probe treats as
//! alive. A row whose heartbeat predates the current boot cannot belong to
//! any process that exists now, whatever the probe says about the number.

/// Seconds since the UNIX epoch at which this host last booted, or `None`
/// when the platform offers no readable source (callers then keep the plain
/// PID-liveness rule).
pub fn system_boot_time() -> Option<i64> {
    read_boot_time()
}

#[cfg(target_os = "linux")]
fn read_boot_time() -> Option<i64> {
    match std::fs::read_to_string("/proc/stat") {
        Ok(stat) => parse_proc_stat_btime(&stat),
        Err(e) => {
            tracing::warn!(error = %e, "boot time unavailable: /proc/stat unreadable");
            None
        }
    }
}

/// The `btime` line of `/proc/stat` is the boot instant in epoch seconds.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat_btime(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|rest| rest.trim().parse::<i64>().ok())
}

#[cfg(target_os = "macos")]
fn read_boot_time() -> Option<i64> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    // SAFETY: pure FFI. `mib` names the `kern.boottime` node, `tv` is a
    // correctly sized, writable `timeval`, and `len` carries its size in and
    // the bytes written out; no pointer outlives the call.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut tv as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != std::mem::size_of::<libc::timeval>() {
        tracing::warn!(
            errno = std::io::Error::last_os_error().raw_os_error(),
            "boot time unavailable: sysctl kern.boottime failed"
        );
        return None;
    }
    Some(tv.tv_sec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_boot_time() -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_btime_from_proc_stat_text() {
        let stat = "cpu  2255 34 2290 22625563 6290 127 456 0 0 0\n\
                    cpu0 1132 34 1441 11311718 3675 127 438 0 0 0\n\
                    intr 114930548 113199788 3 0 5 263 0 4 [...] 0 0 0\n\
                    ctxt 1990473\n\
                    btime 1760000000\n\
                    processes 2915\n";
        assert_eq!(parse_proc_stat_btime(stat), Some(1_760_000_000));
    }

    #[test]
    fn missing_or_malformed_btime_is_none_not_zero() {
        // A zero boot time would mark every lock row as post-boot and
        // silently disable the check; absence must be explicit.
        assert_eq!(parse_proc_stat_btime("cpu 1 2 3\nprocesses 4\n"), None);
        assert_eq!(parse_proc_stat_btime("btime not-a-number\n"), None);
        assert_eq!(parse_proc_stat_btime("btimes 1760000000\n"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn host_boot_time_is_a_past_instant_within_this_epoch() {
        let now = chrono::Utc::now().timestamp();
        let boot = system_boot_time().expect("this platform exposes its boot time");
        assert!(boot <= now, "boot {boot} is in the future of now {now}");
        // Ten years is far beyond any plausible uptime; a value this old
        // means the field was misread (wrong struct offset, ticks not secs).
        assert!(
            boot > now - 10 * 365 * 86_400,
            "boot {boot} is implausibly old"
        );
    }
}
