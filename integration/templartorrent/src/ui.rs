//! PixelKit window. Dialogs submit worker commands. Rates of zero render as
//! an em dash because the engine snapshot does not yet carry a smoothed rate.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use liminalkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use liminalkit_shell::{Input, KeyInput, Modifiers, MouseButton, Scale};
use liminalkit_text::font::{FaceId, FontSet};
use liminalkit_text::{Align, TextCache, TextStyle};
use liminalkit_ui::{
    Cell, CellAppearance, Column, Row, ScrollState, TableState, TextFieldState, Theme, Ui, navigate,
};
use liminalkit_windowing::{CursorShape, PaneMetrics, Panes, PixelApp, Side, Waker};
use templartorrent_core::model::{
    Command, JobState, PeerPreference, TorrentId, TorrentRow, clean_label,
};

use crate::activate::Activator;
use crate::flags;
use crate::persist::{SessionStore, UiState};
use crate::text::{t, tf};
use crate::worker::{
    FileRow, ListenState, PeerView, Request, SessionView, TrackerView, WorkerHandle,
};
use crate::workflow::{
    self, AddSource, LimitForm, affected_bytes, app_title, classify_source, cooldown_text,
    country_text, eta_text, format_bytes, move_file, normalize_magnet, parse_optional_sha256,
    percent_text, preference_note, preference_text, rank_label, rate_text, rtt_text, speed_cap,
    state_text, unknown, validate_destination,
};
use templartorrent_rbit_adapter::driver::PeerEncryption;
use templartorrent_rbit_adapter::http::UrlDetail;

const BG: u32 = 0xf4f4f2;
const PANEL: u32 = 0xffffff;
const INK: u32 = 0x242729;
const MUTED: u32 = 0x676d72;
const EDGE: u32 = 0xd3d6d8;
const SELECTED: u32 = 0xe6edf2;
const HOVER: u32 = 0xeef1f3;
/// PixelKit's table draws with these gaps. They are not re-exported.
const COLUMN_GAP: i32 = 8;
const TABLE_GUTTER: i32 = 10;

/// Light shell, same band structure as jxl-batch-studio: toolbar, panes, status
/// bar. Dimensional theme tokens are device pixels; font sizes stay logical.
fn app_theme(scale: Scale) -> Theme {
    Theme {
        background: BG,
        panel: PANEL,
        panel_alt: BG,
        panel_edge: EDGE,
        text: INK,
        text_dim: MUTED,
        alarm: 0xb42318,
        alarm_soft: 0xf8e8e6,
        good: 0x1f7a4d,
        good_soft: 0xe5f3ec,
        accent: 0x3d6f8f,
        accent_soft: SELECTED,
        hover: HOVER,
        selection: SELECTED,
        row_height: scale.px(30),
        body: TextStyle::new(FaceId(0), 13.0),
        heading: TextStyle::new(FaceId(0), 16.0),
        padding: scale.px(10),
        corner_radius: 0,
        border_width: scale.hairline().max(1),
        focus_width: 1,
    }
}

/// What a job-list column shows. The titles are user text, so cells are
/// matched on this, never on a title.
#[derive(Clone, Copy)]
enum JobColumn {
    Name,
    Progress,
    Down,
    Up,
    Peers,
    State,
}

fn download_columns(px: impl Fn(i32) -> i32, width: i32) -> (Vec<JobColumn>, Vec<Column>) {
    // 300 logical pixels is the opening width. Fixed columns beyond Name and
    // Progress would leave the name unreadably narrow until the window grows.
    let mut kinds = vec![JobColumn::Name, JobColumn::Progress];
    let mut columns = vec![
        Column::flex(t("jobs.column_name"), 4, Align::Left),
        Column::fixed(t("jobs.column_progress"), px(72), Align::Left),
    ];
    if width >= px(640) {
        kinds.extend([JobColumn::Down, JobColumn::Up]);
        columns.push(Column::fixed(t("jobs.column_down"), px(72), Align::Right));
        columns.push(Column::fixed(t("jobs.column_up"), px(72), Align::Right));
    }
    if width >= px(760) {
        kinds.push(JobColumn::Peers);
        columns.push(Column::fixed(t("jobs.column_peers"), px(64), Align::Left));
    }
    if width >= px(420) {
        // Right-aligned like the rates before it, so a right-aligned Up
        // value never runs into a left-aligned state name.
        kinds.push(JobColumn::State);
        columns.push(Column::fixed(t("jobs.column_state"), px(96), Align::Right));
    }
    (kinds, columns)
}

fn progress_parts(row: &TorrentRow) -> (String, f64) {
    if row.wanted_bytes == 0 {
        return (unknown().into(), 0.0);
    }
    let percent = row.verified_bytes.saturating_mul(100) / row.wanted_bytes;
    let fraction = (row.verified_bytes as f64 / row.wanted_bytes as f64).clamp(0.0, 1.0);
    (percent_text(percent), fraction)
}

fn state_ink(state: JobState, accent: u32, good: u32, alarm: u32, dim: u32) -> u32 {
    match state {
        JobState::Downloading | JobState::Metadata | JobState::Checking => accent,
        JobState::Seeding | JobState::Complete => good,
        JobState::Error => alarm,
        JobState::Paused | JobState::Queued | JobState::Removing => dim,
    }
}

fn job_cells(
    job: &TorrentRow,
    kinds: &[JobColumn],
    accent: u32,
    good: u32,
    alarm: u32,
    dim: u32,
) -> Vec<Cell<'static>> {
    let (progress, fraction) = progress_parts(job);
    let state_colour = state_ink(job.state, accent, good, alarm, dim);
    kinds
        .iter()
        .map(|kind| match kind {
            JobColumn::Name => Cell::new(clean_label(&job.name, 160)),
            JobColumn::Progress => Cell::meter(progress.clone(), fraction, accent),
            JobColumn::Down => Cell::new(rate_text(job.down_bps)),
            JobColumn::Up => Cell::new(rate_text(job.up_bps)),
            JobColumn::Peers => Cell::new(tf(
                "jobs.peers_cell",
                &[
                    ("seeds", &job.connected_seeds),
                    ("peers", &job.connected_peers),
                ],
            )),
            JobColumn::State => Cell::coloured(state_text(job.state), state_colour),
        })
        .collect()
}

/// The Limits dialog's fields, in `LimitForm` order.
const LIMIT_FIELDS: usize = 8;

