//! Interactive layout reference with FIXTURE DATA. No live transfers or fake working controls.
//! Run with --font /path/to/a/static-TrueType.ttf. No font files are distributed here.
#![forbid(unsafe_code)]
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, MouseButton, Scale};
use pixelkit_text::font::{Face, FaceId, FontSet};
use pixelkit_text::{Align, TextCache, TextStyle};
use pixelkit_ui::{Cell, Column, Row, ScrollState, TableState, Theme, Ui};
use pixelkit_windowing::{CursorShape, PaneMetrics, Panes, PixelApp, Side, WindowConfig, run_app};
mod bridge;
mod view;

use std::{path::PathBuf, sync::Arc};
use templartorrent_core::{
    model::{Command, JobState, Limits, TorrentId, TorrentRow, window_title},
    sequence::{FileSpec, SKIP, Sequence},
};

use bridge::{AckOutcome, LiveBridge};
const BG: u32 = 0xf4f4f2;
const PANEL: u32 = 0xffffff;
const INK: u32 = 0x242729;
const MUTED: u32 = 0x676d72;
const EDGE: u32 = 0xd3d6d8;
const SELECTED: u32 = 0xe6edf2;
const HOVER: u32 = 0xeef1f3;

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

fn download_columns(px: impl Fn(i32) -> i32, width: i32) -> Vec<Column> {
    let mut columns = vec![
        Column::flex("Name", 4, Align::Left),
        Column::fixed("Progress", px(72), Align::Left),
    ];
    if width >= px(640) {
        columns.push(Column::fixed("Down", px(72), Align::Right));
        columns.push(Column::fixed("Up", px(72), Align::Right));
    }
    if width >= px(760) {
        columns.push(Column::fixed("Peers", px(64), Align::Left));
    }
    if width >= px(420) {
        columns.push(Column::fixed("State", px(96), Align::Left));
    }
    columns
}

fn preview_cells(job: &TorrentRow, columns: &[Column], accent: u32) -> Vec<Cell<'static>> {
    let percent = view::progress_percent(job);
    let fraction = if job.wanted_bytes == 0 {
        0.0
    } else {
        (job.verified_bytes as f64 / job.wanted_bytes as f64).clamp(0.0, 1.0)
    };
    let rate = |bps: u64| {
        if bps == 0 {
            "—".to_string()
        } else {
            format!("{} kB/s", bps / 1000)
        }
    };
    columns
        .iter()
        .map(|column| match column.title.as_ref() {
            "Name" => Cell::new(job.name.to_string()),
            "Progress" => Cell::meter(format!("{percent}%"), fraction, accent),
            "Down" => Cell::new(rate(job.down_bps)),
            "Up" => Cell::new(rate(job.up_bps)),
            "Peers" => Cell::new(format!("{} / {}", job.connected_seeds, job.connected_peers)),
            "State" => Cell::new(match job.state {
                JobState::Complete => "Complete (seeding unavailable)".to_string(),
                other => format!("{other:?}"),
            }),
            _ => Cell::new(""),
        })
        .collect()
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Pane {
    Jobs,
    Inspector,
}
struct Preview {
    text: TextCache,
    kernel: RasterKernel,
    input: Input,
    panes: Panes<Pane>,
    job_table: TableState,
    cursor: CursorShape,
    tab: usize,
    selected: usize,
    scroll: ScrollState,
    jobs: Vec<TorrentRow>,
    sequence: Sequence,
    pending_title: Option<String>,
    quit: bool,
}
fn label(ui: &mut Ui, rect: Rect, text: &str, body: TextStyle, ink: u32) {
    ui.label_styled(rect, text, body, ink, Align::Left);
}

