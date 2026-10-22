//! Production TemplarTorrent window. Pass `--font` or rely on a system
//! TrueType face. No font files are shipped in this tree.

use std::io::Read;
use std::path::Path;

use pixelkit_text::font::{Face, FontSet};
use pixelkit_windowing::{WindowConfig, run_app};
use templartorrent::activate::{
    Activation, activate, default_runtime_dir, spawn, valid_activation,
};
use templartorrent::geo;
use templartorrent::ui::App;
use templartorrent::worker::{WorkerConfig, WorkerHandle};
use templartorrent::workflow::{
    self, Launch, USAGE, load_public_trackers, public_tracker_path, resolve_font,
};
use templartorrent_core::model::Limits;

fn main() {
    let launch = match workflow::parse_args(std::env::args().skip(1)) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("templartorrent: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(launch) {
        eprintln!("templartorrent: {error}");
        std::process::exit(1);
    }
}

fn run(launch: Launch) -> Result<(), String> {
    let Launch::Run {
        font,
        geoip_db,
        allow_local_trackers,
        payloads,
    } = launch
    else {
        println!("{USAGE}");
        return Ok(());
    };
    for payload in &payloads {
        if !valid_activation(payload) {
            return Err("each argument must be one magnet or a .torrent path".into());
        }
    }
    let activation =
        activate(&default_runtime_dir(), &payloads).map_err(|error| error.to_string())?;
    let Activation::Primary {
        listener,
        path,
        payloads,
    } = activation
    else {
        return Ok(());
    };
    let fonts = load_fonts(&resolve_font(font.as_deref())?)?;
    let tracker_path = public_tracker_path();
    let client = WorkerHandle::start(WorkerConfig {
        limits: Limits::default(),
        allow_local_trackers,
        public_trackers: load_public_trackers(&tracker_path),
        public_tracker_path: tracker_path,
        geo_db_path: geo::resolve_db_path(geoip_db.as_deref()),
        listen: "0.0.0.0:0"
            .parse()
            .map_err(|_| "the listen address is invalid".to_string())?,
        session_dir: Some(workflow::data_dir().join("session")),
    })?;
    let activator = spawn(listener, path);
    let mut app = App::new(fonts, client, activator);
    app.prime(payloads);
    // Transmission GTK's default main window is 300×500. A larger opening
    // leaves an empty download list. The minimum stays under that default.
    let mut config = WindowConfig::new("TemplarTorrent", 300.0, 500.0).min_size(260.0, 320.0);
    config.app_id = Some("templartorrent".into());
    run_app(app, config).map_err(|error| error.to_string())
}

fn load_fonts(path: &Path) -> Result<&'static FontSet, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "could not open the font".to_string())?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read the font".to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("font exceeds 4 MiB".into());
    }
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let face = Face::from_bytes(bytes).ok_or("font must be a static TrueType file")?;
    let face: &'static Face = Box::leak(Box::new(face));
    let mut fonts = FontSet::new();
    fonts.add(face);
    Ok(Box::leak(Box::new(fonts)))
}