fn limit_labels() -> [&'static str; LIMIT_FIELDS] {
    [
        t("limits.label_download_jobs"),
        t("limits.label_seed_jobs"),
        t("limits.label_upload_slots"),
        t("limits.label_connections"),
        t("limits.label_per_job"),
        t("limits.label_down_kb"),
        t("limits.label_up_kb"),
        t("limits.label_memory"),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Jobs,
    Inspector,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddFocus {
    Magnet,
    Torrent,
    Destination,
    Digest,
}

impl AddFocus {
    fn next(self) -> Self {
        match self {
            Self::Magnet => Self::Torrent,
            Self::Torrent => Self::Destination,
            Self::Destination => Self::Digest,
            Self::Digest => Self::Magnet,
        }
    }
}

struct AddDialog {
    /// A magnet, or one or more http(s) URLs.
    magnet: TextFieldState,
    torrent: TextFieldState,
    destination: TextFieldState,
    /// Optional SHA-256 for a single URL.
    digest: TextFieldState,
    public_bootstrap: bool,
    focus: AddFocus,
    error: String,
    browsing: bool,
    browse_path: PathBuf,
    entries: Vec<String>,
    browse_error: String,
    space: String,
    space_path: String,
    pending_list: Option<u64>,
    pending_space: Option<u64>,
    pending_torrent: Option<u64>,
    browse_scroll: ScrollState,
}

struct LimitsDialog {
    fields: [TextFieldState; LIMIT_FIELDS],
    focus: usize,
    error: String,
}

struct RemoveDialog {
    id: TorrentId,
    name: String,
    path: String,
    bytes: u64,
    delete_data: bool,
    error: String,
}

struct SettingsDialog {
    allow_local: bool,
    allow_local_urls: bool,
    encryption: PeerEncryption,
    port_mapping: bool,
    port: TextFieldState,
    bound_port: u16,
    error: String,
}

enum Dialog {
    None,
    Add(Box<AddDialog>),
    Limits(Box<LimitsDialog>),
    Remove(Box<RemoveDialog>),
    Settings(Box<SettingsDialog>),
}

enum Pending {
    Add,
    Remove,
    Limits,
    Other,
}

/// What a click can change mid-frame: inspector tab, which dialog is
/// open, and the selected job.
#[derive(PartialEq)]
struct FrameState {
    tab: usize,
    dialog: std::mem::Discriminant<Dialog>,
    selected: Option<TorrentId>,
}

/// Whether the Pause/Resume button resumes this job: paused, waiting for a
/// slot, or failed (a failed magnet fetch is retried by Resume).
fn resumable(state: JobState) -> bool {
    matches!(state, JobState::Paused | JobState::Queued | JobState::Error)
}

struct Desktop {
    panes: Panes<Pane>,
    jobs_table: TableState,
    cursor: CursorShape,
    client: WorkerHandle,
    activator: Activator,
    dialog: Dialog,
    tab: usize,
    selected: Option<TorrentId>,
    file_pick: Option<usize>,
    tracker_pick: usize,
    peer_pick: usize,
    file_scroll: ScrollState,
    peer_scroll: ScrollState,
    tracker_scroll: ScrollState,
    applied: u64,
    seq: u64,
    inflight: Option<(u64, Pending)>,
    status: String,
    last_title: String,
    pending_title: Option<String>,
    waiting: VecDeque<String>,
    /// Inspector share of the body as last laid out, saved on exit.
    split: f32,
    /// Window size in logical pixels as last rendered.
    logical_size: (f32, f32),
    store: Option<SessionStore>,
    /// Native folder and .torrent pickers (Windows).
    pickers: crate::picker::Pickers,
}

#[allow(clippy::too_many_arguments)]
impl Desktop {
    fn new(
        client: WorkerHandle,
        activator: Activator,
        ui: UiState,
        store: Option<SessionStore>,
    ) -> Self {
        let ui = ui.clamped();
        let mut panes = Panes::new(Pane::Jobs);
        if let Some((root, _)) = panes.panes(Rect::new(0, 0, 1100, 700)).first().copied() {
            // The new pane takes this fraction, so the download list stays larger.
            panes.split(root, Side::Bottom, Pane::Inspector, ui.split);
        }
        Self {
            pickers: crate::picker::Pickers::default(),
            panes,
            jobs_table: TableState::new(),
            cursor: CursorShape::Default,
            client,
            activator,
            dialog: Dialog::None,
            tab: 0,
            selected: None,
            file_pick: None,
            tracker_pick: 0,
            peer_pick: 0,
            file_scroll: ScrollState::new(),
            peer_scroll: ScrollState::new(),
            tracker_scroll: ScrollState::new(),
            applied: 0,
            seq: 0,
            inflight: None,
            status: match crate::text::override_problem() {
                Some((path, detail)) => tf(
                    "status.messages_override_ignored",
                    &[("path", &path.display()), ("detail", &detail)],
                ),
                None => t("status.ready").into(),
            },
            last_title: String::new(),
            pending_title: Some(t("title.app").into()),
            waiting: VecDeque::new(),
            split: ui.split,
            logical_size: (ui.width as f32, ui.height as f32),
            store,
        }
    }

    /// Persist geometry and split ratio; failures only lose the next
    /// launch's layout, so they are not surfaced at exit.
    fn save_ui(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let _ = store.save_ui(&UiState {
            width: self.logical_size.0.round().max(0.0) as u32,
            height: self.logical_size.1.round().max(0.0) as u32,
            split: self.split,
        });
    }

    pub fn prime(&mut self, payloads: Vec<String>) {
        for payload in payloads {
            self.queue_activation(payload);
        }
    }

    /// Apply worker acks and forwarded activations. Returns whether another
    /// launch forwarded a payload this frame, so the window can come forward.
    fn pump(&mut self) -> bool {
        let acks = self.client.view().acks;
        for ack in acks {
            if ack.seq <= self.applied {
                continue;
            }
            self.applied = ack.seq;
            self.apply_ack(ack);
        }
        if let Some(picked) = self.pickers.poll() {
            self.apply_pick(picked);
        }
        let mut activated = false;
        while let Some(payload) = self.activator.try_recv() {
            self.queue_activation(payload);
            activated = true;
        }
        self.poll_space();
        activated
    }

    fn apply_ack(&mut self, ack: crate::worker::Ack) {
        let mut consumed = false;
        if let Dialog::Add(add) = &mut self.dialog {
            if add.pending_list == Some(ack.seq) {
                add.pending_list = None;
                consumed = true;
                if ack.accepted {
                    add.entries = ack.entries.clone();
                    add.browse_error.clear();
                } else {
                    add.browse_error = ack.message.clone();
                }
            }
            if add.pending_space == Some(ack.seq) {
                add.pending_space = None;
                consumed = true;
                add.space = ack
                    .space
                    .map(format_bytes)
                    .unwrap_or_else(|| unknown().into());
            }
            if add.pending_torrent == Some(ack.seq) {
                add.pending_torrent = None;
                consumed = true;
                if let Some(magnet) = ack.magnet.clone() {
                    add.magnet = TextFieldState::with_text(magnet);
                    add.error.clear();
                } else if !ack.accepted {
                    add.error = ack.message.clone();
                }
            }
        }
        if consumed {
            return;
        }
        let pending = self
            .inflight
            .take_if(|pending| pending.0 == ack.seq)
            .map(|(_, kind)| kind);
        if !ack.accepted {
            self.status = ack.message.clone();
            self.show_dialog_error(ack.message);
            return;
        }
        if ack.message.is_empty() {
            self.status = t("status.command_accepted").into();
        } else {
            self.status = ack.message;
        }
        if let Some(Pending::Add | Pending::Remove | Pending::Limits) = pending {
            self.close_dialog();
        }
    }

    fn show_dialog_error(&mut self, message: String) {
        match &mut self.dialog {
            Dialog::Add(add) => add.error = message,
            Dialog::Limits(limits) => limits.error = message,
            Dialog::Remove(remove) => remove.error = message,
            _ => {}
        }
    }

    fn poll_space(&mut self) {
        let Dialog::Add(add) = &self.dialog else {
            return;
        };
        if add.browsing || add.pending_space.is_some() {
            return;
        }
        let path = add.destination.text().trim().to_string();
        if path.is_empty() || path == add.space_path {
            return;
        }
        let seq = self.seq.wrapping_add(1);
        if self
            .client
            .try_request(Request::Space {
                seq,
                path: PathBuf::from(&path),
            })
            .is_err()
        {
            return;
        }
        self.seq = seq;
        if let Dialog::Add(add) = &mut self.dialog {
            add.pending_space = Some(seq);
            add.space_path = path;
        }
    }

    /// Send without tracking an acknowledgement: the earlier URLs of a batch.
    /// Their rejections still reach the status line through `apply_ack`.
    fn submit_quiet(&mut self, command: Command) -> bool {
        let seq = self.seq.wrapping_add(1);
        if let Err(reason) = self.client.try_request(Request::Command { seq, command }) {
            self.status = reason.into();
            return false;
        }
        self.seq = seq;
        true
    }

    fn submit(&mut self, command: Command) {
        let kind = match &command {
            Command::Add { .. } | Command::AddUrl { .. } => Pending::Add,
            Command::Remove { .. } => Pending::Remove,
            Command::SetLimits(_) => Pending::Limits,
            _ => Pending::Other,
        };
        if self.inflight.is_some() && !matches!(kind, Pending::Other) {
            self.status = t("status.previous_running").into();
            return;
        }
        let seq = self.seq.wrapping_add(1);
        if let Err(reason) = self.client.try_request(Request::Command { seq, command }) {
            self.status = reason.into();
            return;
        }
        self.seq = seq;
        if !matches!(kind, Pending::Other) {
            self.inflight = Some((seq, kind));
        }
        self.status = t("status.working").into();
    }

    fn send(&mut self, request: Request) -> Option<u64> {
        let seq = self.seq.wrapping_add(1);
        let request = match request {
            Request::ListDir { path, .. } => Request::ListDir { seq, path },
            Request::ReadTorrent { path, .. } => Request::ReadTorrent { seq, path },
            Request::Space { path, .. } => Request::Space { seq, path },
            other => other,
        };
        if self.client.try_request(request).is_err() {
            self.status = t("status.queue_full").into();
            return None;
        }
        self.seq = seq;
        Some(seq)
    }

    fn queue_activation(&mut self, payload: String) {
        if matches!(self.dialog, Dialog::None) {
            self.open_activation(payload);
        } else {
            self.waiting.push_back(payload);
            self.status = t("status.activation_waiting").into();
        }
    }

    fn open_add(&mut self) {
        self.dialog = Dialog::Add(Box::new(AddDialog {
            magnet: TextFieldState::new(),
            torrent: TextFieldState::new(),
            destination: TextFieldState::new(),
            digest: TextFieldState::new(),
            public_bootstrap: false,
            focus: AddFocus::Magnet,
            error: String::new(),
            browsing: false,
            browse_path: PathBuf::new(),
            entries: Vec::new(),
            browse_error: String::new(),
            space: unknown().into(),
            space_path: String::new(),
            pending_list: None,
            pending_space: None,
            pending_torrent: None,
            browse_scroll: ScrollState::new(),
        }));
    }

    fn open_activation(&mut self, payload: String) {
        self.open_add();
        if let Ok(magnet) = normalize_magnet(&payload) {
            if let Dialog::Add(add) = &mut self.dialog {
                add.magnet = TextFieldState::with_text(magnet);
                add.focus = AddFocus::Destination;
            }
            return;
        }
        if payload.starts_with("http://") || payload.starts_with("https://") {
            if let Dialog::Add(add) = &mut self.dialog {
                add.magnet = TextFieldState::with_text(payload);
                add.focus = AddFocus::Destination;
            }
            return;
        }
        if let Dialog::Add(add) = &mut self.dialog {
            add.torrent = TextFieldState::with_text(payload.clone());
        }
        if let Some(seq) = self.send(Request::ReadTorrent {
            seq: 0,
            path: PathBuf::from(payload),
        }) && let Dialog::Add(add) = &mut self.dialog
        {
            add.pending_torrent = Some(seq);
            add.error = t("add.reading_torrent").into();
        }
    }

    fn close_dialog(&mut self) {
        self.dialog = Dialog::None;
        if let Some(payload) = self.waiting.pop_front() {
            self.open_activation(payload);
        }
    }

    fn close_layer(&mut self) {
        if let Dialog::Add(add) = &mut self.dialog
            && add.browsing
        {
            add.browsing = false;
            return;
        }
        self.close_dialog();
    }

    fn confirm_add(&mut self) {
        let Dialog::Add(add) = &self.dialog else {
            return;
        };
        let source = add.magnet.text().to_string();
        let destination = add.destination.text().trim().to_string();
        let digest = add.digest.text().to_string();
        let public_magnet_bootstrap = add.public_bootstrap;
        let source = match classify_source(&source) {
            Ok(source) => source,
            Err(error) => {
                if let Dialog::Add(add) = &mut self.dialog {
                    add.error = error;
                }
                return;
            }
        };
        if let Err(error) = validate_destination(&destination) {
            if let Dialog::Add(add) = &mut self.dialog {
                add.error = error.into();
            }
            return;
        }
        let destination = PathBuf::from(destination);
        match source {
            AddSource::Magnet(magnet) => {
                if !digest.trim().is_empty() {
                    if let Dialog::Add(add) = &mut self.dialog {
                        add.error = t("add.sha256_not_for_torrent").into();
                    }
                    return;
                }
                self.submit(Command::Add {
                    source: magnet,
                    destination,
                    public_magnet_bootstrap,
                });
            }
            AddSource::Urls(urls) => {
                let sha256 = match parse_optional_sha256(&digest) {
                    Ok(digest) => digest,
                    Err(error) => {
                        if let Dialog::Add(add) = &mut self.dialog {
                            add.error = error.into();
                        }
                        return;
                    }
                };
                if sha256.is_some() && urls.len() > 1 {
                    if let Dialog::Add(add) = &mut self.dialog {
                        add.error = t("add.sha256_one_url").into();
                    }
                    return;
                }
                if self.inflight.is_some() {
                    self.status = t("status.previous_running").into();
                    return;
                }
                let last = urls.len() - 1;
                for (index, url) in urls.into_iter().enumerate() {
                    let command = Command::AddUrl {
                        url,
                        destination: destination.clone(),
                        sha256,
                    };
                    if index == last {
                        self.submit(command);
                    } else if !self.submit_quiet(command) {
                        return;
                    }
                }
            }
        }
    }

    fn confirm_limits(&mut self) {
        let Dialog::Limits(dialog) = &self.dialog else {
            return;
        };
        let texts: Vec<String> = dialog
            .fields
            .iter()
            .map(|field| field.text().to_string())
            .collect();
        match workflow::parse_limits(LimitForm {
            download_jobs: &texts[0],
            seed_jobs: &texts[1],
            upload_slots: &texts[2],
            connections: &texts[3],
            per_job: &texts[4],
            down_kb: &texts[5],
            up_kb: &texts[6],
            memory_mib: &texts[7],
        }) {
            Ok(limits) => self.submit(Command::SetLimits(limits)),
            Err(error) => {
                if let Dialog::Limits(dialog) = &mut self.dialog {
                    dialog.error = error.into();
                }
            }
        }
    }

    fn load_torrent_field(&mut self) {
        let Dialog::Add(add) = &self.dialog else {
            return;
        };
        let path = add.torrent.text().trim().to_string();
        if path.is_empty() {
            if crate::picker::Pickers::native() {
                let start = crate::workflow::home_dir()
                    .map(|home| home.join("Downloads"))
                    .unwrap_or_default();
                self.pickers.pick_torrent(
                    t("add.pick_torrent_title").into(),
                    t("add.pick_torrent_filter").into(),
                    start,
                );
                return;
            }
            if let Dialog::Add(add) = &mut self.dialog {
                add.error = t("add.choose_torrent").into();
            }
            return;
        }
        if let Some(seq) = self.send(Request::ReadTorrent {
            seq: 0,
            path: PathBuf::from(path),
        }) && let Dialog::Add(add) = &mut self.dialog
        {
            add.pending_torrent = Some(seq);
            add.error = t("add.reading_torrent").into();
        }
    }

    fn begin_browse(&mut self) {
        let Dialog::Add(add) = &self.dialog else {
            return;
        };
        let typed = PathBuf::from(add.destination.text().trim());
        let start = if typed.is_dir() {
            typed
        } else if typed.parent().is_some_and(|parent| parent.is_dir()) {
            typed
                .parent()
                .unwrap_or(std::path::Path::new("/"))
                .to_path_buf()
        } else if let Some(home) = crate::workflow::home_dir() {
            if home.is_dir() {
                home
            } else {
                PathBuf::from("/")
            }
        } else {
            PathBuf::from("/")
        };
        if crate::picker::Pickers::native() {
            self.pickers
                .pick_folder(t("add.pick_folder_title").into(), start);
            return;
        }
        if let Dialog::Add(add) = &mut self.dialog {
            add.browsing = true;
            add.browse_path = start.clone();
            add.entries.clear();
            add.browse_error.clear();
            add.browse_scroll = ScrollState::new();
        }
        self.request_list(start);
    }

    fn request_list(&mut self, path: PathBuf) {
        if let Some(seq) = self.send(Request::ListDir { seq: 0, path })
            && let Dialog::Add(add) = &mut self.dialog
        {
            add.pending_list = Some(seq);
        }
    }

    fn browse_to(&mut self, path: PathBuf) {
        if let Dialog::Add(add) = &mut self.dialog {
            add.browse_path = path.clone();
            add.entries.clear();
            add.browse_scroll = ScrollState::new();
        }
        self.request_list(path);
    }

    /// Fill the Add dialog from a closed native picker. A cancel changes
    /// nothing.
    fn apply_pick(&mut self, picked: crate::picker::Picked) {
        use crate::picker::Picked;
        let Dialog::Add(add) = &mut self.dialog else {
            return;
        };
        match picked {
            Picked::Folder(Some(path)) => {
                add.destination = TextFieldState::with_text(path.display().to_string());
                add.space_path.clear();
                add.focus = AddFocus::Destination;
            }
            Picked::Torrent(Some(path)) => {
                add.torrent = TextFieldState::with_text(path.display().to_string());
                self.load_torrent_field();
            }
            Picked::Folder(None) | Picked::Torrent(None) => {}
        }
    }

    fn use_folder(&mut self) {
        let Dialog::Add(add) = &self.dialog else {
            return;
        };
        let path = add.browse_path.display().to_string();
        if let Dialog::Add(add) = &mut self.dialog {
            add.destination = TextFieldState::with_text(path);
            add.browsing = false;
            add.space_path.clear();
            add.focus = AddFocus::Destination;
        }
    }

    fn pause_or_resume(&mut self, view: &SessionView) {
        let Some(id) = self.selected else {
            self.status = t("status.select_torrent").into();
            return;
        };
        let paused = view
            .snapshot
            .jobs
            .iter()
            .find(|job| job.id == id)
            .is_some_and(|job| resumable(job.state));
        if paused {
            self.submit(Command::Resume(id));
        } else {
            self.submit(Command::Pause(id));
        }
    }

    fn open_remove(&mut self, view: &SessionView) {
        let Some(id) = self.selected else {
            self.status = t("status.select_torrent").into();
            return;
        };
        let Some(job) = view.snapshot.jobs.iter().find(|job| job.id == id) else {
            self.status = t("status.select_torrent").into();
            return;
        };
        let bytes = if let Some(detail) = view
            .url_detail
            .as_ref()
            .filter(|_| view.selected == Some(id))
        {
            detail.size.unwrap_or(detail.done)
        } else if view.selected == Some(id) {
            affected_bytes(
                &view
                    .files
                    .iter()
                    .map(|file| file.length)
                    .collect::<Vec<_>>(),
            )
        } else {
            job.wanted_bytes
        };
        self.dialog = Dialog::Remove(Box::new(RemoveDialog {
            id,
            name: clean_label(&job.name, 160),
            path: view
                .destination
                .clone()
                .filter(|_| view.selected == Some(id))
                .unwrap_or_else(|| unknown().into()),
            bytes,
            delete_data: false,
            error: String::new(),
        }));
    }

    fn open_limits(&mut self, view: &SessionView) {
        let limits = &view.limits;
        let memory = limits.payload_budget_bytes / (1024 * 1024);
        let values = [
            limits.download_jobs.to_string(),
            limits.seed_jobs.to_string(),
            limits.upload_peer_slots.to_string(),
            limits.connections_total.to_string(),
            limits.connections_per_job.to_string(),
            limits.download_kb_s.to_string(),
            limits.upload_kb_s.to_string(),
            memory.to_string(),
        ];
        self.dialog = Dialog::Limits(Box::new(LimitsDialog {
            fields: values.map(TextFieldState::with_text),
            focus: 0,
            error: String::new(),
        }));
    }

    fn frame_state(&self) -> FrameState {
        FrameState {
            tab: self.tab,
            dialog: std::mem::discriminant(&self.dialog),
            selected: self.selected,
        }
    }

    fn open_settings(&mut self, view: &SessionView) {
        let bound_port = match &view.listen {
            ListenState::Bound(address) => address.port(),
            _ => 0,
        };
        self.dialog = Dialog::Settings(Box::new(SettingsDialog {
            allow_local: view.allow_local_trackers,
            allow_local_urls: view.allow_local_urls,
            encryption: view.encryption,
            port_mapping: view.port_mapping,
            port: TextFieldState::with_text(bound_port.to_string()),
            bound_port,
            error: String::new(),
        }));
    }

    fn sync_selection(&mut self, view: &SessionView) {
        if self
            .selected
            .is_some_and(|id| view.snapshot.jobs.iter().all(|job| job.id != id))
        {
            self.selected = None;
            self.file_pick = None;
            self.client.set_selected(None);
        }
        if self.selected.is_none()
            && let Some(job) = view.snapshot.jobs.first()
        {
            self.selected = Some(job.id);
            self.client.set_selected(Some(job.id));
        }
    }

    fn draw_shell(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        width: i32,
        height: i32,
        interactive: bool,
    ) {
        let full = Rect::new(0, 0, width, height);
        self.logical_size = (s.logical(width), s.logical(height));
        let (toolbar, below) = full.split_top(s.px(36));
        let (statusbar, body_band) = below.split_bottom(s.px(24));
        let area = body_band.inset(s.px(8));
        let metrics = PaneMetrics::scaled(s);
        if interactive {
            self.panes.update(ui.input, area, metrics);
            self.cursor = self.panes.hover_cursor(ui.input, area, metrics);
        }
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        let small = TextStyle::new(FaceId(0), s.font(11.0));
        self.toolbar(ui, view, s, toolbar, interactive);
        for (id, rect) in self.panes.panes(area) {
            let Some(kind) = self.panes.get(id).copied() else {
                continue;
            };
            if matches!(kind, Pane::Inspector) && area.h > 0 {
                self.split = (rect.h as f32 / area.h as f32).clamp(0.1, 0.9);
            }
            let inner = ui.flat_panel(rect.inset(s.px(1)), PANEL, EDGE, false);
            match kind {
                Pane::Jobs => self.draw_jobs(ui, view, s, inner, &body, interactive),
                Pane::Inspector => {
                    self.draw_inspector(ui, view, s, inner, &body, &small, interactive)
                }
            }
        }
        self.status_bar(ui, view, statusbar);
    }

    fn toolbar(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        area: Rect,
        interactive: bool,
    ) {
        ui.painter.fill_rect(area, ui.theme.panel);
        ui.painter.horizontal_rule(
            area.x,
            area.bottom() - s.hairline().max(1),
            area.w,
            ui.theme.panel_edge,
        );
        let (pause_label, selected_name) = {
            let selected_job = view
                .snapshot
                .jobs
                .iter()
                .find(|job| self.selected == Some(job.id));
            let pause_label = if selected_job.is_some_and(|job| resumable(job.state)) {
                t("toolbar.resume")
            } else {
                t("toolbar.pause")
            };
            let selected_name = selected_job
                .map(|job| clean_label(&job.name, 80))
                .unwrap_or_else(|| t("toolbar.no_selection").into());
            (pause_label, selected_name)
        };
        let labels = [
            t("toolbar.add"),
            pause_label,
            t("toolbar.remove"),
            t("toolbar.limits"),
            t("toolbar.settings"),
        ];
        // Minima hold the labels at the 13px body size inside a 300px window.
        let minima = [s.px(36), s.px(58), s.px(58), s.px(54), s.px(68)];
        let pad = s.px(4);
        let gap = s.px(3);
        let slots = labels.len() as i32;
        let available = (area.w - pad * 2 - gap * (slots - 1)).max(slots);
        let min_sum: i32 = minima.iter().sum();
        let h = s.px(26).min((area.h - s.px(6)).max(1));
        let y = area.y + (area.h - h) / 2;
        let mut x = area.x + pad;
        for (index, label) in labels.iter().enumerate() {
            let width = if available >= min_sum {
                minima[index] + (available - min_sum) * minima[index] / min_sum
            } else {
                (available * minima[index] / min_sum).max(1)
            };
            let rect = Rect::new(x, y, width, h);
            x = rect.right() + gap;
            if !interactive {
                ui.label(rect, label, MUTED, Align::Centre);
                continue;
            }
            if !ui.button(rect, label) {
                continue;
            }
            match index {
                0 => self.open_add(),
                1 => self.pause_or_resume(view),
                2 => self.open_remove(view),
                3 => self.open_limits(view),
                _ => self.open_settings(view),
            }
        }
        let name = Rect::new(x + s.px(4), y, (area.right() - s.px(4) - x).max(0), h);
        if name.w > s.px(40) {
            ui.label(name, &selected_name, MUTED, Align::Right);
        }
    }

    fn status_bar(&self, ui: &mut Ui<'_>, view: &SessionView, area: Rect) {
        ui.painter.fill_rect(area, ui.theme.panel);
        ui.painter
            .horizontal_rule(area.x, area.y, area.w, ui.theme.panel_edge);
        let down: u64 = view
            .snapshot
            .jobs
            .iter()
            .map(|job| job.down_bps)
            .fold(0, u64::saturating_add);
        let up: u64 = view
            .snapshot
            .jobs
            .iter()
            .map(|job| job.up_bps)
            .fold(0, u64::saturating_add);
        let active = view
            .snapshot
            .jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.state,
                    JobState::Downloading | JobState::Metadata | JobState::Checking
                )
            })
            .count();
        let jobs = view.snapshot.jobs.len();
        let down = rate_text(down);
        let up = rate_text(up);
        let down_limit = speed_cap(view.limits.download_kb_s);
        let up_limit = speed_cap(view.limits.upload_kb_s);
        let values: &[(&str, &dyn std::fmt::Display)] = &[
            ("down", &down),
            ("up", &up),
            ("down_limit", &down_limit),
            ("up_limit", &up_limit),
            ("active", &active),
            ("jobs", &jobs),
        ];
        let footer = if area.w < ui.px(420) {
            tf("status.footer_narrow", values)
        } else if area.w < ui.px(720) {
            tf("status.footer_medium", values)
        } else {
            let listen = listen_text(&view.listen);
            let wide = [values, &[("listen", &listen), ("status", &self.status)]].concat();
            tf("status.footer_wide", &wide)
        };
        ui.label(
            Rect::new(
                area.x + ui.px(8),
                area.y,
                (area.w - ui.px(16)).max(1),
                area.h,
            ),
            &footer,
            MUTED,
            Align::Left,
        );
    }

    fn draw_jobs(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        inner: Rect,
        body: &TextStyle,
        interactive: bool,
    ) {
        let caption_h = s.px(26);
        ui.label_styled(
            Rect::new(
                inner.x + s.px(12),
                inner.y + s.px(2),
                (inner.w - s.px(24)).max(1),
                caption_h,
            ),
            t("jobs.caption"),
            *body,
            INK,
            Align::Left,
        );
        let table_area = Rect::new(
            inner.x + s.px(8),
            inner.y + caption_h,
            (inner.w - s.px(16)).max(1),
            (inner.h - caption_h - s.px(6)).max(1),
        );
        let (kinds, columns) = download_columns(|logical| s.px(logical), table_area.w);
        if !interactive {
            self.paint_jobs_static(ui, view, table_area, &kinds, &columns, body);
            return;
        }
        let keys: Vec<String> = view
            .snapshot
            .jobs
            .iter()
            .map(|job| job.id.0.to_string())
            .collect();
        let wanted = self.selected.map(|id| id.0.to_string());
        if self.jobs_table.selected() != wanted.as_deref() {
            self.jobs_table.select(wanted);
        }
        let accent = ui.theme.accent;
        let good = ui.theme.good;
        let alarm = ui.theme.alarm;
        let dim = ui.theme.text_dim;
        let jobs = &view.snapshot.jobs;
        let outcome = ui.table(&mut self.jobs_table, table_area, &columns, &keys, |index| {
            let job = &jobs[index];
            Row {
                cells: job_cells(job, &kinds, accent, good, alarm, dim),
            }
        });
        let page = outcome.visible_rows.max(1);
        let _ = navigate(&mut self.jobs_table, &keys, page, ui.input.keys());
        self.apply_table_selection();
        if view.snapshot.jobs.is_empty() {
            ui.label_styled(
                Rect::new(
                    table_area.x,
                    table_area.y + ui.theme.row_height + s.px(8),
                    table_area.w,
                    s.px(24),
                ),
                t("jobs.empty"),
                *body,
                MUTED,
                Align::Left,
            );
        }
    }

    fn apply_table_selection(&mut self) {
        let key = self.jobs_table.selected().map(str::to_owned);
        let Some(key) = key else {
            return;
        };
        let Ok(raw) = key.parse::<u64>() else {
            return;
        };
        let id = TorrentId(raw);
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.file_pick = None;
        self.tracker_pick = 0;
        self.client.set_selected(Some(id));
    }

    fn paint_jobs_static(
        &self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        area: Rect,
        kinds: &[JobColumn],
        columns: &[Column],
        body: &TextStyle,
    ) {
        let content_width = (area.w - TABLE_GUTTER).max(0);
        let widths = liminalkit_ui::column_widths(columns, content_width, COLUMN_GAP);
        let header_h = ui.theme.row_height;
        let mut x = area.x;
        for (column, width) in columns.iter().zip(&widths) {
            ui.label(
                Rect::new(x, area.y, *width, header_h),
                &column.title,
                MUTED,
                column.align,
            );
            x += width + COLUMN_GAP;
        }
        ui.painter
            .horizontal_rule(area.x, area.y + header_h - 1, area.w, ui.theme.panel_edge);
        if view.snapshot.jobs.is_empty() {
            ui.label_styled(
                Rect::new(area.x, area.y + header_h + ui.px(8), area.w, header_h),
                t("jobs.empty"),
                *body,
                MUTED,
                Align::Left,
            );
            return;
        }
        let accent = ui.theme.accent;
        let good = ui.theme.good;
        let alarm = ui.theme.alarm;
        let dim = ui.theme.text_dim;
        for (index, job) in view.snapshot.jobs.iter().enumerate() {
            let top = area.y + header_h + index as i32 * header_h;
            let row = Rect::new(area.x, top, area.w, header_h);
            if row.bottom() > area.bottom() {
                break;
            }
            if self.selected == Some(job.id) {
                ui.painter.fill_rect(row, SELECTED);
            }
            let cells = job_cells(job, kinds, accent, good, alarm, dim);
            let mut x = area.x;
            for ((column, width), cell) in columns.iter().zip(&widths).zip(cells.iter()) {
                let bounds = Rect::new(x, top, *width, header_h);
                let colour = cell.colour.unwrap_or(INK);
                match cell.appearance {
                    CellAppearance::Text => ui.label(bounds, &cell.text, colour, column.align),
                    CellAppearance::Badge { fill, edge } => {
                        ui.badge(bounds, &cell.text, colour, fill, edge);
                    }
                    CellAppearance::Meter { fraction, fill } => {
                        ui.meter(bounds, fraction, &cell.text, fill);
                    }
                }
                x += width + COLUMN_GAP;
            }
        }
    }

    fn draw_inspector(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        inner: Rect,
        body: &TextStyle,
        small: &TextStyle,
        interactive: bool,
    ) {
        let caption_h = s.px(26);
        ui.label_styled(
            Rect::new(
                inner.x + s.px(12),
                inner.y + s.px(2),
                (inner.w - s.px(24)).max(1),
                caption_h,
            ),
            t("inspector.caption"),
            *body,
            INK,
            Align::Left,
        );
        if interactive {
            ui.underline_tabs(
                &mut self.tab,
                Rect::new(
                    inner.x + s.px(8),
                    inner.y + caption_h,
                    s.px(470).min(inner.w),
                    s.px(36),
                ),
                &[
                    t("inspector.tab_details"),
                    t("inspector.tab_files"),
                    t("inspector.tab_peers"),
                    t("inspector.tab_trackers"),
                ],
                *body,
                INK,
                MUTED,
                INK,
                2,
            );
        }
        let content_y = inner.y + caption_h + s.px(40);
        let content = Rect::new(
            inner.x + s.px(12),
            content_y,
            (inner.w - s.px(24)).max(1),
            (inner.bottom() - content_y - s.px(8)).max(1),
        );
        let job = self
            .selected
            .and_then(|id| view.snapshot.jobs.iter().find(|job| job.id == id));
        match self.tab {
            0 => self.draw_details(ui, view, s, content, body, job, interactive),
            1 => self.draw_files(ui, view, s, content, body, small, interactive),
            2 => self.draw_peers(ui, view, s, content, body, small, interactive),
            _ => self.draw_trackers(ui, view, s, content, body, small, interactive),
        }
    }

    fn draw_details(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        content: Rect,
        body: &TextStyle,
        job: Option<&TorrentRow>,
        interactive: bool,
    ) {
        let Some(job) = job else {
            ui.label_styled(
                content,
                t("inspector.select_download"),
                *body,
                MUTED,
                Align::Left,
            );
            return;
        };
        if let Some(detail) = view
            .url_detail
            .as_ref()
            .filter(|_| view.selected == Some(job.id))
        {
            self.draw_url_details(ui, s, content, body, job, detail, interactive);
            return;
        }
        let ratio = if job.total_payload_downloaded == 0 {
            unknown().to_string()
        } else {
            format!(
                "{:.2}",
                job.total_payload_uploaded as f64 / job.total_payload_downloaded as f64
            )
        };
        let current = if view.selected == Some(job.id) {
            current_file(&view.files)
        } else {
            unknown()
        };
        let reason = if job.state == JobState::Complete {
            view.seeding_reason.as_str()
        } else {
            ""
        };
        let destination = view
            .destination
            .as_deref()
            .filter(|_| view.selected == Some(job.id))
            .unwrap_or(unknown());
        let last_error = view
            .last_error
            .as_deref()
            .filter(|_| view.selected == Some(job.id))
            .unwrap_or(unknown());
        let lines = [
            clean_label(&job.name, 160),
            tf("details.state", &[("state", &state_text(job.state))]),
            tf(
                "details.bytes",
                &[
                    ("verified", &format_bytes(job.verified_bytes)),
                    ("wanted", &format_bytes(job.wanted_bytes)),
                    ("downloaded", &format_bytes(job.total_payload_downloaded)),
                    ("uploaded", &format_bytes(job.total_payload_uploaded)),
                ],
            ),
            tf(
                "details.rates",
                &[
                    ("down", &rate_text(job.down_bps)),
                    ("up", &rate_text(job.up_bps)),
                    ("eta", &eta_text(job)),
                    ("ratio", &ratio),
                ],
            ),
            tf(
                "details.connected",
                &[
                    ("seeds", &job.connected_seeds),
                    ("peers", &job.connected_peers),
                ],
            ),
            t("details.tracker_estimates").into(),
            tf("details.destination", &[("path", &destination)]),
            tf("details.current_file", &[("file", &current)]),
            tf("details.last_error", &[("error", &last_error)]),
            if reason.is_empty() {
                String::new()
            } else {
                tf("details.seeding", &[("reason", &reason)])
            },
        ];
        let rowh = s.px(24);
        for (index, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let row = Rect::new(content.x, content.y + index as i32 * rowh, content.w, rowh);
            if row.bottom() > content.bottom() {
                break;
            }
            ui.label_styled(
                row,
                line,
                *body,
                if index == 0 { INK } else { MUTED },
                Align::Left,
            );
        }
    }

    /// Details of a URL download: sizes, connections, identity, digest and
    /// the Retry and Cancel actions torrents do not have.
    #[allow(clippy::too_many_arguments)]
    fn draw_url_details(
        &mut self,
        ui: &mut Ui<'_>,
        s: Scale,
        content: Rect,
        body: &TextStyle,
        job: &TorrentRow,
        detail: &UrlDetail,
        interactive: bool,
    ) {
        let mut retry = false;
        let mut cancel = false;
        if interactive {
            retry = ui.button(
                Rect::new(content.x, content.y, s.px(90), s.px(26)),
                t("details.retry"),
            );
            cancel = ui.button(
                Rect::new(content.x + s.px(98), content.y, s.px(90), s.px(26)),
                t("common.cancel"),
            );
        }
        let lines = url_detail_lines(job, detail);
        let rowh = s.px(24);
        let top = content.y + s.px(32);
        for (index, line) in lines.iter().enumerate() {
            let row = Rect::new(content.x, top + index as i32 * rowh, content.w, rowh);
            if row.bottom() > content.bottom() {
                break;
            }
            ui.label_styled(
                row,
                line,
                *body,
                if index == 0 { INK } else { MUTED },
                Align::Left,
            );
        }
        if retry {
            self.submit(Command::Retry(job.id));
        } else if cancel {
            self.submit(Command::Cancel(job.id));
        }
    }

    fn draw_files(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        content: Rect,
        body: &TextStyle,
        small: &TextStyle,
        interactive: bool,
    ) {
        if let Some(message) = selection_message(self.selected, view.selected) {
            ui.label_styled(content, message, *body, MUTED, Align::Left);
            return;
        }
        if view.url_detail.is_some() {
            ui.label_styled(content, t("files.url_one_file"), *body, MUTED, Align::Left);
            return;
        }
        let header = s.px(32);
        if interactive {
            let earlier = ui.button(
                Rect::new(content.x, content.y, s.px(90), s.px(26)),
                t("files.earlier"),
            );
            let later = ui.button(
                Rect::new(content.x + s.px(98), content.y, s.px(90), s.px(26)),
                t("files.later"),
            );
            let automatic = ui.button(
                Rect::new(content.x + s.px(196), content.y, s.px(160), s.px(26)),
                t("files.automatic"),
            );
            if earlier {
                self.nudge(view, true);
            } else if later {
                self.nudge(view, false);
            } else if automatic {
                self.restore_order(view);
            }
        }
        let list = Rect::new(
            content.x,
            content.y + header,
            content.w,
            (content.h - header).max(1),
        );
        if view.files.is_empty() {
            ui.label_styled(list, t("files.empty"), *small, MUTED, Align::Left);
            return;
        }
        // Pad files only align real files to pieces; they are not listed.
        let files: Vec<&FileRow> = view.files.iter().filter(|file| !file.pad).collect();
        let heights = vec![s.px(28); files.len()];
        let mut toggle = None;
        let mut pick = None;
        let files = &files;
        let pick_now = self.file_pick;
        let narrow = list.w < s.px(520);
        ui.scroll_list(&mut self.file_scroll, list, &heights, |ui, index, rect| {
            let Some(file) = files.get(index) else {
                return;
            };
            if pick_now == Some(file.index) {
                ui.painter.fill_rect(rect, SELECTED);
            }
            if interactive {
                let mut wanted = file.wanted;
                if ui.checkbox(Rect::new(rect.x, rect.y, s.px(28), rect.h), &mut wanted, "") {
                    toggle = Some((file.index, wanted));
                }
                if ui.input.take_click(rect) {
                    pick = Some(file.index);
                }
            }
            let name_x = rect.x + s.px(32);
            ui.label_styled(
                Rect::new(name_x, rect.y, (rect.w / 2).max(1), rect.h),
                &clean_label(&file.path, 80),
                *body,
                INK,
                Align::Left,
            );
            // Files of one tier share a rank: the picker takes them rarest-first.
            let shared = file.wanted
                && file.verified_bytes < file.length
                && files
                    .iter()
                    .filter(|other| {
                        other.rank == file.rank
                            && other.wanted
                            && other.verified_bytes < other.length
                    })
                    .count()
                    > 1;
            let mut rank = rank_label(file.rank, file.wanted, file.verified_bytes, file.length);
            if shared {
                rank = tf("files.rank_shared", &[("rank", &rank)]);
            }
            ui.label_styled(
                Rect::new(rect.x + rect.w / 2, rect.y, rect.w / 4, rect.h),
                &rank,
                *small,
                MUTED,
                Align::Left,
            );
            if !narrow {
                let progress = if file.length == 0 {
                    unknown().into()
                } else {
                    tf(
                        "files.progress",
                        &[
                            ("verified", &format_bytes(file.verified_bytes)),
                            ("length", &format_bytes(file.length)),
                        ],
                    )
                };
                ui.label_styled(
                    Rect::new(rect.x + rect.w * 3 / 4, rect.y, rect.w / 4, rect.h),
                    &progress,
                    *small,
                    MUTED,
                    Align::Left,
                );
            }
        });
        if let Some(index) = pick {
            self.file_pick = Some(index);
        }
        if let Some((index, wanted)) = toggle
            && let Some(id) = self.selected
        {
            self.submit(Command::SetFileWanted {
                id,
                file_index: index,
                wanted,
            });
        }
    }

    fn nudge(&mut self, view: &SessionView, earlier: bool) {
        let Some(id) = self.selected else {
            self.status = t("status.select_torrent").into();
            return;
        };
        let Some(index) = self.file_pick else {
            self.status = t("status.select_file").into();
            return;
        };
        let ranks: Vec<u32> = view.files.iter().map(|file| file.rank).collect();
        let Some(order) = move_file(&ranks, index, earlier) else {
            self.status = t("status.file_at_end").into();
            return;
        };
        self.submit(Command::SetFileOrder {
            id,
            file_indices: order,
        });
    }

    fn restore_order(&mut self, _view: &SessionView) {
        let Some(id) = self.selected else {
            return;
        };
        self.submit(Command::SetFileOrderAutomatic(id));
    }

    fn draw_peers(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        content: Rect,
        body: &TextStyle,
        small: &TextStyle,
        interactive: bool,
    ) {
        let geo = tf("peers.country_database", &[("status", &view.geo_status)]);
        if let Some(message) = selection_message(self.selected, view.selected) {
            ui.label_styled(
                Rect::new(content.x, content.y, content.w, s.px(22)),
                &geo,
                *small,
                MUTED,
                Align::Left,
            );
            ui.label_styled(
                Rect::new(content.x, content.y + s.px(28), content.w, s.px(24)),
                message,
                *body,
                MUTED,
                Align::Left,
            );
            return;
        }
        if view.peers.is_empty() {
            ui.label_styled(
                Rect::new(content.x, content.y, content.w, s.px(22)),
                &geo,
                *small,
                MUTED,
                Align::Left,
            );
            ui.label_styled(
                Rect::new(content.x, content.y + s.px(28), content.w, s.px(24)),
                if view.url_detail.is_some() {
                    t("peers.url_none")
                } else {
                    t("peers.empty")
                },
                *body,
                MUTED,
                Align::Left,
            );
            return;
        }
        // One row of preference buttons with the country status beside
        // them; the explanation only when it leaves room for four peers, so
        // a short inspector still shows the list.
        let mut chosen = None;
        let mut x = content.x;
        for preference in [
            PeerPreference::Prefer,
            PeerPreference::Normal,
            PeerPreference::Avoid,
            PeerPreference::Block,
        ] {
            let rect = Rect::new(x, content.y, s.px(68), s.px(26));
            if interactive && ui.button(rect, preference_text(preference)) {
                chosen = Some(preference);
            }
            x += s.px(72);
        }
        ui.label_styled(
            Rect::new(
                x + s.px(8),
                content.y,
                (content.x + content.w - x - s.px(8)).max(1),
                s.px(26),
            ),
            &geo,
            *small,
            MUTED,
            Align::Left,
        );
        let note = preference_note();
        let note_height = s.px(18) * note.len() as i32;
        let with_note = content.h >= s.px(32) + note_height + s.px(44) * 4;
        if with_note {
            for (line, text) in note.iter().enumerate() {
                ui.label_styled(
                    Rect::new(
                        content.x,
                        content.y + s.px(30) + s.px(18) * line as i32,
                        content.w,
                        s.px(18),
                    ),
                    text,
                    *small,
                    MUTED,
                    Align::Left,
                );
            }
        }
        let top = if with_note {
            s.px(32) + note_height + s.px(4)
        } else {
            s.px(32)
        };
        let list = Rect::new(
            content.x,
            content.y + top,
            content.w,
            (content.h - top).max(1),
        );
        let heights = vec![s.px(44); view.peers.len()];
        let peers = &view.peers;
        let picked = self.peer_pick.min(peers.len() - 1);
        let mut pick = None;
        ui.scroll_list(&mut self.peer_scroll, list, &heights, |ui, index, rect| {
            let Some(peer) = peers.get(index) else {
                return;
            };
            if picked == index {
                ui.painter.fill_rect(rect, SELECTED);
            }
            if interactive && ui.input.take_click(rect) {
                pick = Some(index);
            }
            let remote = if peer.remote_choked {
                t("peers.remote_choked")
            } else {
                t("peers.remote_unchoked")
            };
            let local = if peer.we_choke {
                t("peers.we_choke")
            } else {
                t("peers.we_unchoke")
            };
            let interest = if peer.remote_interested {
                t("peers.remote_interested")
            } else {
                t("peers.remote_not_interested")
            };
            draw_peer_heading(ui, peer, rect, s, body);
            ui.label_styled(
                Rect::new(rect.x, rect.y + s.px(20), rect.w, s.px(20)),
                &tf(
                    "peers.row_detail",
                    &[
                        ("remote", &remote),
                        ("local", &local),
                        ("interest", &interest),
                        ("down", &rate_text(peer.down_bps)),
                        ("up", &rate_text(peer.up_bps)),
                        ("rtt", &rtt_text(peer.rtt_us)),
                        ("bad", &peer.hash_failures),
                    ],
                ),
                *small,
                MUTED,
                Align::Left,
            );
        });
        if let Some(index) = pick {
            self.peer_pick = index;
        }
        if let Some(preference) = chosen {
            self.set_peer_preference(view, picked, preference);
        }
    }

    fn set_peer_preference(
        &mut self,
        view: &SessionView,
        index: usize,
        preference: PeerPreference,
    ) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(peer) = view.peers.get(index) else {
            self.status = t("status.select_peer").into();
            return;
        };
        self.submit(Command::SetPeerPreference {
            id,
            address: peer.socket,
            preference,
        });
    }

    fn draw_trackers(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        content: Rect,
        body: &TextStyle,
        small: &TextStyle,
        interactive: bool,
    ) {
        if let Some(message) = selection_message(self.selected, view.selected) {
            ui.label_styled(content, message, *body, MUTED, Align::Left);
            return;
        }
        if view.url_detail.is_some() {
            ui.label_styled(content, t("trackers.url_none"), *body, MUTED, Align::Left);
            return;
        }
        let mut reannounce = false;
        if interactive
            && ui.button(
                Rect::new(content.x, content.y, s.px(140), s.px(26)),
                t("trackers.reannounce"),
            )
        {
            reannounce = true;
        }
        ui.label_styled(
            Rect::new(
                content.x + s.px(150),
                content.y,
                (content.w - s.px(150)).max(1),
                s.px(26),
            ),
            t("trackers.manual_note"),
            *small,
            MUTED,
            Align::Left,
        );
        if view.trackers.is_empty() {
            ui.label_styled(
                Rect::new(content.x, content.y + s.px(36), content.w, s.px(24)),
                t("trackers.empty"),
                *body,
                MUTED,
                Align::Left,
            );
            return;
        }
        let list = Rect::new(
            content.x,
            content.y + s.px(36),
            content.w,
            (content.h - s.px(36)).max(1),
        );
        let heights = vec![s.px(40); view.trackers.len()];
        let trackers = &view.trackers;
        let chosen = self.tracker_pick;
        let mut pick = None;
        ui.scroll_list(
            &mut self.tracker_scroll,
            list,
            &heights,
            |ui, index, rect| {
                let Some(tracker) = trackers.get(index) else {
                    return;
                };
                if chosen == index {
                    ui.painter.fill_rect(rect, SELECTED);
                }
                if interactive && ui.input.take_click(rect) {
                    pick = Some(index);
                }
                paint_tracker(ui, rect, tracker, view.elapsed_s, body, small);
            },
        );
        if let Some(index) = pick {
            self.tracker_pick = index;
        }
        if reannounce {
            self.reannounce(view);
        }
    }

    fn reannounce(&mut self, view: &SessionView) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(tracker) = view.trackers.get(self.tracker_pick) else {
            self.status = t("status.select_tracker").into();
            return;
        };
        if view.elapsed_s < tracker.manual_due_s {
            self.status = tf(
                "status.reannounce_cooldown",
                &[("seconds", &(tracker.manual_due_s - view.elapsed_s))],
            );
            return;
        }
        self.submit(Command::Reannounce {
            id,
            tracker_index: self.tracker_pick,
        });
    }

    fn draw_dialog(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        width: i32,
        height: i32,
    ) {
        let (escape, enter, tab) = dialog_keys(ui);
        if escape {
            self.close_layer();
            return;
        }
        if tab {
            match &mut self.dialog {
                Dialog::Add(add) if !add.browsing => add.focus = add.focus.next(),
                Dialog::Limits(limits) => limits.focus = (limits.focus + 1) % LIMIT_FIELDS,
                _ => {}
            }
        }
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        let small = TextStyle::new(FaceId(0), s.font(11.0));
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Add(_) => self.draw_add(ui, s, width, height, &body, &small, enter),
            Dialog::Limits(_) => self.draw_limits(ui, s, width, height, &body, &small, enter),
            Dialog::Remove(_) => self.draw_remove(ui, s, width, height, &body, &small),
            Dialog::Settings(_) => self.draw_settings(ui, view, s, width, height, &body, &small),
        }
    }

    fn draw_add(
        &mut self,
        ui: &mut Ui<'_>,
        s: Scale,
        width: i32,
        height: i32,
        body: &TextStyle,
        small: &TextStyle,
        enter: bool,
    ) {
        let inner = centered_panel(ui, s, width, height, 640, 520);
        let Dialog::Add(add) = &mut self.dialog else {
            return;
        };
        if add.browsing {
            ui.label_styled(
                Rect::new(inner.x, inner.y, inner.w, s.px(24)),
                t("add.browse_title"),
                *body,
                INK,
                Align::Left,
            );
            ui.label_styled(
                Rect::new(inner.x, inner.y + s.px(28), inner.w, s.px(22)),
                &clean_label(&add.browse_path.display().to_string(), 120),
                *small,
                MUTED,
                Align::Left,
            );
            let parent = ui.button(
                Rect::new(inner.x, inner.y + s.px(54), s.px(90), s.px(26)),
                t("add.browse_parent"),
            );
            let use_this = ui.button(
                Rect::new(inner.x + s.px(100), inner.y + s.px(54), s.px(150), s.px(26)),
                t("add.browse_use"),
            );
            let cancel = ui.button(
                Rect::new(inner.x + s.px(260), inner.y + s.px(54), s.px(90), s.px(26)),
                t("add.browse_back"),
            );
            if !add.browse_error.is_empty() {
                ui.label_styled(
                    Rect::new(inner.x, inner.y + s.px(84), inner.w, s.px(20)),
                    &add.browse_error,
                    *small,
                    MUTED,
                    Align::Left,
                );
            }
            let list = Rect::new(
                inner.x,
                inner.y + s.px(108),
                inner.w,
                (inner.h - s.px(116)).max(1),
            );
            let names = add.entries.clone();
            let heights = vec![s.px(26); names.len()];
            let mut chosen = None;
            ui.scroll_list(&mut add.browse_scroll, list, &heights, |ui, index, rect| {
                let Some(name) = names.get(index) else {
                    return;
                };
                if ui.button(rect, &clean_label(name, 80)) {
                    chosen = Some(name.clone());
                }
            });
            let path = add.browse_path.clone();
            if parent {
                let mut parent_path = path.clone();
                parent_path.pop();
                if parent_path.as_os_str().is_empty() {
                    parent_path.push("/");
                }
                self.browse_to(parent_path);
            } else if let Some(name) = chosen {
                self.browse_to(path.join(name));
            } else if use_this || enter {
                self.use_folder();
            } else if cancel && let Dialog::Add(add) = &mut self.dialog {
                add.browsing = false;
            }
            return;
        }
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, s.px(24)),
            t("add.title"),
            *body,
            INK,
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(32), inner.w, s.px(18)),
            t("add.source_label"),
            *small,
            MUTED,
            Align::Left,
        );
        let magnet_rect = Rect::new(inner.x, inner.y + s.px(52), inner.w, s.px(28));
        let torrent_rect = Rect::new(inner.x, inner.y + s.px(106), inner.w - s.px(110), s.px(28));
        let destination_rect =
            Rect::new(inner.x, inner.y + s.px(162), inner.w - s.px(110), s.px(28));
        let digest_rect = Rect::new(inner.x, inner.y + s.px(342), inner.w, s.px(28));
        if ui.input.take_click(magnet_rect) {
            add.focus = AddFocus::Magnet;
        } else if ui.input.take_click(torrent_rect) {
            add.focus = AddFocus::Torrent;
        } else if ui.input.take_click(destination_rect) {
            add.focus = AddFocus::Destination;
        } else if ui.input.take_click(digest_rect) {
            add.focus = AddFocus::Digest;
        }
        let magnet_focus = add.focus == AddFocus::Magnet;
        let torrent_focus = add.focus == AddFocus::Torrent;
        let dest_focus = add.focus == AddFocus::Destination;
        let digest_focus = add.focus == AddFocus::Digest;
        ui.text_field(
            &mut add.magnet,
            magnet_rect,
            t("add.source_hint"),
            magnet_focus,
            false,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(86), inner.w, s.px(18)),
            t("add.torrent_label"),
            *small,
            MUTED,
            Align::Left,
        );
        ui.text_field(
            &mut add.torrent,
            torrent_rect,
            t("add.torrent_hint"),
            torrent_focus,
            false,
        );
        let load = ui.button(
            Rect::new(
                inner.x + inner.w - s.px(100),
                inner.y + s.px(106),
                s.px(100),
                s.px(28),
            ),
            t("add.load"),
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(142), inner.w, s.px(18)),
            t("add.destination_label"),
            *small,
            MUTED,
            Align::Left,
        );
        ui.text_field(
            &mut add.destination,
            destination_rect,
            t("add.destination_hint"),
            dest_focus,
            false,
        );
        let browse = ui.button(
            Rect::new(
                inner.x + inner.w - s.px(100),
                inner.y + s.px(162),
                s.px(100),
                s.px(28),
            ),
            t("add.browse"),
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(196), inner.w, s.px(20)),
            &tf("add.available", &[("space", &add.space)]),
            *small,
            MUTED,
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(216), inner.w, s.px(36)),
            t("add.folder_note"),
            *small,
            MUTED,
            Align::Left,
        );
        let mut bootstrap = add.public_bootstrap;
        if ui.checkbox(
            Rect::new(inner.x, inner.y + s.px(258), inner.w, s.px(28)),
            &mut bootstrap,
            t("add.public_discovery"),
        ) {
            add.public_bootstrap = bootstrap;
        }
        ui.label_styled(
            Rect::new(
                inner.x + s.px(28),
                inner.y + s.px(286),
                inner.w - s.px(28),
                s.px(32),
            ),
            t("add.public_note"),
            *small,
            MUTED,
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(322), inner.w, s.px(18)),
            t("add.sha256_label"),
            *small,
            MUTED,
            Align::Left,
        );
        ui.text_field(
            &mut add.digest,
            digest_rect,
            t("add.sha256_hint"),
            digest_focus,
            false,
        );
        let notice = if !add.error.is_empty() {
            add.error.clone()
        } else {
            add.magnet
                .clipboard_error()
                .or_else(|| add.torrent.clipboard_error())
                .or_else(|| add.destination.clipboard_error())
                .or_else(|| add.digest.clipboard_error())
                .unwrap_or("")
                .to_string()
        };
        if !notice.is_empty() {
            ui.label_styled(
                Rect::new(inner.x, inner.y + s.px(378), inner.w, s.px(40)),
                &notice,
                *small,
                INK,
                Align::Left,
            );
        }
        let add_clicked = ui.button(
            Rect::new(inner.x, inner.y + inner.h - s.px(36), s.px(120), s.px(28)),
            t("add.confirm"),
        );
        let cancel = ui.button(
            Rect::new(
                inner.x + s.px(132),
                inner.y + inner.h - s.px(36),
                s.px(120),
                s.px(28),
            ),
            t("common.cancel"),
        );
        if load {
            self.load_torrent_field();
        } else if browse {
            self.begin_browse();
        } else if cancel {
            self.close_dialog();
        } else if add_clicked || enter {
            self.confirm_add();
        }
    }

    fn draw_limits(
        &mut self,
        ui: &mut Ui<'_>,
        s: Scale,
        width: i32,
        height: i32,
        body: &TextStyle,
        small: &TextStyle,
        enter: bool,
    ) {
        let inner = centered_panel(ui, s, width, height, 520, 560);
        let Dialog::Limits(dialog) = &mut self.dialog else {
            return;
        };
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, s.px(24)),
            t("limits.title"),
            *body,
            INK,
            Align::Left,
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(26), inner.w, s.px(32)),
            t("limits.note"),
            *small,
            MUTED,
            Align::Left,
        );
        for (index, label) in limit_labels().iter().enumerate() {
            let y = inner.y + s.px(64) + index as i32 * s.px(48);
            ui.label_styled(
                Rect::new(inner.x, y, inner.w, s.px(16)),
                label,
                *small,
                MUTED,
                Align::Left,
            );
            let rect = Rect::new(inner.x, y + s.px(16), inner.w, s.px(26));
            if ui.input.take_click(rect) {
                dialog.focus = index;
            }
            ui.text_field(
                &mut dialog.fields[index],
                rect,
                "",
                dialog.focus == index,
                false,
            );
        }
        if !dialog.error.is_empty() {
            ui.label_styled(
                Rect::new(inner.x, inner.y + inner.h - s.px(72), inner.w, s.px(24)),
                &dialog.error,
                *small,
                INK,
                Align::Left,
            );
        }
        let apply = ui.button(
            Rect::new(inner.x, inner.y + inner.h - s.px(36), s.px(120), s.px(28)),
            t("common.apply"),
        );
        let cancel = ui.button(
            Rect::new(
                inner.x + s.px(132),
                inner.y + inner.h - s.px(36),
                s.px(120),
                s.px(28),
            ),
            t("common.cancel"),
        );
        if cancel {
            self.close_dialog();
        } else if apply || enter {
            self.confirm_limits();
        }
    }

    fn draw_remove(
        &mut self,
        ui: &mut Ui<'_>,
        s: Scale,
        width: i32,
        height: i32,
        body: &TextStyle,
        small: &TextStyle,
    ) {
        let inner = centered_panel(ui, s, width, height, 560, 320);
        let Dialog::Remove(dialog) = &mut self.dialog else {
            return;
        };
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, s.px(28)),
            &tf("remove.title", &[("name", &dialog.name)]),
            *body,
            INK,
            Align::Left,
        );
        let mut delete_data = dialog.delete_data;
        if ui.checkbox(
            Rect::new(inner.x, inner.y + s.px(40), inner.w, s.px(28)),
            &mut delete_data,
            t("remove.delete_data"),
        ) {
            dialog.delete_data = delete_data;
        }
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(76), inner.w, s.px(22)),
            &tf("remove.path", &[("path", &clean_label(&dialog.path, 140))]),
            *small,
            MUTED,
            Align::Left,
        );
        let effect = if dialog.delete_data {
            tf(
                "remove.effect_delete",
                &[("size", &format_bytes(dialog.bytes))],
            )
        } else {
            t("remove.effect_keep").into()
        };
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(104), inner.w, s.px(44)),
            &effect,
            *small,
            MUTED,
            Align::Left,
        );
        if !dialog.error.is_empty() {
            ui.label_styled(
                Rect::new(inner.x, inner.y + s.px(150), inner.w, s.px(36)),
                &dialog.error,
                *small,
                INK,
                Align::Left,
            );
        }
        let remove = ui.button(
            Rect::new(inner.x, inner.y + inner.h - s.px(36), s.px(120), s.px(28)),
            t("remove.confirm"),
        );
        let cancel = ui.button(
            Rect::new(
                inner.x + s.px(132),
                inner.y + inner.h - s.px(36),
                s.px(120),
                s.px(28),
            ),
            t("common.cancel"),
        );
        let delete_data = dialog.delete_data;
        let id = dialog.id;
        if cancel {
            self.close_dialog();
        } else if remove {
            self.submit(Command::Remove { id, delete_data });
        }
    }

    fn draw_settings(
        &mut self,
        ui: &mut Ui<'_>,
        view: &SessionView,
        s: Scale,
        width: i32,
        height: i32,
        body: &TextStyle,
        small: &TextStyle,
    ) {
        let inner = centered_panel(ui, s, width, height, 640, 520);
        let Dialog::Settings(dialog) = &mut self.dialog else {
            return;
        };
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, s.px(24)),
            t("settings.title"),
            *body,
            INK,
            Align::Left,
        );
        let mut allow = dialog.allow_local;
        if ui.checkbox(
            Rect::new(inner.x, inner.y + s.px(36), inner.w, s.px(28)),
            &mut allow,
            t("settings.allow_local"),
        ) {
            dialog.allow_local = allow;
        }
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(68), inner.w, s.px(36)),
            t("settings.allow_local_note"),
            *small,
            MUTED,
            Align::Left,
        );
        let mut allow_urls = dialog.allow_local_urls;
        if ui.checkbox(
            Rect::new(inner.x, inner.y + s.px(104), inner.w, s.px(28)),
            &mut allow_urls,
            t("settings.allow_local_urls"),
        ) {
            dialog.allow_local_urls = allow_urls;
        }
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(136), inner.w, s.px(44)),
            &tf(
                "settings.public_trackers",
                &[
                    ("count", &view.public_tracker_count),
                    ("path", &clean_label(&view.public_tracker_path, 120)),
                ],
            ),
            *small,
            MUTED,
            Align::Left,
        );
        let reload = ui.button(
            Rect::new(inner.x, inner.y + s.px(188), s.px(180), s.px(28)),
            t("settings.reload_trackers"),
        );
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(232), inner.w, s.px(20)),
            t("settings.listen_port_label"),
            *small,
            MUTED,
            Align::Left,
        );
        ui.text_field(
            &mut dialog.port,
            Rect::new(inner.x, inner.y + s.px(256), s.px(120), s.px(26)),
            "",
            true,
            false,
        );
        let mut port_mapping = dialog.port_mapping;
        if ui.checkbox(
            Rect::new(inner.x, inner.y + s.px(290), inner.w, s.px(28)),
            &mut port_mapping,
            t("settings.port_mapping"),
        ) {
            dialog.port_mapping = port_mapping;
        }
        ui.label_styled(
            Rect::new(inner.x, inner.y + s.px(322), inner.w, s.px(56)),
            &settings_reachability(view),
            *small,
            MUTED,
            Align::Left,
        );
        let cycle = ui.button(
            Rect::new(inner.x, inner.y + s.px(386), s.px(300), s.px(28)),
            match dialog.encryption {
                PeerEncryption::PreferEncrypted => t("settings.encryption_prefer"),
                PeerEncryption::RequireEncrypted => t("settings.encryption_require"),
                PeerEncryption::PlaintextOnly => t("settings.encryption_plaintext"),
            },
        );
        if cycle {
            dialog.encryption = match dialog.encryption {
                PeerEncryption::PreferEncrypted => PeerEncryption::RequireEncrypted,
                PeerEncryption::RequireEncrypted => PeerEncryption::PlaintextOnly,
                PeerEncryption::PlaintextOnly => PeerEncryption::PreferEncrypted,
            };
        }
        if !dialog.error.is_empty() {
            ui.label_styled(
                Rect::new(inner.x, inner.y + s.px(422), inner.w, s.px(20)),
                &dialog.error,
                *small,
                INK,
                Align::Left,
            );
        }
        let apply = ui.button(
            Rect::new(inner.x, inner.y + inner.h - s.px(36), s.px(120), s.px(28)),
            t("common.apply"),
        );
        let close = ui.button(
            Rect::new(
                inner.x + s.px(132),
                inner.y + inner.h - s.px(36),
                s.px(120),
                s.px(28),
            ),
            t("common.close"),
        );
        let allow = dialog.allow_local;
        let allow_urls = dialog.allow_local_urls;
        let encryption = dialog.encryption;
        let port_mapping = dialog.port_mapping;
        let port_text = dialog.port.text().trim().to_string();
        let bound_port = dialog.bound_port;
        if reload {
            if self.client.try_request(Request::ReloadTrackers).is_ok() {
                self.status = t("status.trackers_reloaded").into();
            } else {
                self.status = t("status.queue_full").into();
            }
        } else if apply {
            let port = match port_text.parse::<u16>() {
                Ok(port) if port >= 1024 => port,
                _ => {
                    if let Dialog::Settings(dialog) = &mut self.dialog {
                        dialog.error = t("settings.port_invalid").into();
                    }
                    return;
                }
            };
            if (encryption != view.encryption
                && self
                    .client
                    .try_request(Request::SetEncryption(encryption))
                    .is_err())
                || (port != bound_port
                    && self
                        .client
                        .try_request(Request::SetListenPort(port))
                        .is_err())
                || (port_mapping != view.port_mapping
                    && self
                        .client
                        .try_request(Request::SetPortMapping(port_mapping))
                        .is_err())
                || (allow_urls != view.allow_local_urls
                    && self
                        .client
                        .try_request(Request::AllowLocalUrls(allow_urls))
                        .is_err())
            {
                self.status = t("status.queue_full").into();
                return;
            }
            if self.client.try_request(Request::AllowLocal(allow)).is_ok() {
                self.status = if allow {
                    t("status.local_trackers_on").into()
                } else {
                    t("status.local_trackers_off").into()
                };
            } else {
                self.status = t("status.queue_full").into();
            }
            self.close_dialog();
        } else if close {
            self.close_dialog();
        }
    }
}

