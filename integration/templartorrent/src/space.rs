//! Available bytes on the filesystem that holds a destination. This is a
//! display hint only. Deletion never uses it.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

fn probe_dir(path: &Path) -> PathBuf {
    let mut probe = path.to_path_buf();
    if !probe.exists() {
        probe.pop();
    }
    if probe.as_os_str().is_empty() {
        probe.push(".");
    }
    probe
}

#[cfg(unix)]
pub fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    let probe = probe_dir(path);
    let text = probe.to_str()?;
    let c_path = CString::new(text).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    #[allow(clippy::useless_conversion)]
    u64::from(stat.f_frsize).checked_mul(u64::from(stat.f_bavail))
}

#[cfg(windows)]
pub fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let probe = probe_dir(path);
    let wide: Vec<u16> = probe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if wide[..wide.len() - 1].contains(&0) {
        return None;
    }
    let mut free_to_caller = 0u64;
    // SAFETY: `wide` is NUL-terminated; the out pointer is a valid u64 and
    // the two optional outputs are null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_to_caller)
}
