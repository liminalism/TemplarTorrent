//! Linux TemplarTorrent client. The window submits typed commands; a worker
//! thread owns the rbit driver. Fixture rows stay in the preview binary.
#![deny(unsafe_code)]

pub mod activate;
pub mod geo;
mod space;
pub mod ui;
pub mod worker;
pub mod workflow;

#[cfg(test)]
mod source_tests {
    #[test]
    fn production_sources_have_no_preview_fixtures() {
        const FILES: &[&str] = &[
            include_str!("lib.rs"),
            include_str!("main.rs"),
            include_str!("ui.rs"),
            include_str!("worker.rs"),
            include_str!("workflow.rs"),
            include_str!("activate.rs"),
            include_str!("geo.rs"),
            include_str!("space.rs"),
        ];
        for kind in ["series", "album"] {
            let needle = format!("Example {kind}");
            for source in FILES {
                assert!(
                    !source.contains(&needle),
                    "preview fixture name leaked into the production client"
                );
            }
        }
    }
}