fn selection_message(
    selected: Option<TorrentId>,
    published: Option<TorrentId>,
) -> Option<&'static str> {
    if selected.is_none() {
        Some(t("inspector.select_download"))
    } else if published != selected {
        Some(t("inspector.loading"))
    } else {
        None
    }
}

/// A peer row's first line: the country flag, then the country code,
/// address, resolved client and preference. The flag slot is reserved even
/// when empty so addresses line up; the code stays in the text, so an
/// unknown or flagless country still reads `[--]` or `[XK]`.
fn draw_peer_heading(ui: &mut Ui<'_>, peer: &PeerView, rect: Rect, s: Scale, body: &TextStyle) {
    let flag = peer_flag_rect(rect, s);
    if flags::draw_flag(&mut ui.painter, peer.country, flag) {
        ui.painter.stroke_rect(flag.inset(-1), 1, EDGE);
    }
    let text_x = rect.x + s.px(22);
    ui.label_styled(
        Rect::new(text_x, rect.y, (rect.right() - text_x).max(1), s.px(20)),
        &tf(
            "peers.row_heading",
            &[
                ("country", &country_text(peer.country)),
                ("address", &peer.address),
                ("client", &clean_label(&peer.client, 40)),
                ("preference", &preference_text(peer.preference)),
            ],
        ),
        *body,
        INK,
        Align::Left,
    );
}

