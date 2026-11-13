//! The computer's memory (total and available) for the results budget, read
//! with the operating system's own calls: a few lines per platform instead of
//! a system-information crate, to keep the app small (AGENTS.md: app size).

/// Total physical memory in bytes.
pub fn total() -> Option<u64> {
    imp::total().filter(|t| *t > 0)
}

/// Memory available to programs now (free plus reclaimable caches), in bytes.
pub fn available() -> Option<u64> {
    imp::available()
}

/// (free, total) bytes of the disk holding `path` (an existing folder);
/// free = what this user may still write.
pub fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    disk::space(path)
}

// System calls with plain out-parameters; nothing is kept past the call.
#[cfg(unix)]
#[allow(unsafe_code)]
mod disk {
    pub fn space(path: &std::path::Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: zeroed POD struct filled by statvfs; `c` is a valid C string.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = st.f_frsize as u64;
        Some((st.f_bavail as u64 * unit, st.f_blocks as u64 * unit))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod disk {
    pub fn space(path: &std::path::Path) -> Option<(u64, u64)> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated; the out-parameters are valid u64s.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
        (ok != 0).then_some((free, total))
    }
}

#[cfg(not(any(unix, windows)))]
mod disk {
    pub fn space(_path: &std::path::Path) -> Option<(u64, u64)> {
        None
    }
}

// System calls with plain out-parameters; nothing is kept past the call.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod imp {
    pub fn total() -> Option<u64> {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `v` and `len` are valid for the size sysctl writes.
        let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
        (rc == 0).then_some(v)
    }

    /// Free and inactive pages (inactive ones are reclaimed first, as
    /// Activity Monitor's "available" counts them).
    pub fn available() -> Option<u64> {
        // SAFETY: zeroed POD struct filled by the kernel; `count` is its size in integers.
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        #[allow(deprecated)] // libc points to the `mach2` crate: one call doesn't warrant a dependency.
        let host = unsafe { libc::mach_host_self() };
        let rc = unsafe { libc::host_statistics64(host, libc::HOST_VM_INFO64, (&mut stats as *mut libc::vm_statistics64).cast(), &mut count) };
        if rc != 0 {
            return None;
        }
        // SAFETY: plain sysconf query.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page > 0).then(|| (stats.free_count as u64 + stats.inactive_count as u64) * page as u64)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// A `/proc/meminfo` line in bytes (`MemAvailable:  123 kB`).
    fn meminfo(key: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        super::meminfo_value(&text, key)
    }

    pub fn total() -> Option<u64> {
        meminfo("MemTotal")
    }

    pub fn available() -> Option<u64> {
        meminfo("MemAvailable")
    }
}

// System call with a plain out-parameter; nothing is kept past the call.
#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    fn status() -> Option<MEMORYSTATUSEX> {
        // SAFETY: zeroed struct with its length set, as the call requires.
        let mut s: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        s.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (unsafe { GlobalMemoryStatusEx(&mut s) } != 0).then_some(s)
    }

    pub fn total() -> Option<u64> {
        status().map(|s| s.ullTotalPhys)
    }

    pub fn available() -> Option<u64> {
        status().map(|s| s.ullAvailPhys)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    pub fn total() -> Option<u64> {
        None
    }

    pub fn available() -> Option<u64> {
        None
    }
}

/// The value of `key` in `/proc/meminfo` text, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo_value(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.split(':').next() == Some(key))?;
    let mut parts = line.split(':').nth(1)?.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    Some(match parts.next() {
        Some("kB") => n * 1024,
        _ => n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_computer() {
        let (t, a) = (total(), available());
        if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            let t = t.expect("total memory");
            let a = a.expect("available memory");
            assert!(t > 256 << 20, "{t}");
            assert!(a > 0 && a <= t, "{a} of {t}");
        }
    }

    #[test]
    fn reads_this_disk() {
        let (free, total) = disk_space(&std::env::temp_dir()).expect("disk space");
        assert!(total > 0 && free <= total, "{free} of {total}");
    }

    #[test]
    fn meminfo_lines() {
        let text = "MemTotal:       16314288 kB\nMemFree:          812340 kB\nMemAvailable:    9120000 kB\n";
        assert_eq!(meminfo_value(text, "MemTotal"), Some(16_314_288 * 1024));
        assert_eq!(meminfo_value(text, "MemAvailable"), Some(9_120_000 * 1024));
        assert_eq!(meminfo_value(text, "Mem"), None);
    }
}
