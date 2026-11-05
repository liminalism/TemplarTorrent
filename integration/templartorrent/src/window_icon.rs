//! The window's title-bar and taskbar icons, Windows only.
//!
//! The program icon is linked into `templartorrent.exe` by `build.rs` as
//! resource group 1, with 16 to 256 px images (assets/icon/). Explorer and
//! desktop shortcuts read it from the file. A window does not inherit it,
//! because winit registers its window class without an icon, so this module
//! loads two sizes for the window's current DPI and sets them:
//! - `ICON_SMALL`: the title bar (16 px at 100% scaling, 24 at 150%, 32 at 200%);
//! - `ICON_BIG`: the taskbar and Alt-Tab (32 px at 100%, 48 at 150%, 64 at 200%).
//!
//! Each is loaded at exactly the size Windows asks for, so it picks the
//! nearest image rendered for that size rather than stretching another one.
//! A move to a monitor with a different scale reloads them
//! ([`WindowIcon::apply`] again from `on_scale`). Elsewhere this is a no-op.
#![allow(unsafe_code)]

/// Resource id of the icon group `build.rs` writes.
#[cfg(windows)]
const ICON_GROUP_ID: u16 = 1;

#[derive(Debug, Default)]
pub struct WindowIcon {
    /// Handles currently set on the window: small, big.
    #[cfg(windows)]
    current: Option<(isize, isize)>,
}

impl WindowIcon {
    /// Set the title-bar and taskbar icons on `hwnd` at its current DPI,
    /// replacing (and freeing) any set before.
    pub fn apply(&mut self, hwnd: Option<isize>) {
        #[cfg(windows)]
        if let Some(hwnd) = hwnd {
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DestroyIcon, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW,
                SM_CXICON, SM_CXSMICON, SendMessageW, WM_SETICON,
            };
            let window = hwnd as windows_sys::Win32::Foundation::HWND;
            // SAFETY: `window` is this process's live main window; the module
            // handle of the running executable is always valid; the resource
            // id is passed as an integer resource name (MAKEINTRESOURCEW).
            // Each loaded icon is owned here until replaced.
            unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let dpi = match GetDpiForWindow(window) {
                    0 => 96,
                    dpi => dpi,
                };
                let load = |metric| {
                    let side = GetSystemMetricsForDpi(metric, dpi);
                    LoadImageW(
                        module,
                        ICON_GROUP_ID as usize as *const u16,
                        IMAGE_ICON,
                        side,
                        side,
                        LR_DEFAULTCOLOR,
                    ) as isize
                };
                let small = load(SM_CXSMICON);
                let big = load(SM_CXICON);
                if small == 0 || big == 0 {
                    // No icon resource (a non-MSVC build): keep the default.
                    for handle in [small, big] {
                        if handle != 0 {
                            DestroyIcon(handle as _);
                        }
                    }
                    return;
                }
                SendMessageW(window, WM_SETICON, ICON_SMALL as usize, small);
                SendMessageW(window, WM_SETICON, ICON_BIG as usize, big);
                if let Some((old_small, old_big)) = self.current.replace((small, big)) {
                    DestroyIcon(old_small as _);
                    DestroyIcon(old_big as _);
                }
            }
        }
        #[cfg(not(windows))]
        let _ = hwnd;
    }
}

#[cfg(all(test, windows))]
mod tests {
    /// Library tests are not linked with the icon resource, so loading it
    /// fails and the window keeps its default icon instead of breaking.
    #[test]
    fn a_missing_icon_resource_leaves_the_window_alone() {
        let mut icon = super::WindowIcon::default();
        icon.apply(None);
        assert!(icon.current.is_none());
    }
}