/// 16x12 logical (the 4:3 flag-icons shape), centred in the 20-pixel line.
fn peer_flag_rect(row: Rect, s: Scale) -> Rect {
    Rect::new(
        row.x + s.px(1),
        row.y + (s.px(20) - s.px(12)) / 2,
        s.px(16),
        s.px(12),
    )
}

#[cfg(test)]
mod peer_flag_tests {
    //! Draws a real peer heading through PixelKit's `Ui` into an offscreen
    //! buffer and checks where the flag pixels land.
    use super::{EDGE, PANEL, draw_peer_heading, peer_flag_rect};
    use crate::worker::PeerView;
    use liminalkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
    use liminalkit_shell::{Input, Scale};
    use liminalkit_text::TextCache;
    use liminalkit_text::TextStyle;
    use liminalkit_text::font::{Face, FaceId, FontSet};
    use liminalkit_ui::Ui;
    use templartorrent_core::model::PeerPreference;

    fn fonts() -> &'static FontSet {
        static FONTS: std::sync::OnceLock<&'static FontSet> = std::sync::OnceLock::new();
        FONTS.get_or_init(|| {
            let path = crate::workflow::resolve_font(None).expect("a TrueType font");
            let bytes: &'static [u8] = Box::leak(std::fs::read(path).expect("read font").into());
            let face: &'static Face = Box::leak(Box::new(Face::from_bytes(bytes).expect("font")));
            let mut fonts = FontSet::new();
            fonts.add(face);
            Box::leak(Box::new(fonts))
        })
    }

    fn peer(country: Option<[u8; 2]>) -> PeerView {
        PeerView {
            address: "192.0.2.7:51413".into(),
            socket: "192.0.2.7:51413".parse().unwrap(),
            preference: PeerPreference::Normal,
            client: "qBittorrent 4.6.2".into(),
            down_bps: 0,
            up_bps: 0,
            rtt_us: None,
            hash_failures: 0,
            country,
            remote_choked: true,
            remote_interested: false,
            we_choke: true,
        }
    }

    /// Render one heading per peer, 44 px apart, at `s`; return the buffer.
    fn render(peers: &[PeerView], s: Scale) -> WindowBuffer {
        let mut buffer = WindowBuffer::new(s.px(480) as u32, s.px(44) as u32 * peers.len() as u32);
        let mut text = TextCache::new(fonts());
        let mut input = Input::new();
        let mut kernel = RasterKernel::new();
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            super::app_theme(s),
            s,
            &mut kernel,
        );
        ui.painter.clear(PANEL);
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        for (index, peer) in peers.iter().enumerate() {
            let row = Rect::new(0, s.px(44) * index as i32, s.px(480), s.px(44));
            draw_peer_heading(&mut ui, peer, row, s, &body);
        }
        drop(ui);
        buffer
    }

    fn rgb(buffer: &WindowBuffer, x: i32, y: i32) -> u32 {
        buffer.pixels[y as usize * buffer.width as usize + x as usize] & 0x00ff_ffff
    }

    #[test]
    fn flag_is_drawn_left_of_the_peer_text_only_for_a_known_country() {
        for s in [Scale::ONE, Scale::new(2.0)] {
            let rows = [peer(Some(*b"DE")), peer(None), peer(Some(*b"ZZ"))];
            let buffer = render(&rows, s);
            let flag = peer_flag_rect(Rect::new(0, 0, s.px(480), s.px(44)), s);
            // Germany: black, red, gold bands, framed by the edge colour.
            let cx = flag.x + flag.w / 2;
            assert_eq!(rgb(&buffer, cx, flag.y + flag.h / 6), 0x000001);
            assert_eq!(rgb(&buffer, cx, flag.y + flag.h / 2), 0xff0000);
            assert_eq!(rgb(&buffer, cx, flag.y + flag.h * 5 / 6), 0xffcc00);
            assert_eq!(rgb(&buffer, flag.x - 1, flag.y + 2), EDGE);
            // The text starts after the flag slot, and the row has text.
            let text_x = s.px(22);
            let inked = |top: i32| {
                (top..top + s.px(20))
                    .flat_map(|y| (text_x..s.px(480)).map(move |x| (x, y)))
                    .filter(|&(x, y)| rgb(&buffer, x, y) != PANEL)
                    .count()
            };
            assert!(inked(0) > 0, "known-country row has no text");
            // Unknown and flagless rows: the slot stays blank, text remains.
            for row in 1..3 {
                let top = s.px(44) * row;
                for y in top..top + s.px(20) {
                    for x in 0..text_x - s.px(2) {
                        assert_eq!(rgb(&buffer, x, y), PANEL, "row {row} slot pixel ({x},{y})");
                    }
                }
                assert!(inked(top) > 0, "row {row} lost its text");
            }
        }
    }
}