/// Horizontal padding only. A full inset would eat a short toolbar or status band.
fn band_text(band: Rect, pad: i32) -> Rect {
    Rect::new(band.x + pad, band.y, (band.w - pad * 2).max(1), band.h)
}
impl PixelApp for Preview {
    fn render(&mut self, buffer: &mut WindowBuffer, s: Scale) {
        let width = buffer.width as i32;
        let height = buffer.height as i32;
        let full = Rect::new(0, 0, width, height);
        let (toolbar, below) = full.split_top(s.px(36));
        let (statusbar, body_band) = below.split_bottom(s.px(24));
        let area = body_band.inset(s.px(8));
        let metrics = PaneMetrics::scaled(s);
        self.panes.update(&mut self.input, area, metrics);
        self.cursor = self.panes.hover_cursor(&self.input, area, metrics);
        let regions = self.panes.panes(area);
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        let small = TextStyle::new(FaceId(0), s.font(11.0));
        let keys: Vec<String> = self.jobs.iter().map(|job| job.id.0.to_string()).collect();
        let wanted = self.jobs.get(self.selected).map(|job| job.id.0.to_string());
        if self.job_table.selected() != wanted.as_deref() {
            self.job_table.select(wanted);
        }
        let Preview {
            text,
            kernel,
            input,
            panes,
            job_table,
            tab,
            selected,
            scroll,
            jobs,
            sequence,
            ..
        } = self;
        let mut ui = Ui::new(Painter::new(buffer), text, input, app_theme(s), s, kernel);
        ui.painter.clear(ui.theme.background);
        ui.painter.fill_rect(toolbar, ui.theme.panel);
        ui.painter.horizontal_rule(
            toolbar.x,
            toolbar.bottom() - s.hairline().max(1),
            toolbar.w,
            ui.theme.panel_edge,
        );
        label(
            &mut ui,
            band_text(toolbar, s.px(8)),
            "Preview — fixture data, no transfers. Keys: 1/2 select, P pause, Q exit.",
            small,
            MUTED,
        );
        let accent = ui.theme.accent;
        for (id, rect) in regions {
            let Some(kind) = panes.get(id).copied() else {
                continue;
            };
            let inner = ui.flat_panel(rect.inset(s.px(1)), PANEL, EDGE, false);
            match kind {
                Pane::Jobs => {
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(12),
                            inner.y + s.px(2),
                            (inner.w - s.px(24)).max(1),
                            s.px(26),
                        ),
                        "Downloads",
                        body,
                        INK,
                    );
                    let table_area = Rect::new(
                        inner.x + s.px(8),
                        inner.y + s.px(26),
                        (inner.w - s.px(16)).max(1),
                        (inner.h - s.px(32)).max(1),
                    );
                    let columns = download_columns(|logical| s.px(logical), table_area.w);
                    let outcome = ui.table(job_table, table_area, &columns, &keys, |index| Row {
                        cells: preview_cells(&jobs[index], &columns, accent),
                    });
                    if let Some(key) = outcome.activated
                        && let Some(index) = jobs.iter().position(|job| job.id.0.to_string() == key)
                    {
                        *selected = index;
                        *scroll = ScrollState::new();
                    }
                }
                Pane::Inspector => {
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(12),
                            inner.y + s.px(2),
                            (inner.w - s.px(24)).max(1),
                            s.px(26),
                        ),
                        "Status",
                        body,
                        INK,
                    );
                    ui.underline_tabs(
                        tab,
                        Rect::new(
                            inner.x + s.px(8),
                            inner.y + s.px(26),
                            s.px(470).min(inner.w),
                            s.px(36),
                        ),
                        &["Details", "Files", "Peers", "Trackers"],
                        body,
                        INK,
                        MUTED,
                        INK,
                        2,
                    );
                    let content_y = inner.y + s.px(66);
                    let content = Rect::new(
                        inner.x + s.px(12),
                        content_y,
                        (inner.w - s.px(24)).max(1),
                        (inner.bottom() - content_y - s.px(8)).max(1),
                    );
                    let rowh = s.px(25);
                    match *tab {
                        0 => {
                            let job = &jobs[*selected];
                            let live = view::details_lines(job);
                            let rows = [
                                live[0].clone(),
                                live[1].clone(),
                                live[2].clone(),
                                live[3].clone(),
                                "Path: selected destination / torrent root".into(),
                                "Queue: 3 download jobs / 3 seed jobs | Upload peer slots: 8"
                                    .into(),
                                "Down: unlimited | Up: unlimited | Payload memory budget: 48 MiB"
                                    .into(),
                            ];
                            for (i, text) in rows.iter().enumerate() {
                                let row = Rect::new(
                                    content.x,
                                    content.y + i as i32 * rowh,
                                    content.w,
                                    rowh,
                                );
                                if row.bottom() <= content.bottom() {
                                    label(
                                        &mut ui,
                                        row,
                                        text,
                                        body,
                                        if i == 0 { INK } else { MUTED },
                                    );
                                }
                            }
                        }
                        1 => {
                            let heights = vec![rowh; sequence.files().len()];
                            ui.scroll_list(scroll, content, &heights, |ui, index, row| {
                                let file = &sequence.files()[index];
                                let rank = sequence.file_ranks()[index];
                                let text = format!(
                                    "{}     {} / {} bytes     {}",
                                    file.path,
                                    sequence.file_verified_bytes()[index],
                                    file.length,
                                    if rank == SKIP {
                                        "Complete / skipped".into()
                                    } else {
                                        format!("Order {}", rank + 1)
                                    }
                                );
                                label(ui, row.inset(s.px(3)), &text, body, INK);
                            });
                        }
                        2 => {
                            let rows = [
                                "COUNTRY   ADDRESS                CLIENT             DOWN        UP       PREFERENCE",
                                "--        192.0.2.10:6881        Test peer A        600 kB/s    20 kB/s  Prefer",
                                "--        198.51.100.20:6881     Test peer B        400 kB/s    10 kB/s  Normal",
                                "--        203.0.113.30:6881      Unknown            0 kB/s      0 kB/s   Avoid",
                                "Documentation IP ranges: no geolocation or network activity in this preview.",
                                "Production: offline MMDB + flag atlas, measured peer counters, real policy commands.",
                            ];
                            for (i, text) in rows.iter().enumerate() {
                                let row = Rect::new(
                                    content.x,
                                    content.y + i as i32 * rowh,
                                    content.w,
                                    rowh,
                                );
                                if row.bottom() <= content.bottom() {
                                    label(
                                        &mut ui,
                                        row,
                                        text,
                                        small,
                                        if i == 0 { MUTED } else { INK },
                                    );
                                }
                            }
                        }
                        _ => {
                            let rows = [
                                "TRACKER                          STATUS                  NEXT ANNOUNCE",
                                "tracker.example.invalid          Fixture only            Not scheduled",
                                "Automatic additions: public torrents only; private metadata disables public discovery.",
                                "Manual announce and periodic announce share tracker cooldown/backoff.",
                            ];
                            for (i, text) in rows.iter().enumerate() {
                                let row = Rect::new(
                                    content.x,
                                    content.y + i as i32 * rowh,
                                    content.w,
                                    rowh,
                                );
                                if row.bottom() <= content.bottom() {
                                    label(&mut ui, row, text, small, INK);
                                }
                            }
                        }
                    }
                }
            }
        }
        ui.painter.fill_rect(statusbar, ui.theme.panel);
        ui.painter
            .horizontal_rule(statusbar.x, statusbar.y, statusbar.w, ui.theme.panel_edge);
        label(
            &mut ui,
            band_text(statusbar, s.px(8)),
            "Preview only. Transfers are not wired.",
            small,
            MUTED,
        );
        ui.input.end_frame();
    }
    fn poll_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }
    fn cursor_shape(&self) -> CursorShape {
        self.cursor
    }
    fn on_key(&mut self, key: &KeyInput) {
        match key {
            KeyInput::Character('q') => self.quit = true,
            KeyInput::Character('1') => self.selected = 0,
            KeyInput::Character('2') => self.selected = 1,
            KeyInput::Character('p') => {
                self.jobs[1].state = if self.jobs[1].state == JobState::Paused {
                    JobState::Downloading
                } else {
                    JobState::Paused
                };
                self.pending_title = Some(format!("PREVIEW | {}", window_title(&self.jobs)));
            }
            KeyInput::Character('c') => {
                let peer = vec![u64::MAX; self.sequence.piece_count().div_ceil(64)];
                let busy = vec![0; peer.len()];
                let availability = vec![1; self.sequence.piece_count()];
                if let Ok(Some(piece)) = self.sequence.pick(&peer, &busy, &availability) {
                    // FIXTURE transition only; production must verify a cryptographic hash and write first.
                    let _ = self.sequence.mark_verified(piece as usize);
                }
            }
            _ => {}
        }
        self.input.key(key.clone());
    }
    fn on_cursor(&mut self, x: f32, y: f32) {
        self.input.cursor_moved(x, y)
    }
    fn on_cursor_left(&mut self) {
        self.input.cursor_left()
    }
    fn on_mouse(&mut self, b: MouseButton, pressed: bool) {
        self.input.mouse(b, pressed)
    }
    fn on_scroll(&mut self, dx: f32, dy: f32) {
        self.input.scrolled(dx, dy)
    }
    fn should_exit(&self) -> bool {
        self.quit
    }
}
/// Live session shell: renders engine snapshots only, never fixtures.
/// Starts empty; number keys dispatch real pause commands through the bridge
/// (rejected for unknown jobs, visibly), `S` requests shutdown, `Q` exits.
struct LiveApp {
    text: TextCache,
    kernel: RasterKernel,
    input: Input,
    panes: Panes<Pane>,
    job_table: TableState,
    bridge: LiveBridge,
    started: std::time::Instant,
    last_ack: String,
    last_title: String,
    pending_title: Option<String>,
    quit: bool,
}

