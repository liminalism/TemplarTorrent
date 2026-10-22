//! Available bytes on the filesystem that holds a destination. This is a
//! display hint only. Deletion never uses it.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::path::Path;

pub fn available_bytes(path: &Path) -> Option<u64> {
    let mut probe = path.to_path_buf();
    if !probe.exists() {
        probe.pop();
    }
    if probe.as_os_str().is_empty() {
        probe.push(".");
    }
    let text = probe.to_str()?;
    let c_path = CString::new(text).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    stat.f_frsize.checked_mul(stat.f_bavail)
}