/// The Details lines for a URL job. Unknown values are an em dash: an
/// unknown size is never shown as zero.
pub(crate) fn url_detail_lines(job: &TorrentRow, detail: &UrlDetail) -> Vec<String> {
    let size = detail
        .size
        .map(format_bytes)
        .unwrap_or_else(|| unknown().into());
    let percent = match detail.size {
        Some(size) if size > 0 => percent_text(detail.done.saturating_mul(100) / size),
        _ => unknown().into(),
    };
    let parts = detail.parts.len();
    let connections = if parts > 1 {
        tf(
            "url.connections_parts",
            &[("active", &detail.connections), ("parts", &parts)],
        )
    } else if detail.ranged {
        tf(
            "url.connections_one_part",
            &[("count", &detail.connections)],
        )
    } else {
        tf(
            "url.connections_no_ranges",
            &[("count", &detail.connections)],
        )
    };
    let resume = match &detail.validator {
        Some(validator) => tf("url.resume_identity", &[("validator", validator)]),
        None if detail.ranged => t("url.resume_none").into(),
        None => t("url.resume_impossible").into(),
    };
    let file = match &detail.file {
        Some(name) if matches!(job.state, JobState::Complete) => {
            tf("url.file_complete", &[("name", name)])
        }
        Some(name) => tf("url.file_partial", &[("name", name)]),
        None => t("url.file_unnamed").into(),
    };
    let sha256 = match detail.sha256.as_deref() {
        Some(digest) => tf("url.sha256", &[("digest", &digest)]),
        None => t("url.sha256_none").into(),
    };
    vec![
        clean_label(&job.name, 160),
        tf(
            "url.state",
            &[
                ("state", &state_text(job.state)),
                ("size", &size),
                ("done", &format_bytes(detail.done)),
                ("percent", &percent),
            ],
        ),
        tf(
            "url.rates",
            &[("down", &rate_text(job.down_bps)), ("eta", &eta_text(job))],
        ),
        connections,
        tf("url.address", &[("url", &clean_label(&detail.url, 160))]),
        tf(
            "url.destination",
            &[("path", &clean_label(&detail.destination, 160))],
        ),
        file,
        resume,
        sha256,
        tf(
            "url.last_error",
            &[("error", &detail.error.as_deref().unwrap_or(unknown()))],
        ),
        detail
            .note
            .as_deref()
            .map(|note| tf("url.note", &[("note", &note)]))
            .unwrap_or_default(),
    ]
    .into_iter()
    .filter(|line| !line.is_empty())
    .collect()
}