impl LiveApp {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn pump(&mut self) {
        for ack in self.bridge.dispatch(self.now_ns()) {
            self.last_ack = match &ack.outcome {
                AckOutcome::Accepted => format!("ack #{} accepted", ack.seq.0),
                AckOutcome::Rejected { reason } => {
                    format!("ack #{} rejected: {reason}", ack.seq.0)
                }
            };
        }
        if self.bridge.is_shutdown_requested() {
            self.quit = true;
        }
    }
}

impl PixelApp for LiveApp {
    fn render(&mut self, buffer: &mut WindowBuffer, s: Scale) {
        self.pump();
        let snapshot = self.bridge.snapshot();
        let title = window_title(&snapshot.jobs);
        if title != self.last_title {
            self.last_title = title.clone();
            self.pending_title = Some(title);
        }
        let width = buffer.width as i32;
        let height = buffer.height as i32;
        let full = Rect::new(0, 0, width, height);
        let (toolbar, below) = full.split_top(s.px(36));
        let (statusbar, body_band) = below.split_bottom(s.px(24));
        let area = body_band.inset(s.px(8));
        let metrics = PaneMetrics::scaled(s);
        self.panes.update(&mut self.input, area, metrics);
        let regions = self.panes.panes(area);
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        let small = TextStyle::new(FaceId(0), s.font(11.0));
        let keys: Vec<String> = snapshot
            .jobs
            .iter()
            .map(|job| job.id.0.to_string())
            .collect();
        let ack = self.last_ack.clone();
        let job_count = snapshot.jobs.len();
        let LiveApp {
            text,
            kernel,
            input,
            panes,
            job_table,
            ..
        } = self;
        let mut ui = Ui::new(Painter::new(buffer), text, input, app_theme(s), s, kernel);
        ui.painter.clear(ui.theme.background);
        ui.painter.fill_rect(toolbar, ui.theme.panel);
        ui.painter.horizontal_rule(
            toolbar.x,
            toolbar.bottom() - s.hairline().max(1),
            toolbar.w,
            ui.theme.panel_edge,
        );
        label(
            &mut ui,
            band_text(toolbar, s.px(8)),
            "Live session — no fixtures. Add is not on this screen.",
            small,
            MUTED,
        );
        let accent = ui.theme.accent;
        for (id, rect) in regions {
            let Some(kind) = panes.get(id).copied() else {
                continue;
            };
            let inner = ui.flat_panel(rect.inset(s.px(1)), PANEL, EDGE, false);
            match kind {
                Pane::Jobs => {
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(12),
                            inner.y + s.px(2),
                            (inner.w - s.px(24)).max(1),
                            s.px(26),
                        ),
                        "Downloads",
                        body,
                        INK,
                    );
                    let table_area = Rect::new(
                        inner.x + s.px(8),
                        inner.y + s.px(26),
                        (inner.w - s.px(16)).max(1),
                        (inner.h - s.px(32)).max(1),
                    );
                    let columns = download_columns(|logical| s.px(logical), table_area.w);
                    let _ = ui.table(job_table, table_area, &columns, &keys, |index| Row {
                        cells: preview_cells(&snapshot.jobs[index], &columns, accent),
                    });
                    if snapshot.jobs.is_empty() {
                        let row_h = ui.theme.row_height;
                        label(
                            &mut ui,
                            Rect::new(
                                table_area.x,
                                table_area.y + row_h + s.px(8),
                                table_area.w,
                                s.px(24),
                            ),
                            "No jobs. Adding a torrent is not available on this screen.",
                            body,
                            MUTED,
                        );
                    }
                }
                Pane::Inspector => {
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(12),
                            inner.y + s.px(8),
                            (inner.w - s.px(24)).max(1),
                            s.px(24),
                        ),
                        "Status",
                        body,
                        INK,
                    );
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(12),
                            inner.y + s.px(36),
                            (inner.w - s.px(24)).max(1),
                            s.px(48),
                        ),
                        &format!("{ack}. Keys 1-9 pause that job, S shuts down, Q exits."),
                        small,
                        MUTED,
                    );
                }
            }
        }
        ui.painter.fill_rect(statusbar, ui.theme.panel);
        ui.painter
            .horizontal_rule(statusbar.x, statusbar.y, statusbar.w, ui.theme.panel_edge);
        label(
            &mut ui,
            band_text(statusbar, s.px(8)),
            &format!("Live: {job_count} job(s). {ack}"),
            small,
            MUTED,
        );
        ui.input.end_frame();
    }
    fn poll_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }
    fn cursor_shape(&self) -> CursorShape {
        CursorShape::Default
    }
    fn on_key(&mut self, key: &KeyInput) {
        match key {
            KeyInput::Character('q') => self.quit = true,
            KeyInput::Character('s') => {
                let _ = self.bridge.submit(Command::Shutdown);
            }
            KeyInput::Character(digit @ '1'..='9') => {
                let id = TorrentId(u64::from(*digit as u8 - b'0'));
                let _ = self.bridge.submit(Command::Pause(id));
            }
            _ => {}
        }
        self.input.key(key.clone());
    }
    fn on_cursor(&mut self, x: f32, y: f32) {
        self.input.cursor_moved(x, y)
    }
    fn on_cursor_left(&mut self) {
        self.input.cursor_left()
    }
    fn on_mouse(&mut self, b: MouseButton, pressed: bool) {
        self.input.mouse(b, pressed)
    }
    fn on_scroll(&mut self, dx: f32, dy: f32) {
        self.input.scrolled(dx, dy)
    }
    fn should_exit(&self) -> bool {
        self.quit
    }
}

