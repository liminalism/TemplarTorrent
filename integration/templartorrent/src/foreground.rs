//! Bring the window forward when another launch forwards a magnet or
//! `.torrent` to this instance (a browser link, a double-clicked file).
//!
//! Windows only. The forwarding process calls `AllowSetForegroundWindow`
//! before it connects (see `activate_sys.rs`), which is what lets this
//! process take the foreground without the user clicking it. Elsewhere this
//! is a no-op: X11/Wayland compositors decide focus themselves.
#![allow(unsafe_code)]

#[derive(Debug, Default)]
pub struct Foreground {
    #[cfg(windows)]
    hwnd: Option<isize>,
}

impl Foreground {
    /// Remember the native window. Called once per window creation.
    pub fn remember(&mut self, window: &winit::window::Window) {
        #[cfg(windows)]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            self.hwnd = match window.window_handle().map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get()),
                _ => None,
            };
        }
        #[cfg(not(windows))]
        let _ = window;
    }

    /// The main window's HWND, once known (Windows only).
    pub fn hwnd(&self) -> Option<isize> {
        #[cfg(windows)]
        {
            self.hwnd
        }
        #[cfg(not(windows))]
        None
    }

    /// Restore the window if minimized and ask for the foreground.
    pub fn raise(&self) {
        #[cfg(windows)]
        if let Some(hwnd) = self.hwnd {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
            };
            let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
            // SAFETY: `hwnd` came from this process's live window; these
            // calls take a window handle by value and tolerate a stale one.
            unsafe {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
            }
        }
    }
}