fn current_file(files: &[FileRow]) -> &str {
    files
        .iter()
        .filter(|file| file.wanted && file.verified_bytes < file.length)
        .min_by_key(|file| file.rank)
        .map(|file| file.path.as_str())
        .unwrap_or(unknown())
}

/// Listener, DHT and router-mapping state under the mapping checkbox. The
/// mapping status is the driver's own; it never carries the external address.
fn settings_reachability(view: &SessionView) -> String {
    let mapping = if view.port_mapping {
        tf(
            "settings.mapping_on",
            &[("status", &view.port_mapping_status)],
        )
    } else {
        t("settings.mapping_off").to_string()
    };
    let dht = view
        .dht_nodes
        .map_or(t("settings.dht_off").to_string(), |nodes| {
            tf("settings.dht_nodes", &[("count", &nodes)])
        });
    tf(
        "settings.reachability",
        &[
            ("listen", &listen_text(&view.listen)),
            ("dht", &dht),
            ("mapping", &mapping),
        ],
    )
}

fn listen_text(state: &ListenState) -> String {
    match state {
        ListenState::Starting => t("status.listen_starting").into(),
        ListenState::Bound(address) => tf("status.listen_bound", &[("address", address)]),
        ListenState::Failed(error) => tf("status.listen_failed", &[("detail", error)]),
    }
}

