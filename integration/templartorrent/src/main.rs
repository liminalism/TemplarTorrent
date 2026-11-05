//! Production TemplarTorrent window. Pass `--font` or rely on a system
//! TrueType face. No font files are shipped in this tree.
// No console window when a browser or Explorer opens a magnet or .torrent
// with this program; a terminal launch reattaches (see `console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::Read;
use std::path::Path;

use templartorrent::console;

use liminalkit_text::font::{Face, FontSet};
use liminalkit_windowing::{WindowConfig, run_app};
use templartorrent::activate::{
    Activation, activate, default_runtime_dir, spawn, valid_activation,
};
use templartorrent::geo;
use templartorrent::persist::{SessionStore, UiState};
use templartorrent::text::{t, tf};
use templartorrent::ui::App;
use templartorrent::worker::{WorkerConfig, WorkerHandle};
use templartorrent::workflow::{
    self, Launch, load_public_trackers, public_tracker_path, resolve_font,
};
use templartorrent_core::model::Limits;

fn main() {
    // Each real torrent file keeps a handle open; storage admission checks
    // the budget this leaves.
    templartorrent_storage::raise_open_file_limit();
    let visible = console::attach_parent();
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let launch = match args
        .map_err(|_| t("cli.argument_not_unicode").to_string())
        .and_then(workflow::parse_args)
    {
        Ok(launch) => launch,
        Err(error) => {
            console::report_error(visible, &error);
            std::process::exit(2);
        }
    };
    if let Err(error) = run(launch) {
        console::report_error(visible, &error);
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn register() -> Result<(), String> {
    use templartorrent::winreg;
    let exe = std::env::current_exe()
        .map_err(|error| tf("cli.register_no_exe", &[("detail", &error)]))?;
    let exe = templartorrent_core::paths::canonical_dir(&exe).unwrap_or(exe);
    let registration =
        winreg::register(&exe).map_err(|error| tf("cli.register_failed", &[("detail", &error)]))?;
    println!("{}", tf("cli.registered", &[("path", &exe.display())]));
    if registration.is_magnet_default() {
        println!("{}", t("cli.registered_magnet_default"));
        println!("{}", t("cli.registered_torrent_hint"));
    } else {
        println!("{}", t("cli.registered_not_default"));
        let _ = winreg::open_default_apps_settings();
    }
    Ok(())
}

#[cfg(windows)]
fn unregister() -> Result<(), String> {
    templartorrent::winreg::unregister()
        .map_err(|error| tf("cli.unregister_failed", &[("detail", &error)]))?;
    println!("{}", t("cli.unregistered"));
    Ok(())
}

#[cfg(not(windows))]
fn register() -> Result<(), String> {
    Err(t("cli.register_windows_only").into())
}

#[cfg(not(windows))]
fn unregister() -> Result<(), String> {
    Err(t("cli.unregister_windows_only").into())
}

fn run(launch: Launch) -> Result<(), String> {
    match launch {
        Launch::Register => return register(),
        Launch::Unregister => return unregister(),
        _ => {}
    }
    let Launch::Run {
        font,
        geoip_db,
        allow_local_trackers,
        allow_local_urls,
        payloads,
    } = launch
    else {
        println!("{}", t("cli.usage"));
        return Ok(());
    };
    for payload in &payloads {
        if !valid_activation(payload) {
            return Err(t("cli.invalid_payload").into());
        }
    }
    let activation = activate(&default_runtime_dir(), &payloads)
        .map_err(|error| tf("cli.activation_failed", &[("detail", &error)]))?;
    let Activation::Primary {
        listener,
        path,
        payloads,
    } = activation
    else {
        return Ok(());
    };
    let fonts = load_fonts(&resolve_font(font.as_deref())?)?;
    let session_dir = workflow::data_dir().join("session");
    let store = SessionStore::new(session_dir.clone());
    let ui = store
        .load_ui()
        .ok()
        .flatten()
        .unwrap_or_default()
        .upgraded()
        .clamped();
    let tracker_path = public_tracker_path();
    let client = WorkerHandle::start(WorkerConfig {
        limits: Limits::default(),
        allow_local_trackers,
        allow_local_urls,
        public_trackers: load_public_trackers(&tracker_path),
        public_tracker_path: tracker_path,
        geo_db_path: geo::resolve_db_path(geoip_db.as_deref()),
        listen: "[::]:0"
            .parse()
            .map_err(|_| t("cli.listen_address_invalid").to_string())?,
        session_dir: Some(session_dir),
    })?;
    let activator = spawn(listener, path);
    let mut app = App::new(fonts, client, activator, ui, Some(store));
    app.prime(payloads);
    // The saved size, or the wide default in `UiState::default`.
    let mut config = WindowConfig::new(t("title.app"), f64::from(ui.width), f64::from(ui.height))
        .min_size(
            f64::from(UiState::MIN_WIDTH),
            f64::from(UiState::MIN_HEIGHT),
        );
    config.app_id = Some("templartorrent".into());
    run_app(app, config).map_err(|error| tf("cli.window_failed", &[("detail", &error)]))
}

fn load_fonts(path: &Path) -> Result<&'static FontSet, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| t("cli.font_open_failed").to_string())?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| t("cli.font_read_failed").to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(t("cli.font_too_large").into());
    }
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let face = Face::from_bytes(bytes).ok_or(t("cli.font_not_truetype"))?;
    let face: &'static Face = Box::leak(Box::new(face));
    let mut fonts = FontSet::new();
    fonts.add(face);
    Ok(Box::leak(Box::new(fonts)))
}
