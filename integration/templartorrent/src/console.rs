//! Where messages go when the release binary has no console of its own.
//!
//! On Windows the executable is a GUI-subsystem program, so a browser or
//! Explorer launching it for a magnet or `.torrent` does not flash a console
//! window. Run from a terminal it attaches to that terminal's console, so
//! `--help`, `--register` and errors still print. Launched with no console,
//! an error is shown in a message box instead of vanishing. Elsewhere both
//! are plain stdout/stderr.
#![allow(unsafe_code)]

/// Attach to the parent's console when there is one. Returns whether output
/// written to stdout/stderr will be visible.
pub fn attach_parent() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        // SAFETY: no pointers; fails harmlessly when there is no parent
        // console or one is already attached.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
    }
    #[cfg(not(windows))]
    true
}

/// Report a fatal launch error where the user will see it.
pub fn report_error(visible_console: bool, message: &str) {
    eprintln!("templartorrent: {message}");
    #[cfg(windows)]
    if !visible_console {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = crate::text::t("console.error_title")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: both strings are NUL-terminated UTF-16; no owner window.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = visible_console;
}
