//! TemplarTorrent client for Linux and Windows. The window submits typed
//! commands; a worker thread owns the rbit driver. Fixture rows stay in the
//! preview binary. Every message shown to the user comes from
//! `assets/messages.json` through [`text`].
#![deny(unsafe_code)]

pub mod activate;
pub mod console;
pub mod flags;
pub mod foreground;
pub mod geo;
pub mod persist;
pub mod picker;
mod space;
pub mod text;
pub mod ui;
pub mod window_icon;
#[cfg(windows)]
pub mod winreg;
pub mod worker;
pub mod workflow;

/// Every client source file except `text.rs` (which documents the key
/// syntax), for tests that scan the code: the messages-key check and the
/// fixture-leak check.
#[cfg(test)]
pub(crate) const SOURCES: &[&str] = &[
    include_str!("lib.rs"),
    include_str!("main.rs"),
    include_str!("ui.rs"),
    include_str!("worker.rs"),
    include_str!("workflow.rs"),
    include_str!("activate.rs"),
    include_str!("activate_sys.rs"),
    include_str!("console.rs"),
    include_str!("flags.rs"),
    include_str!("foreground.rs"),
    include_str!("geo.rs"),
    include_str!("persist.rs"),
    include_str!("picker.rs"),
    include_str!("space.rs"),
    include_str!("winreg.rs"),
    include_str!("window_icon.rs"),
];

#[cfg(test)]
mod source_tests {
    #[test]
    fn production_sources_have_no_preview_fixtures() {
        for kind in ["series", "album"] {
            let needle = format!("Example {kind}");
            for source in crate::SOURCES {
                assert!(
                    !source.contains(&needle),
                    "preview fixture name leaked into the production client"
                );
            }
        }
    }
}
