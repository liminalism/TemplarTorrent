//! Native file and folder pickers on Windows (rfd, IFileOpenDialog).
//!
//! The dialog runs on its own thread, owned by the main window so it stays
//! in front of it, and the answer comes back through a channel plus a wake,
//! so the window keeps painting while the dialog is open. Linux keeps the
//! in-app folder browser: rfd there needs GTK or an XDG portal stack, a
//! second GUI stack the product rules exclude.

#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc;

/// Which field a finished pick fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// The download folder, or `None` when the user cancelled.
    Folder(Option<PathBuf>),
    /// A `.torrent` file, or `None` when the user cancelled.
    Torrent(Option<PathBuf>),
}

/// At most one dialog at a time; its answer is read in the next frame.
#[derive(Default)]
pub struct Pickers {
    pending: Option<mpsc::Receiver<Picked>>,
    /// Main window handle, the dialogs' owner.
    pub owner: Option<isize>,
    /// Wakes the window when a dialog closes.
    pub wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl Pickers {
    /// Whether this platform has a native picker; otherwise the caller uses
    /// the in-app browser.
    pub const fn native() -> bool {
        cfg!(windows)
    }

    pub fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// The finished pick, once.
    pub fn poll(&mut self) -> Option<Picked> {
        let receiver = self.pending.as_ref()?;
        match receiver.try_recv() {
            Ok(picked) => {
                self.pending = None;
                Some(picked)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                None
            }
        }
    }

    /// Open the folder picker starting at `start`.
    pub fn pick_folder(&mut self, title: String, start: PathBuf) {
        self.open(move |dialog| Picked::Folder(dialog_with(dialog, title, start).pick_folder()));
    }

    /// Open the `.torrent` picker starting at `start`.
    pub fn pick_torrent(&mut self, title: String, filter: String, start: PathBuf) {
        self.open(move |dialog| {
            Picked::Torrent(
                dialog_with(dialog, title, start)
                    .add_filter(filter, &["torrent"])
                    .pick_file(),
            )
        });
    }

    #[cfg(windows)]
    fn open(&mut self, run: impl FnOnce(rfd::FileDialog) -> Picked + Send + 'static) {
        if self.is_open() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let owner = self.owner;
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new()
            .name("templartorrent-picker".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new();
                if let Some(owner) = owner.and_then(Owner::new) {
                    dialog = dialog.set_parent(&owner);
                }
                let _ = sender.send(run(dialog));
                if let Some(wake) = wake {
                    wake();
                }
            });
        if spawned.is_ok() {
            self.pending = Some(receiver);
        }
    }

    #[cfg(not(windows))]
    fn open(&mut self, _run: impl FnOnce(NoDialog) -> Picked + Send + 'static) {}
}

#[cfg(windows)]
fn dialog_with(dialog: rfd::FileDialog, title: String, start: PathBuf) -> rfd::FileDialog {
    let dialog = dialog.set_title(title);
    if start.is_dir() {
        dialog.set_directory(start)
    } else {
        dialog
    }
}

/// Stand-in so the non-Windows signatures type-check; never constructed.
#[cfg(not(windows))]
pub struct NoDialog;

#[cfg(not(windows))]
impl NoDialog {
    fn pick_folder(self) -> Option<PathBuf> {
        None
    }
    fn pick_file(self) -> Option<PathBuf> {
        None
    }
    fn add_filter(self, _name: String, _extensions: &[&str]) -> Self {
        self
    }
}

#[cfg(not(windows))]
fn dialog_with(dialog: NoDialog, _title: String, _start: PathBuf) -> NoDialog {
    dialog
}

/// The main window as a dialog owner, from its raw HWND.
#[cfg(windows)]
struct Owner(winit::raw_window_handle::Win32WindowHandle);

#[cfg(windows)]
impl Owner {
    fn new(hwnd: isize) -> Option<Self> {
        std::num::NonZeroIsize::new(hwnd)
            .map(|hwnd| Self(winit::raw_window_handle::Win32WindowHandle::new(hwnd)))
    }
}

#[cfg(windows)]
impl winit::raw_window_handle::HasWindowHandle for Owner {
    fn window_handle(
        &self,
    ) -> Result<winit::raw_window_handle::WindowHandle<'_>, winit::raw_window_handle::HandleError>
    {
        let raw = winit::raw_window_handle::RawWindowHandle::Win32(self.0);
        // SAFETY: the HWND belongs to this process's main window, which
        // outlives the dialog it owns (the dialog closes with the process).
        Ok(unsafe { winit::raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

#[cfg(windows)]
impl winit::raw_window_handle::HasDisplayHandle for Owner {
    fn display_handle(
        &self,
    ) -> Result<winit::raw_window_handle::DisplayHandle<'_>, winit::raw_window_handle::HandleError>
    {
        Ok(winit::raw_window_handle::DisplayHandle::windows())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Opens the real folder dialog, closes it the way a user's Cancel does,
    /// and checks the answer and the wake arrive. Opt-in: it shows a window.
    #[test]
    #[ignore = "opens a real Windows dialog for about a second"]
    fn the_native_folder_dialog_opens_and_reports_a_cancel() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
        let title = format!("TemplarTorrent picker probe {}", std::process::id());
        let woken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woken);
        let mut pickers = Pickers {
            wake: Some(Arc::new(move || flag.store(true, Ordering::SeqCst))),
            ..Pickers::default()
        };
        pickers.pick_folder(title.clone(), std::env::temp_dir());
        assert!(pickers.is_open());
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let start = std::time::Instant::now();
        let mut closed = false;
        let picked = loop {
            if let Some(picked) = pickers.poll() {
                break picked;
            }
            assert!(start.elapsed().as_secs() < 20, "the dialog never answered");
            if !closed {
                // SAFETY: NUL-terminated title; posting WM_CLOSE to a dialog
                // window found by its unique title.
                let hwnd = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
                if !hwnd.is_null() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
                    closed = true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(closed, "the dialog window appeared with its title");
        assert_eq!(picked, Picked::Folder(None));
        assert!(
            woken.load(Ordering::SeqCst),
            "closing the dialog wakes the window"
        );
        assert!(!pickers.is_open());
    }
}