fn paint_tracker(
    ui: &mut Ui<'_>,
    rect: Rect,
    tracker: &TrackerView,
    elapsed_s: u64,
    body: &TextStyle,
    small: &TextStyle,
) {
    let seeds = tracker
        .seeds
        .map(|value| value.to_string())
        .unwrap_or_else(|| unknown().into());
    let peers = tracker
        .peers
        .map(|value| value.to_string())
        .unwrap_or_else(|| unknown().into());
    let error = tracker.last_error.as_deref().unwrap_or(unknown());
    ui.label_styled(
        Rect::new(rect.x + 8, rect.y, rect.w - 16, rect.h / 2),
        &tf(
            "trackers.row_heading",
            &[
                ("label", &clean_label(&tracker.label, 80)),
                ("origin", &tracker.origin),
            ],
        ),
        *body,
        INK,
        Align::Left,
    );
    ui.label_styled(
        Rect::new(rect.x + 8, rect.y + rect.h / 2, rect.w - 16, rect.h / 2),
        &tf(
            "trackers.row_detail",
            &[
                ("seeds", &seeds),
                ("peers", &peers),
                ("cooldown", &cooldown_text(elapsed_s, tracker.manual_due_s)),
                ("error", &error),
            ],
        ),
        *small,
        MUTED,
        Align::Left,
    );
}

fn dialog_keys(ui: &mut Ui<'_>) -> (bool, bool, bool) {
    let escape = ui
        .input
        .keys()
        .iter()
        .any(|key| matches!(key, KeyInput::Escape));
    let enter = ui
        .input
        .keys()
        .iter()
        .any(|key| matches!(key, KeyInput::Enter));
    let tab = ui
        .input
        .keys()
        .iter()
        .any(|key| matches!(key, KeyInput::Tab));
    if escape || tab {
        ui.input.take_keys();
    }
    (escape, enter, tab)
}

fn centered_panel(
    ui: &mut Ui<'_>,
    s: Scale,
    width: i32,
    height: i32,
    logical_w: i32,
    logical_h: i32,
) -> Rect {
    let w = s.px(logical_w).min(width - s.px(24)).max(s.px(280));
    let h = s.px(logical_h).min(height - s.px(24)).max(s.px(180));
    let rect = Rect::new((width - w) / 2, (height - h) / 2, w, h);
    ui.flat_panel(rect, PANEL, EDGE, false).inset(s.px(16))
}

pub struct App {
    text: TextCache,
    kernel: RasterKernel,
    input: Input,
    desktop: Desktop,
    /// Asks the host for another frame; see the end of `render`.
    waker: Option<Waker>,
    foreground: crate::foreground::Foreground,
    window_icon: crate::window_icon::WindowIcon,
}

impl App {
    pub fn new(
        fonts: &'static FontSet,
        client: WorkerHandle,
        activator: Activator,
        ui: UiState,
        store: Option<SessionStore>,
    ) -> Self {
        Self {
            text: TextCache::new(fonts),
            kernel: RasterKernel::new(),
            input: Input::new(),
            desktop: Desktop::new(client, activator, ui, store),
            waker: None,
            foreground: crate::foreground::Foreground::default(),
            window_icon: crate::window_icon::WindowIcon::default(),
        }
    }

    pub fn prime(&mut self, payloads: Vec<String>) {
        self.desktop.prime(payloads);
    }
}

impl PixelApp for App {
    fn attach(&mut self, waker: Waker) {
        self.waker = Some(waker.clone());
        let wake = waker.clone();
        self.desktop.client.set_waker(Arc::new(move || wake.wake()));
        let activate = waker.clone();
        self.desktop
            .activator
            .set_waker(Arc::new(move || activate.wake()));
        let picked = waker.clone();
        self.desktop.pickers.wake = Some(Arc::new(move || picked.wake()));
        let redraw = waker;
        self.input
            .attach_clipboard(liminalkit_shell::clipboard::ClipboardWorker::new(
                move || redraw.wake(),
            ));
    }

    fn on_native_window(&mut self, window: &winit::window::Window) {
        self.foreground.remember(window);
        self.desktop.pickers.owner = self.foreground.hwnd();
        self.window_icon.apply(self.foreground.hwnd());
    }

    fn on_scale(&mut self, _scale: Scale) {
        // A monitor with another scale wants other icon sizes.
        self.window_icon.apply(self.foreground.hwnd());
    }

    fn render(&mut self, buffer: &mut WindowBuffer, s: Scale) {
        if self.desktop.pump() {
            self.foreground.raise();
        }
        let view = self.desktop.client.view();
        self.desktop.sync_selection(&view);
        let title = app_title(&view.snapshot.jobs);
        if title != self.desktop.last_title {
            self.desktop.last_title = title.clone();
            self.desktop.pending_title = Some(title);
        }
        let width = buffer.width as i32;
        let height = buffer.height as i32;
        let before = self.desktop.frame_state();
        let dialog = !matches!(self.desktop.dialog, Dialog::None);
        let mut ui = Ui::new(
            Painter::new(buffer),
            &mut self.text,
            &mut self.input,
            app_theme(s),
            s,
            &mut self.kernel,
        );
        ui.painter.clear(ui.theme.background);
        self.desktop
            .draw_shell(&mut ui, &view, s, width, height, !dialog);
        if dialog {
            self.desktop.draw_dialog(&mut ui, &view, s, width, height);
        }
        ui.input.end_frame();
        // A click handled while this frame was drawn (a tab, a dialog, a
        // selection) can leave widgets drawn earlier in the frame showing
        // the old state. Without continuous redraws, ask for one more frame.
        if self.desktop.frame_state() != before
            && let Some(waker) = &self.waker
        {
            waker.wake();
        }
    }

    fn poll_title(&mut self) -> Option<String> {
        self.desktop.pending_title.take()
    }

    fn cursor_shape(&self) -> CursorShape {
        self.desktop.cursor
    }

    fn on_key(&mut self, key: &KeyInput) {
        self.input.key(key.clone());
    }

    fn on_modifiers(&mut self, modifiers: Modifiers) {
        self.input.modifiers_changed(modifiers);
    }

    fn on_cursor(&mut self, x: f32, y: f32) {
        self.input.cursor_moved(x, y);
    }