fn load_fonts(path: PathBuf) -> Result<&'static FontSet, Box<dyn std::error::Error>> {
    use std::io::Read;
    // Bounded even if file changes between stat and read. One process-lifetime font allocation.
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("font exceeds 4 MiB preview limit".into());
    }
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let face = Face::from_bytes(bytes)
        .ok_or("font must be supported static TrueType glyf, not CFF or a collection")?;
    let face: &'static Face = Box::leak(Box::new(face));
    let mut fonts = FontSet::new();
    fonts.add(face);
    Ok(Box::leak(Box::new(fonts)))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let flag = args
        .next()
        .ok_or("usage: templartorrent-preview --font /path/to/font.ttf [--live]")?;
    if flag != "--font" {
        return Err("expected --font".into());
    }
    let font = args.next().ok_or("missing font path")?;
    let live = match args.next() {
        None => false,
        Some(extra) if extra == "--live" => true,
        Some(_) => return Err("unexpected extra argument".into()),
    };
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let fonts = load_fonts(PathBuf::from(font))?;
    if live {
        let mut panes = Panes::new(Pane::Jobs);
        let root = panes.panes(Rect::new(0, 0, 1100, 700))[0].0;
        panes
            .split(root, Side::Bottom, Pane::Inspector, 0.38)
            .ok_or("could not split panes")?;
        let app = LiveApp {
            text: TextCache::new(fonts),
            kernel: RasterKernel::new(),
            input: Input::new(),
            panes,
            job_table: TableState::new(),
            bridge: LiveBridge::new(Limits::default())?,
            started: std::time::Instant::now(),
            last_ack: "no commands yet".into(),
            last_title: String::new(),
            pending_title: None,
            quit: false,
        };
        run_app(
            app,
            WindowConfig::new("TemplarTorrent", 300.0, 500.0).min_size(260.0, 320.0),
        )?;
        return Ok(());
    }
    let mut panes = Panes::new(Pane::Jobs);
    let root = panes.panes(Rect::new(0, 0, 1100, 700))[0].0;
    panes
        .split(root, Side::Bottom, Pane::Inspector, 0.38)
        .ok_or("could not split panes")?;
    let jobs = vec![
        TorrentRow {
            id: TorrentId(1),
            name: Arc::from("Example series - Season 1"),
            state: JobState::Downloading,
            wanted_bytes: 1_000_000_000,
            verified_bytes: 400_000_000,
            total_payload_downloaded: 410_000_000,
            total_payload_uploaded: 20_000_000,
            down_bps: 1_000_000,
            up_bps: 30_000,
            connected_seeds: 4,
            connected_peers: 8,
            estimated_seeds: Some(40),
            estimated_peers: Some(80),
        },
        TorrentRow {
            id: TorrentId(2),
            name: Arc::from("Example album"),
            state: JobState::Downloading,
            wanted_bytes: 400_000_000,
            verified_bytes: 200_000_000,
            total_payload_downloaded: 200_000_000,
            total_payload_uploaded: 0,
            down_bps: 250_000,
            up_bps: 0,
            connected_seeds: 1,
            connected_peers: 3,
            estimated_seeds: None,
            estimated_peers: None,
        },
    ];
    let sequence = Sequence::new(
        (1..=12)
            .map(|i| FileSpec {
                path: format!("Episode {i:02}.mkv"),
                length: 6,
                wanted: true,
            })
            .collect(),
        4,
        100,
    )?;
    let app = Preview {
        text: TextCache::new(fonts),
        kernel: RasterKernel::new(),
        input: Input::new(),
        panes,
        job_table: TableState::new(),
        cursor: CursorShape::Default,
        tab: 0,
        selected: 0,
        scroll: ScrollState::new(),
        pending_title: Some(format!("PREVIEW | {}", window_title(&jobs))),
        jobs,
        sequence,
        quit: false,
    };
    run_app(
        app,
        WindowConfig::new("TemplarTorrent preview", 300.0, 500.0).min_size(260.0, 320.0),
    )?;
    Ok(())
}
