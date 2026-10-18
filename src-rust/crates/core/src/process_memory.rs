//! Process memory inspection and retained-heap release.
//!
//! Ported from jcode's `jcode-base/src/process_memory.rs`, trimmed to what
//! idle session hibernation needs: an RSS reading for the before/after
//! notice, and a way to return freed-but-retained heap pages to the OS.
//! Claurst does not build against jemalloc, so the release path is
//! `malloc_trim(0)` on glibc and a no-op elsewhere.

/// Current resident set size in bytes, when the platform exposes it.
///
/// macOS: `proc_pidinfo` resident size. Linux: `VmRSS` from
/// `/proc/self/status`. Other platforms report `None`, which callers treat
/// as "measurement unavailable" rather than zero.
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        rss_bytes_macos()
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status.lines().find_map(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed.strip_prefix("VmRSS:")?;
            let mut parts = rest.split_whitespace();
            let number = parts.next()?.parse::<u64>().ok()?;
            // /proc reports kB (KiB).
            Some(number.saturating_mul(1024))
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Return freed-but-retained heap pages to the OS.
///
/// The system allocator keeps pages freed by large transient allocations
/// (transcript rebuilds, provider payloads, render caches) inside its arenas,
/// which shows up as RSS that never shrinks. On glibc, `malloc_trim(0)` walks
/// all arenas and hands the pages back (MADV_DONTNEED). On other platforms
/// this is a no-op: the measured notice may then read "unchanged", which is
/// honest, and no savings are ever invented.
pub fn release_retained_heap(reason: &str) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: `malloc_trim` is a glibc symbol with a C ABI matching the
        // declaration above; `pad = 0` leaves every free page eligible for
        // return and the call has no preconditions on our heap state.
        let trimmed = unsafe { malloc_trim(0) };
        tracing::debug!(target: "process_memory", "malloc_trim ({reason}): {}", if trimmed == 1 {
            "released pages"
        } else {
            "no pages to release"
        });
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = reason;
    }
}

#[cfg(target_os = "macos")]
fn rss_bytes_macos() -> Option<u64> {
    // SAFETY: `proc_pidinfo` with PROC_PIDTASKINFO reads the current
    // process's task info into a fixed-size kernel-owned struct layout.
    unsafe {
        let pid = libc::getpid();
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut libc::proc_taskinfo as *mut libc::c_void,
            std::mem::size_of::<libc::proc_taskinfo>() as i32,
        );
        if size <= 0 {
            return None;
        }
        Some(info.pti_resident_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_retained_heap_is_safe_to_call() {
        // Allocate and drop a large transient buffer, then release. This must
        // not crash on any allocator configuration.
        let buffer = vec![0u8; 8 * 1024 * 1024];
        drop(buffer);
        release_retained_heap("unit_test");
    }

    #[test]
    fn rss_bytes_reports_some_on_supported_platforms() {
        let rss = rss_bytes();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(
                rss.is_some_and(|bytes| bytes > 0),
                "live process RSS must be positive"
            );
        }
    }
}