    fn on_cursor_left(&mut self) {
        self.input.cursor_left();
    }

    fn on_mouse(&mut self, button: MouseButton, pressed: bool) {
        self.input.mouse(button, pressed);
    }

    fn on_scroll(&mut self, dx: f32, dy: f32) {
        self.input.scrolled(dx, dy);
    }

    fn on_exit(&mut self) {
        self.desktop.save_ui();
        self.desktop.client.shutdown();
        self.desktop.activator.stop();
    }
}

#[cfg(test)]
mod tests {
    //! PixelKit's `close_slot` calls `PixelApp::on_exit`. These tests drive that
    //! method, and the Add dialog's Ctrl+V path, without opening a window.
    use super::{App, Dialog};
    use crate::activate::{Activation, activate, spawn};
    use crate::persist::{SessionStore, UiState, hex};
    use crate::worker::{WorkerConfig, WorkerHandle};
    use liminalkit_raster::WindowBuffer;
    use liminalkit_shell::{KeyInput, Modifiers, Scale};
    use liminalkit_text::font::{Face, FontSet};
    use liminalkit_windowing::{PixelApp, Waker};
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use templartorrent_core::model::{JobState, Limits};
    use templartorrent_rbit_adapter::driver::JobRecord;

    /// Synthetic magnet. Not a swarm the test contacts.
    const MAGNET: &str =
        "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=clipboard-probe";

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Owns the connection that published the probe text. On Wayland the
    /// owner has to stay alive until the paste is read. Drop writes the
    /// previous text back, or clears when there was none. A clipboard that
    /// held only an image cannot be restored by this text round-trip.
    struct ClipboardGuard {
        clipboard: liminalkit_shell::Clipboard,
        previous: Option<String>,
        /// Set when the clipboard had no text. Cleared once that state is put back.
        clear_on_drop: bool,
    }

    impl ClipboardGuard {
        fn replace_text(text: &str) -> Result<Self, String> {
            let mut clipboard = liminalkit_shell::Clipboard::new();
            let previous = clipboard
                .try_text()
                .map_err(|error| format!("clipboard read failed: {error}"))?;
            clipboard
                .try_set_text(text.to_string())
                .map_err(|error| format!("clipboard write failed: {error}"))?;
            let guard = Self {
                clipboard,
                clear_on_drop: previous.is_none(),
                previous,
            };
            match guard.clipboard.try_text() {
                Ok(Some(current)) if current == text => Ok(guard),
                Ok(_) => Err("clipboard did not keep the text just written".into()),
                Err(error) => Err(format!("clipboard read-back failed: {error}")),
            }
        }

        /// Put the previous text back and read it again. The error text does not
        /// include clipboard contents.
        fn restore(&mut self) -> Result<(), String> {
            if let Some(text) = self.previous.clone() {
                self.clipboard
                    .try_set_text(text.clone())
                    .map_err(|error| format!("clipboard restore failed: {error}"))?;
                match self.clipboard.try_text() {
                    Ok(Some(current)) if current == text => {
                        self.previous = None;
                        self.clear_on_drop = false;
                        Ok(())
                    }
                    Ok(_) => Err("clipboard restore did not read back".into()),
                    Err(error) => Err(format!("clipboard restore read failed: {error}")),
                }
            } else if self.clear_on_drop {
                self.clipboard
                    .try_clear()
                    .map_err(|error| format!("clipboard clear failed: {error}"))?;
                match self.clipboard.try_text() {
                    Ok(None) => {
                        self.clear_on_drop = false;
                        Ok(())
                    }
                    Ok(Some(_)) => Err("clipboard clear left text behind".into()),
                    Err(error) => Err(format!("clipboard clear read failed: {error}")),
                }
            } else {
                Ok(())
            }
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn temp_root(label: &str) -> RemoveOnDrop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "templartorrent-ui-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        RemoveOnDrop(root)
    }

    fn fonts() -> &'static FontSet {
        static FONTS: OnceLock<&'static FontSet> = OnceLock::new();
        FONTS.get_or_init(|| {
            let path = crate::workflow::resolve_font(None).expect("a TrueType font");
            let mut bytes = Vec::new();
            std::fs::File::open(&path)
                .expect("open font")
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .expect("read font");
            assert!(bytes.len() <= 4 * 1024 * 1024, "font exceeds 4 MiB");
            let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            let face = Face::from_bytes(bytes).expect("static TrueType font");
            let face: &'static Face = Box::leak(Box::new(face));
            let mut fonts = FontSet::new();
            fonts.add(face);
            Box::leak(Box::new(fonts))
        })
    }

    fn worker(session: PathBuf) -> WorkerHandle {
        WorkerHandle::start(WorkerConfig {
            limits: Limits::default(),
            allow_local_trackers: false,
            allow_local_urls: true,
            public_trackers: Vec::new(),
            public_tracker_path: session.join("public-trackers.txt"),
            geo_db_path: None,
            listen: "127.0.0.1:0".parse().expect("listen address"),
            session_dir: Some(session),
        })
        .expect("worker")
    }

    fn activator_in(dir: &Path) -> crate::activate::Activator {
        let activation = activate(dir, &[]).expect("activation listener");
        let Activation::Primary { listener, path, .. } = activation else {
            panic!("activation was forwarded");
        };
        spawn(listener, path)
    }

    fn seed_job(base: &Path) -> [u8; 20] {
        std::fs::create_dir_all(base.join("job/owned-series")).unwrap();
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let torrent = rbit::TorrentBuilder::new("owned-series")
            .version(rbit::TorrentVersion::V1)
            .piece_length(16384)
            .add_file("Episode02.bin", data[..20_000].to_vec())
            .add_file("Episode01.bin", data[20_000..].to_vec())
            .build()
            .unwrap();
        std::fs::write(base.join("job/owned-series/Episode02.bin"), &data[..20_000]).unwrap();
        std::fs::write(base.join("job/owned-series/Episode01.bin"), &data[20_000..]).unwrap();
        let meta = rbit::Metainfo::from_bytes(&torrent).unwrap();
        let rbit::InfoHash::V1(hash) = meta.info_hash else {
            panic!("fixture must be v1");
        };
        SessionStore::new(base.join("session"))
            .save_job(&JobRecord {
                info_hash: hash,
                torrent,
                destination: base.join("job"),
                paused: false,
                public_bootstrap: false,
                wanted: vec![true, true],
                order: vec![1, 0],
                tiers: Vec::new(),
                trackers: Vec::new(),
                preferences: Vec::new(),
            })
            .unwrap();
        hash
    }

    fn wait_until_seeding(handle: &WorkerHandle) {
        let start = Instant::now();
        loop {
            let view = handle.view();
            if view.snapshot.jobs.len() == 1
                && matches!(
                    view.snapshot.jobs[0].state,
                    JobState::Complete | JobState::Seeding
                )
            {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "job never restored: {:?}",
                view.note
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn window_close_saves_layout_and_resume() {
        let root = temp_root("close");
        let hash = seed_job(&root.0);
        let session = root.0.join("session");
        let client = worker(session.clone());
        wait_until_seeding(&client);
        let mut app = App::new(
            fonts(),
            client,
            activator_in(&root.0.join("runtime")),
            UiState::default(),
            Some(SessionStore::new(session.clone())),
        );
        let mut buffer = WindowBuffer::new(960, 640);
        app.render(&mut buffer, Scale::ONE);
        let width = app.desktop.logical_size.0.round() as u32;
        let height = app.desktop.logical_size.1.round() as u32;
        let split = app.desktop.split;
        assert_eq!(
            (width, height),
            (960, 640),
            "the rendered frame is what close persists"
        );
        assert_ne!(
            (width, height),
            (UiState::default().width, UiState::default().height)
        );
        let socket = root.0.join("runtime").join(crate::activate::ENDPOINT);
        assert!(socket.exists(), "the listener is up before close");
        app.on_exit();
        drop(app);
        let loaded = SessionStore::new(session)
            .load_ui()
            .expect("ui record")
            .expect("on_exit wrote ui state");
        assert_eq!(loaded.width, width);
        assert_eq!(loaded.height, height);
        let expected_split = format!("{split:.4}").parse::<f32>().expect("split");
        assert!(
            (loaded.split - expected_split).abs() < 1e-6,
            "saved {} expected {expected_split}",
            loaded.split
        );
        assert!(!socket.exists(), "on_exit stops the activation listener");
        let resume = std::fs::read_to_string(
            root.0
                .join("session")
                .join(format!("{}.resume", hex(&hash))),
        )
        .expect("resume bitmap");
        assert!(resume.starts_with("templartorrent-resume 1\n"), "{resume}");
        assert!(resume.contains("barrier 1\n"), "{resume}");
        assert!(
            resume.contains("bits 1f\n"),
            "five verified pieces, got {resume}"
        );
    }

    #[test]
    fn paste_magnet_from_the_system_clipboard() {
        // Declared first so a later panic still restores the clipboard.
        let mut clipboard = ClipboardGuard::replace_text(MAGNET).expect("system clipboard");
        let root = temp_root("paste");
        let session = root.0.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let mut app = App::new(
            fonts(),
            worker(session),
            activator_in(&root.0.join("runtime")),
            UiState::default(),
            None,
        );
        app.attach(Waker::detached());
        app.desktop.open_add();
        app.on_modifiers(Modifiers {
            control: true,
            shift: false,
            alt: false,
            logo: false,
        });
        app.on_key(&KeyInput::Character('v'));
        let mut buffer = WindowBuffer::new(1100, 800);
        let start = Instant::now();
        loop {
            app.render(&mut buffer, Scale::ONE);
            let Dialog::Add(add) = &app.desktop.dialog else {
                panic!("add dialog closed");
            };
            if let Some(error) = add.magnet.clipboard_error() {
                panic!("clipboard paste failed: {error}");
            }
            if add.magnet.text() == MAGNET {
                assert!(add.torrent.text().is_empty());
                assert!(add.destination.text().is_empty());
                assert!(add.torrent.clipboard_error().is_none());
                assert!(add.destination.clipboard_error().is_none());
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "magnet field did not receive the clipboard text"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        app.on_exit();
        clipboard.restore().expect("clipboard restored");
    }

    #[test]
    fn url_details_show_an_em_dash_for_an_unknown_size() {
        use templartorrent_rbit_adapter::http::UrlDetail;
        let job = templartorrent_core::model::TorrentRow {
            id: templartorrent_core::model::TorrentId(1 << 63),
            name: std::sync::Arc::from("stream"),
            state: JobState::Downloading,
            wanted_bytes: 0,
            verified_bytes: 4096,
            total_payload_downloaded: 4096,
            total_payload_uploaded: 0,
            down_bps: 0,
            up_bps: 0,
            connected_seeds: 0,
            connected_peers: 1,
            estimated_seeds: None,
            estimated_peers: None,
        };
        let detail = UrlDetail {
            url: "http://h.example/stream".into(),
            destination: "/tmp/d".into(),
            file: Some("stream".into()),
            size: None,
            done: 4096,
            ranged: false,
            validator: None,
            parts: Vec::new(),
            connections: 1,
            sha256: None,
            error: None,
            note: None,
            state: JobState::Downloading,
        };
        use crate::text::tf;
        use crate::workflow::{format_bytes, state_text, unknown};
        let lines = super::url_detail_lines(&job, &detail);
        let state = tf(
            "url.state",
            &[
                ("state", &state_text(job.state)),
                ("size", &unknown()),
                ("done", &format_bytes(4096)),
                ("percent", &unknown()),
            ],
        );
        assert_eq!(lines[1], state);
        let no_ranges = tf("url.connections_no_ranges", &[("count", &1)]);
        assert!(lines.contains(&no_ranges), "{lines:?}");
        let partial = tf("url.file_partial", &[("name", &"stream")]);
        assert!(lines.contains(&partial), "{lines:?}");
        let (progress, _) = super::progress_parts(&job);
        assert_eq!(progress, unknown());
    }

    #[test]
    fn settings_line_shows_the_live_port_mapping_state() {
        let mut view = crate::worker::SessionView::idle(Limits::default(), "t.txt".into());
        view.listen = crate::worker::ListenState::Bound("0.0.0.0:51234".parse().unwrap());
        use crate::text::{t, tf};
        let off = super::settings_reachability(&view);
        assert!(off.ends_with(t("settings.mapping_off")), "{off}");
        assert!(off.starts_with(&super::listen_text(&view.listen)), "{off}");
        view.port_mapping = true;
        view.port_mapping_status = "mapped by NAT-PMP as external port 51234 (TCP and UDP)".into();
        let on = super::settings_reachability(&view);
        let mapping = tf(
            "settings.mapping_on",
            &[("status", &view.port_mapping_status)],
        );
        assert!(on.ends_with(&mapping), "{on}");
    }
}
