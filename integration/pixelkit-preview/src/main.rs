//! Interactive layout reference with FIXTURE DATA. No live transfers or fake working controls.
//! Run with --font /path/to/a/static-TrueType.ttf. No font files are distributed here.
#![forbid(unsafe_code)]
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, KeyInput, MouseButton, Scale};
use pixelkit_text::font::{Face, FaceId, FontSet};
use pixelkit_text::{Align, TextCache, TextStyle};
use pixelkit_ui::{ButtonStyle, ScrollState, Theme, Ui};
use pixelkit_windowing::{CursorShape, PaneMetrics, Panes, PixelApp, Side, WindowConfig, run_app};
use std::{path::PathBuf, sync::Arc};
use templartorrent_core::{
    model::{JobState, TorrentId, TorrentRow, window_title},
    sequence::{FileSpec, SKIP, Sequence},
};
const BG: u32 = 0xf4f4f2;
const PANEL: u32 = 0xffffff;
const INK: u32 = 0x242729;
const MUTED: u32 = 0x676d72;
const EDGE: u32 = 0xd3d6d8;
const SELECTED: u32 = 0xe6edf2;
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
    cursor: CursorShape,
    tab: usize,
    selected: usize,
    scroll: ScrollState,
    jobs: Vec<TorrentRow>,
    sequence: Sequence,
    pending_title: Option<String>,
    quit: bool,
}
fn style() -> ButtonStyle {
    ButtonStyle {
        text: MUTED,
        text_active: INK,
        fill: PANEL,
        fill_hover: BG,
        fill_active: SELECTED,
        edge: EDGE,
        edge_active: MUTED,
        radius: 0,
    }
}
fn label(ui: &mut Ui, rect: Rect, text: &str, body: TextStyle, ink: u32) {
    ui.label_styled(rect, text, body, ink, Align::Left);
}
impl PixelApp for Preview {
    fn render(&mut self, buffer: &mut WindowBuffer, s: Scale) {
        let width = buffer.width as i32;
        let height = buffer.height as i32;
        let top = s.px(78);
        let foot = s.px(32);
        let area = Rect::new(
            s.px(8),
            top,
            (width - s.px(16)).max(1),
            (height - top - foot).max(1),
        );
        let metrics = PaneMetrics::scaled(s);
        self.panes.update(&mut self.input, area, metrics);
        self.cursor = self.panes.hover_cursor(&self.input, area, metrics);
        let regions = self.panes.panes(area);
        let body = TextStyle::new(FaceId(0), s.font(13.0));
        let small = TextStyle::new(FaceId(0), s.font(11.0));
        let mut ui = Ui::new(
            Painter::new(buffer),
            &mut self.text,
            &mut self.input,
            Theme::default(),
            s,
            &mut self.kernel,
        );
        ui.painter.clear(BG);
        label(
            &mut ui,
            Rect::new(s.px(16), s.px(8), width - s.px(32), s.px(23)),
            "TemplarTorrent  /  INTERACTIVE LAYOUT PREVIEW - fixture data, no transfers",
            body,
            INK,
        );
        if let Some(index) = ui.segmented(
            Rect::new(s.px(16), s.px(40), s.px(270), s.px(28)),
            &["Torrent 1", "Torrent 2"],
            self.selected,
            body,
            style(),
            4,
        ) {
            self.selected = index;
            self.scroll = ScrollState::new();
        }
        label(
            &mut ui,
            Rect::new(s.px(302), s.px(42), width - s.px(320), s.px(25)),
            "Drag divider | P: toggle second job | C: verify next fixture piece | Q: exit",
            small,
            MUTED,
        );
        for (id, rect) in regions {
            let Some(kind) = self.panes.get(id).copied() else {
                continue;
            };
            let inner = ui.flat_panel(rect.inset(s.px(2)), PANEL, EDGE, false);
            match kind {
                Pane::Jobs => {
                    let rowh = s.px(30);
                    label(
                        &mut ui,
                        Rect::new(
                            inner.x + s.px(10),
                            inner.y + s.px(5),
                            inner.w - s.px(20),
                            rowh,
                        ),
                        "NAME                              PROGRESS      DOWN           UP              STATUS",
                        small,
                        MUTED,
                    );
                    for (index, job) in self.jobs.iter().enumerate() {
                        let row =
                            Rect::new(inner.x, inner.y + rowh * (index as i32 + 1), inner.w, rowh);
                        if row.bottom() > inner.bottom() {
                            break;
                        }
                        if index == self.selected {
                            ui.painter.fill_rect(row, SELECTED);
                        }
                        let percent = job
                            .verified_bytes
                            .saturating_mul(100)
                            .checked_div(job.wanted_bytes)
                            .unwrap_or(0);
                        let text = format!(
                            "{}     {}%     {} kB/s     {} kB/s     {:?}",
                            job.name,
                            percent,
                            job.down_bps / 1000,
                            job.up_bps / 1000,
                            job.state
                        );
                        label(&mut ui, row.inset(s.px(8)), &text, body, INK);
                    }
                }
                Pane::Inspector => {
                    ui.underline_tabs(
                        &mut self.tab,
                        Rect::new(inner.x + s.px(8), inner.y, s.px(470).min(inner.w), s.px(36)),
                        &["Details", "Files", "Peers", "Trackers"],
                        body,
                        INK,
                        MUTED,
                        INK,
                        2,
                    );
                    let content = Rect::new(
                        inner.x + s.px(12),
                        inner.y + s.px(44),
                        (inner.w - s.px(24)).max(1),
                        (inner.h - s.px(52)).max(1),
                    );
                    let rowh = s.px(25);
                    match self.tab {
                        0 => {
                            let job = &self.jobs[self.selected];
                            let rows = [
                                format!("{}", job.name),
                                format!(
                                    "Downloaded: {} bytes     Uploaded: {} bytes",
                                    job.total_payload_downloaded, job.total_payload_uploaded
                                ),
                                format!(
                                    "Verified wanted data: {} / {} bytes",
                                    job.verified_bytes, job.wanted_bytes
                                ),
                                format!(
                                    "ETA: {}     Seeds: {}     Peers: {}",
                                    job.eta_seconds()
                                        .map(|v| format!("{v} s"))
                                        .unwrap_or_else(|| "Unknown".into()),
                                    job.connected_seeds,
                                    job.connected_peers
                                ),
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
                            let heights = vec![rowh; self.sequence.files().len()];
                            let sequence = &self.sequence;
                            ui.scroll_list(
                                &mut self.scroll,
                                content,
                                &heights,
                                |ui, index, row| {
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
                                },
                            );
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
        label(
            &mut ui,
            Rect::new(
                s.px(14),
                height - foot + s.px(6),
                width - s.px(28),
                s.px(20),
            ),
            "Preview only. Add/remove/limits and live transfers are specified in DESIGN.md, not wired to this screen.",
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
        .ok_or("usage: templartorrent-preview --font /path/to/font.ttf")?;
    if flag != "--font" {
        return Err("expected --font".into());
    }
    let font = args.next().ok_or("missing font path")?;
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let fonts = load_fonts(PathBuf::from(font))?;
    let mut panes = Panes::new(Pane::Jobs);
    let root = panes.panes(Rect::new(0, 0, 1100, 700))[0].0;
    panes
        .split(root, Side::Bottom, Pane::Inspector, 0.45)
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
        WindowConfig::new("TemplarTorrent preview", 1120.0, 760.0).min_size(720.0, 480.0),
    )?;
    Ok(())
}
