use crate::cache;
use crate::command::{Command, Mode};
use crate::config::{self, Config};
use crate::dashboard;
use crate::editor;
use crate::encode;
use crate::media;
use crate::render;
use crate::sensors;
use crate::video;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Tray menu item ids, shared between `main` (menu construction) and the
/// dedicated tray-event thread (`spawn_tray_thread`).
#[derive(Clone)]
pub struct TrayIds {
    pub open_id: tray_icon::menu::MenuId,
    pub quit_id: tray_icon::menu::MenuId,
}

impl Default for TrayIds {
    fn default() -> Self {
        Self {
            open_id: tray_icon::menu::MenuId::new("astroshel-lcd-open"),
            quit_id: tray_icon::menu::MenuId::new("astroshel-lcd-quit"),
        }
    }
}

/// Phase 3b-i Task B: shared state for a background media-load worker thread,
/// spawned when the user picks a file via "Add Media...". `update` polls
/// this (via `App::loading`) each frame to draw a progress bar/label, and to
/// pick up the finished `Media` (plus, for a GIF, the pre-built preview compose
/// cache) once `result` is populated -- this is what keeps `media::load` (and,
/// for a big GIF, the initial `render::compose_frames` preview cache) off the
/// UI thread so the window never freezes on open.
///
/// Task 4a: a picked VIDEO file spawns a different worker (`video::import_to_cache`)
/// that populates `video_result` instead of `result` -- both fields live on the same
/// struct (rather than a separate type) since only one worker is ever in flight at a
/// time (`loading.is_none()` gates the "Add Media..." button), so `update`'s single
/// poll just checks whichever field the in-flight worker populates.
struct LoadProgress {
    done: usize,
    total: usize,
    stage: String,
    /// `Ok((media, Some(composed)))` for an animated GIF (the pre-built preview
    /// cache, mirroring `gif_composed`), `Ok((media, None))` for a static image
    /// (composed on demand, same as the old inline path), or `Err(message)`.
    result: Option<Result<(media::Media, Option<Vec<(Vec<u8>, u32)>>), String>>,
    /// Populated by a video-import worker (`video::import_to_cache`) instead of
    /// `result` above: the cached packets/dims/fps ready for `cache::write_lcdv`,
    /// plus the first composed 320x320 frame as a thumbnail.
    video_result: Option<Result<(cache::CachedVideo, image::RgbaImage), String>>,
}

/// Task 4a: in-process playback state for previewing a cached (`.lcdv`) video --
/// no ffmpeg involved. `packets`/`fps` mirror `cache::CachedVideo`; `decoder`
/// decodes them sequentially (paced by the preview's own fast-repaint cadence,
/// see `preview_rgba`); `idx` is the next packet to decode; `last` is the most
/// recently decoded frame, shown again while a packet decodes to `None` (a
/// skipped/parameter-only packet) or errors, so playback never flashes blank.
struct VideoPreview {
    decoder: encode::VideoDecoder,
    packets: Vec<(Vec<u8>, u32)>,
    idx: usize,
    fps: f32,
    last: Option<Vec<u8>>,
    // Wall-clock pacing so the preview plays at the video's real speed regardless of
    // egui's repaint rate (which spikes on mouse-move -- otherwise hovering visibly
    // speeds the clip up). `frame_ms` is the per-frame interval (baked at import);
    // `last_advance` is when the currently-shown frame started. `preview_rgba` only
    // advances/decodes the next frame once `frame_ms` of real time has elapsed.
    frame_ms: u32,
    last_advance: Option<std::time::Instant>,
}

/// Decodes forward from `vp.idx` to the next producible 320x320 RGBA frame, advancing
/// `idx` and, at the stream end, wrapping to 0 + recreating the decoder (P-frames can't
/// resume across a reset). Returns the decoded raw RGBA, or `None` if a full pass over the
/// packets yields no frame. The decoder state persists across calls, so sequential calls
/// walk the stream correctly; callers pace HOW OFTEN they call this by wall clock.
fn decode_next_video_frame(vp: &mut VideoPreview, n: usize) -> Option<Vec<u8>> {
    let mut attempts = 0usize;
    loop {
        let (pkt, _delay) = &vp.packets[vp.idx];
        match vp.decoder.decode(pkt) {
            Ok(Some(rgba)) => {
                vp.idx = (vp.idx + 1) % n;
                return Some(rgba.into_raw());
            }
            Ok(None) => {
                vp.idx += 1;
                if vp.idx >= n {
                    vp.idx = 0;
                    if let Ok(d) = encode::new_video_decoder() {
                        vp.decoder = d;
                    }
                }
            }
            Err(_) => {
                vp.idx += 1;
                if vp.idx >= n {
                    vp.idx = 0;
                    if let Ok(d) = encode::new_video_decoder() {
                        vp.decoder = d;
                    }
                }
                return None;
            }
        }
        attempts += 1;
        if attempts > n {
            return None;
        }
    }
}

pub struct App {
    cfg: Config,
    cfg_path: PathBuf,
    tx: Sender<Command>,
    status: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    has_tray: bool,
    // Kept alive for the lifetime of the app; dropping it removes the tray icon.
    // None when tray build failed; app degrades to windowed-only mode.
    _tray: Option<tray_icon::TrayIcon>,
    // Tracks whether we've shown the window on the first frame (for no-tray case).
    shown_on_first_frame: bool,
    // Phase 2c: live preview pane state. `renderer`/`sensors` are the same
    // dashboard-rendering pipeline the device output uses (WYSIWYG); `media_obj` is
    // preloaded once from `cfg.media.path` (if any) rather than re-decoded every frame.
    renderer: render::Renderer,
    sensors: sensors::Sensors,
    media_obj: Option<media::Media>,
    preview_tex: Option<egui::TextureHandle>,
    // Phase 3a-i Task 5: the working dashboard (theme + widgets) being edited by the
    // Template/Theme/Colors pickers below. Initialized from `cfg.dashboard` and only
    // written back into `cfg.dashboard` (and sent to the device) on Apply, so editing
    // the pickers never touches the persisted config until the user commits. `dash_hist`
    // is the matching per-widget sample history for Sparkline previews, fed every frame
    // by `preview_rgba` -- a fresh `History` local to the GUI process (independent of the
    // pipeline thread's own `History` in `pipeline.rs`).
    dashboard: dashboard::Dashboard,
    dash_hist: render::History,
    // Phase 3b: wall-clock epoch used to animate GIF previews. Reset whenever a new
    // media file is loaded (Open handler) so playback always restarts from frame 0.
    gif_epoch: std::time::Instant,
    // Perf: pre-composed (320x320 RGBA, delay_ms) frames for the currently-loaded
    // animated GIF, mirroring the pipeline's `gif_composed` -- computed once via
    // `render::compose_frames` rather than re-running crop+resize on the full-resolution
    // source frame every preview repaint. `gif_compose_key` is the (path, fit, zoom, pan)
    // tuple `gif_composed` was last built from; `preview_rgba` recomputes only when this
    // key no longer matches the current settings (media loaded, or fit/zoom/pan edited,
    // e.g. via the live pan-drag below), keeping playback itself encode/display-only.
    gif_composed: Option<Vec<(Vec<u8>, u32)>>,
    gif_compose_key: Option<(String, media::Fit, f32, [f32; 2])>,
    // Polish round: whether the "Colors" modal (Accent/Background/Text/Track
    // pickers) is open. Toggled by the "Colors…" button; the egui::Window's
    // title-bar X (via `.open(&mut self.show_colors)`) sets it back to false.
    show_colors: bool,
    // Phase 3a fix: cached sensor snapshot for the preview, throttled to ~1 Hz.
    // egui repaints (e.g. every mouse move) would otherwise call `self.sensors.read()`
    // many times per second; since CPU%/GPU% are measured over the interval since the
    // previous read, over-frequent reads yield wildly jumping values. Caching the
    // snapshot and only refreshing it once per second keeps the preview's numbers
    // stable while still updating live. The device pipeline (pipeline.rs) has its own
    // independent 1 Hz read cadence and is unaffected by this cache.
    last_snap: Option<crate::sensors::Snapshot>,
    last_snap_at: std::time::Instant,
    // Phase 3a-ii Task 2: index into `self.dashboard.widgets` of the widget currently
    // shown in the "Widget settings" panel, or `None` if nothing is selected. Reset to
    // `None` whenever the widget list changes shape out from under it (template switch,
    // delete) so it never points at a stale/out-of-range index.
    selected: Option<usize>,
    // Phase 3a-ii Task 3: which part of the selected widget's rect is being dragged
    // (Body/corner), set on drag start and cleared on drag stop. `None` means no drag
    // is in progress (or the press wasn't on the selected widget/its handles), in which
    // case `dragged()` deltas are ignored -- this is what lets a plain click-to-select
    // coexist with drag-to-move/resize on the same `Sense::click_and_drag()` response.
    drag_grab: Option<editor::Grab>,
    // Phase 3b-i Task A: shared with the pipeline worker thread, set while it's
    // (re)loading/pre-encoding media (GIF pre-encode or a static image load) so the GUI
    // can show an "Applying..." indicator instead of appearing to hang.
    media_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Phase 3b-i Task B: set while a background "Open image/GIF..." worker thread is
    // decoding/composing the picked file (see `LoadProgress`); `None` when no load is
    // in flight. `update` polls it every frame, drawing a progress bar/label in the
    // preview panel while `Some`, and swaps the finished result into `media_obj` (+
    // `gif_composed`/`gif_compose_key`) once `result` is populated.
    loading: Option<std::sync::Arc<std::sync::Mutex<LoadProgress>>>,
    // The path the in-flight `loading` worker is loading, applied to `cfg.media.path`
    // only once the load succeeds (mirrors the old inline behavior of only touching
    // `cfg.media.path` on a successful `media::load`).
    loading_path: Option<String>,
    // Bugfix: the bottom status label used to be permanent, so "Applied" (set
    // eagerly on click, before the pipeline had actually reloaded/pre-encoded
    // anything) would sit there forever, contradicting the "Applying to cooler…"
    // spinner shown at the same time. `status_until` makes `self.status` transient:
    // the label only renders while `Instant::now() < status_until`, set via
    // `set_status` below. `None`/expired means nothing is shown.
    status_until: Option<std::time::Instant>,
    // Falling-edge detector for `media_busy`: `update` compares this frame's busy
    // read against the previous frame's, so it can show "Applied ✓" exactly when a
    // media (re)load/pre-encode kicked off by `apply()` actually finishes, instead
    // of the instant the Apply button is clicked.
    was_busy: bool,
    // Media bitrate slider: whether the "Bitrate — please read" warning modal is
    // currently open. Toggled true when the slider is dragged (see `update`, gated
    // by `bitrate_warned_session` below) and closed via the modal's own "Got it"
    // button or the egui::Window title-bar X (`.open(&mut self.bitrate_modal)`).
    bitrate_modal: bool,
    // Mirrors the modal's "Don't show this again" checkbox; only actually suppresses
    // future warnings (by clearing `cfg.show_bitrate_warning` + saving) once "Got it"
    // is clicked, so closing the modal via the title bar with the box ticked doesn't
    // silently persist the choice.
    bitrate_dont_show: bool,
    // Session guard: true once the bitrate warning modal has been shown this run, so
    // it opens at most once per app launch (on the first slider drag) rather than
    // re-popping on every drag tick even while `cfg.show_bitrate_warning` is true.
    bitrate_warned_session: bool,
    // Task 4a: `.lcdv` path of the currently-selected cached video (if the current
    // media is a cached video rather than an image/GIF), and the original source
    // file it was imported from (kept so "Re-import" can re-run the import with new
    // fit/zoom/pan/bitrate without re-picking the file). Both `None` when the
    // current media is an image/GIF (or nothing). Cleared whenever a new image/GIF
    // finishes loading; NOT cleared on a Dashboard/Media mode toggle, so switching
    // back to Media mode re-shows the same cached video (see `preview_rgba`).
    current_video_lcdv: Option<String>,
    current_video_src: Option<String>,
    // The (fit, zoom, pan, bitrate_kbps) the current cached video was last
    // imported/re-imported with -- compared against `self.cfg.media`'s live values
    // to decide whether the "Re-import" button should be enabled (`None` when
    // there's no cached video selected, matching `current_video_lcdv`).
    video_import_settings: Option<(String, f32, [f32; 2], u32)>,
    // Task 4a: in-process decode/playback state for a selected cached video's
    // preview (see `VideoPreview`). `None` whenever the current media isn't a
    // cached video, or Media mode isn't active (see `preview_rgba`'s clearing at
    // the top, which stops playback whenever the user leaves Media mode).
    video_preview: Option<VideoPreview>,
    // Task 4b: id of the cache entry currently selected as "the current media"
    // (video/image/gif), or `None` in Dashboard mode / nothing selected -- fed
    // into a saved Profile's `media_id` and set whenever a media becomes
    // current (video import, image/GIF load that got a Recent entry,
    // Recent-grid click, or loading a media Profile). Cleared when switching
    // back to Dashboard mode so a Profile saved from Dashboard mode doesn't
    // inherit a stale media reference (see `update`'s mode-edge check).
    current_media_id: Option<String>,
    // Task 4b: lazily-loaded thumbnail textures for the Recent-media grid,
    // keyed by cache entry id -- decoded from the entry's `.png` thumb once
    // and reused every frame rather than re-decoding on every repaint.
    recent_thumbs: std::collections::HashMap<String, egui::TextureHandle>,
    // Task 4b: the Recent-media index, loaded once at startup and reloaded
    // (via `refresh_recent`) after anything that adds/removes/pins a cache
    // entry (import, Recent-grid remove, Clear Cache, Profile save) so the
    // grid stays in sync with disk.
    recent: cache::CacheIndex,
    // Task 4b: `Some(id)` while an in-flight `loading` worker (spawned by
    // `spawn_media_load`) is re-loading media that's ALREADY a Recent-media
    // entry (Recent-grid click, or a Profile referencing cached media) rather
    // than a fresh "Add Media..." pick -- the completion handler in `update`
    // uses this to skip re-adding a duplicate Recent entry and to avoid
    // resetting zoom/pan back to defaults.
    loading_existing_id: Option<String>,
    // Task 4b: whether the "⚙ Settings" modal (Clear Cache + cache size) is open.
    show_settings: bool,
    // Task 4b: the name typed into the "Save current as…" field, cleared once
    // the profile is saved.
    new_profile_name: String,
}

/// All `WidgetKind` variants, in the order offered by the "Add widget" picker.
/// `dashboard::WidgetKind` has no built-in enumerator, so the GUI keeps this list
/// in sync by hand; `dashboard::tests` would catch a variant being added there
/// without a matching renderer/sensors branch, but not a missing entry here, so
/// keep this list updated whenever `WidgetKind` gains a variant.
const ALL_WIDGET_KINDS: [dashboard::WidgetKind; 7] = [
    dashboard::WidgetKind::GpuTemp,
    dashboard::WidgetKind::GpuUsage,
    dashboard::WidgetKind::CpuUsage,
    dashboard::WidgetKind::RamUsage,
    dashboard::WidgetKind::Clock,
    dashboard::WidgetKind::Date,
    dashboard::WidgetKind::Text,
];

/// The four sensor-driven widget kinds that carry a min/max range and a choice of
/// Gauge/Ring/Bar/Number/Sparkline visualization. Clock/Date/Text are not "metric"
/// (no numeric range to scale against).
/// Task 4b: total size in bytes of every file directly under `cache::cache_dir()`
/// (the `.lcdv`/`.png` cache entries + `index.toml`) -- shown in the ⚙ Settings
/// modal as "Cache: N MB". Best-effort: an unreadable dir or entry contributes 0
/// rather than erroring.
fn cache_dir_size() -> u64 {
    std::fs::read_dir(cache::cache_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn is_metric_kind(kind: dashboard::WidgetKind) -> bool {
    matches!(
        kind,
        dashboard::WidgetKind::GpuTemp
            | dashboard::WidgetKind::GpuUsage
            | dashboard::WidgetKind::CpuUsage
            | dashboard::WidgetKind::RamUsage
    )
}

impl App {
    pub fn new(
        cfg: Config,
        cfg_path: PathBuf,
        tx: Sender<Command>,
        tray: Option<tray_icon::TrayIcon>,
        has_tray: bool,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        media_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        // Renderer init only fails if the embedded font asset fails to parse, which
        // would be a build-time asset defect, not a runtime condition -- so treat it
        // like the other places (render.rs tests) that unwrap-on-init.
        let mut renderer = render::new().expect("renderer init");
        renderer.twelve_hour = cfg.twelve_hour;
        let sensors = sensors::new();
        // Video is no longer a live `Media` variant loaded via `media::load` -- a
        // configured video path is simply skipped here; cached-video selection
        // (Recent-media grid, Task 4b) isn't persisted to `cfg.media.path` yet, so
        // there's nothing to preload for it across a restart. Static/GIF still
        // preload unconditionally (cheap, keeps switching into Media mode instant).
        let media_obj = cfg.media.path.as_ref().and_then(|p| {
            let path = std::path::Path::new(p);
            if media::is_video_path(path) {
                None
            } else {
                media::load(path).ok()
            }
        });
        let dashboard = cfg.dashboard.clone();
        Self {
            cfg,
            cfg_path,
            tx,
            status: "Running".into(),
            stop,
            has_tray,
            _tray: tray,
            shown_on_first_frame: false,
            renderer,
            sensors,
            media_obj,
            preview_tex: None,
            gif_epoch: std::time::Instant::now(),
            gif_composed: None,
            gif_compose_key: None,
            dashboard,
            dash_hist: render::History::new(),
            show_colors: false,
            last_snap: None,
            last_snap_at: std::time::Instant::now(),
            selected: None,
            drag_grab: None,
            media_busy,
            loading: None,
            loading_path: None,
            status_until: None,
            was_busy: false,
            bitrate_modal: false,
            bitrate_dont_show: false,
            bitrate_warned_session: false,
            current_video_lcdv: None,
            current_video_src: None,
            video_import_settings: None,
            video_preview: None,
            current_media_id: None,
            recent_thumbs: std::collections::HashMap::new(),
            recent: cache::load_index(),
            loading_existing_id: None,
            show_settings: false,
            new_profile_name: String::new(),
        }
    }

    /// Task 4a: (re)loads the in-process preview decoder for the cached video at
    /// `lcdv_path` -- reads the `.lcdv` packets back off disk and spins up a fresh
    /// `encode::VideoDecoder` for them. Called right after a successful import, and
    /// whenever `preview_rgba` finds a cached video selected but no preview state
    /// (e.g. switching back into Media mode after it was stopped, see `preview_rgba`).
    /// On failure, clears `video_preview` and shows a transient error rather than
    /// leaving stale/partial state around.
    fn load_video_preview(&mut self, lcdv_path: &str) {
        match cache::read_lcdv(std::path::Path::new(lcdv_path)) {
            Ok(cv) => match encode::new_video_decoder() {
                Ok(decoder) => {
                    // Per-frame interval for wall-clock pacing: the delay baked into the
                    // first packet (all uniform from import), falling back to fps.
                    let frame_ms = cv
                        .frames
                        .first()
                        .map(|(_, d)| *d)
                        .filter(|d| *d > 0)
                        .unwrap_or_else(|| ((1000.0 / cv.fps.max(1.0)).round() as u32).max(40));
                    self.video_preview = Some(VideoPreview {
                        decoder,
                        packets: cv.frames,
                        idx: 0,
                        fps: cv.fps,
                        last: None,
                        frame_ms,
                        last_advance: None,
                    });
                }
                Err(e) => {
                    self.set_status(format!("Preview decoder init failed: {e}"), 6);
                    self.video_preview = None;
                }
            },
            Err(e) => {
                self.set_status(format!("Failed to load cached video: {e}"), 6);
                self.video_preview = None;
            }
        }
    }

    /// Task 4a: spawns a background worker running `video::import_to_cache` for
    /// `src`, using the CURRENT `self.cfg.media.fit/zoom/pan/bitrate_kbps` -- shared
    /// by both the initial "Add Media..." video pick and the "Re-import" button (the
    /// latter re-runs the same import with whatever settings have changed since).
    /// Populates `LoadProgress::video_result` (polled by `update`) rather than
    /// `result`, which is reserved for the image/GIF loader.
    fn spawn_video_import(&mut self, src: PathBuf) {
        let ps = src.display().to_string();
        let progress = std::sync::Arc::new(std::sync::Mutex::new(LoadProgress {
            done: 0,
            total: 1,
            stage: "Importing…".to_string(),
            result: None,
            video_result: None,
        }));
        let worker_progress = progress.clone();
        let fit = crate::media::Fit::from_str(&self.cfg.media.fit);
        let zoom = self.cfg.media.zoom;
        let pan = self.cfg.media.pan;
        let bitrate_kbps = self.cfg.media.bitrate_kbps;
        std::thread::spawn(move || {
            let result = video::import_to_cache(&src, fit, zoom, pan, bitrate_kbps, &mut |done, total| {
                if let Ok(mut p) = worker_progress.lock() {
                    p.done = done;
                    p.total = total;
                    p.stage = format!("Importing frame {done}/{total}");
                }
            });
            if let Ok(mut p) = worker_progress.lock() {
                p.video_result = Some(result.map_err(|e| e.to_string()));
            }
        });
        self.loading_path = Some(ps);
        self.loading = Some(progress);
        self.status = "Importing…".into();
    }

    /// Task 4b: spawns a background worker that decodes (and, for a GIF,
    /// pre-composes the preview cache for) the image/GIF at `path`, exactly
    /// the same worker the original "Add Media..." button used inline before
    /// this was extracted -- shared by that button, by re-selecting an
    /// image/GIF from the Recent-media grid, and by loading a Profile that
    /// references one (see `select_cache_entry`). Callers wanting the
    /// "already-cached, don't re-add to Recent" behavior must set
    /// `self.loading_existing_id` themselves AFTER calling this (this method
    /// only ever starts the worker/progress state, never touches that field).
    fn spawn_media_load(&mut self, path: PathBuf) {
        let ps = path.display().to_string();
        // Task B-2: decode (+ for a GIF, pre-compose the preview cache) off the
        // UI thread so opening a big file doesn't freeze the window. The worker
        // only ever touches `progress`/`fit_str` (both moved in); `update`'s
        // poll is the sole place that then applies the result to `self`.
        let progress = std::sync::Arc::new(std::sync::Mutex::new(LoadProgress {
            done: 0,
            total: 1,
            stage: "Decoding…".to_string(),
            result: None,
            video_result: None,
        }));
        let worker_progress = progress.clone();
        let fit_str = self.cfg.media.fit.clone();
        std::thread::spawn(move || {
            let fit = crate::media::Fit::from_str(&fit_str);
            match media::load(&path) {
                Ok(media::Media::Animated(frames)) => {
                    let total = frames.len().max(1);
                    if let Ok(mut p) = worker_progress.lock() {
                        p.stage = "Composing…".to_string();
                        p.total = total;
                        p.done = 0;
                    }
                    // Compose incrementally (rather than one
                    // `render::compose_frames` call) so `done` advances
                    // and the progress bar actually moves for a big GIF.
                    let mut composed = Vec::with_capacity(frames.len());
                    for f in &frames {
                        let (sw, sh) = f.img.dimensions();
                        let pl = crate::media::placement(sw, sh, fit, 1.0, [0.0, 0.0]);
                        let rgba = crate::render::compose_media(&f.img, &pl, [8, 10, 18, 255]);
                        composed.push((rgba, f.delay_ms));
                        if let Ok(mut p) = worker_progress.lock() {
                            p.done += 1;
                        }
                    }
                    if let Ok(mut p) = worker_progress.lock() {
                        p.result = Some(Ok((media::Media::Animated(frames), Some(composed))));
                    }
                }
                // Static image delivers straight into `media_obj` with
                // no compose-cache -- cheap to compose on demand.
                Ok(m @ media::Media::Static(_)) => {
                    if let Ok(mut p) = worker_progress.lock() {
                        p.stage = "Done".to_string();
                        p.done = 1;
                        p.total = 1;
                        p.result = Some(Ok((m, None)));
                    }
                }
                Err(e) => {
                    if let Ok(mut p) = worker_progress.lock() {
                        p.result = Some(Err(e.to_string()));
                    }
                }
            }
        });
        self.loading_path = Some(ps);
        self.loading = Some(progress);
        self.status = "Loading…".into();
    }

    /// Task 4b: reloads `self.recent` from `cache/index.toml` -- called after
    /// anything that mutates the on-disk index out from under it (import,
    /// Recent-grid remove, Clear Cache, pinning for a saved Profile) so the
    /// Recent-media grid stays in sync with disk.
    fn refresh_recent(&mut self) {
        self.recent = cache::load_index();
        // Prune GPU texture handles for any id no longer present in the index --
        // otherwise entries that age out via `evict_unpinned(10)` (or get removed
        // by Clear Cache / the ✕ button) leak their decoded thumbnail texture for
        // the lifetime of the tray session, since `recent_thumbs` is only ever
        // inserted into (see the Recent-grid rendering), never pruned on its own.
        let live: std::collections::HashSet<&str> =
            self.recent.entries.iter().map(|e| e.id.as_str()).collect();
        self.recent_thumbs.retain(|id, _| live.contains(id.as_str()));
    }

    /// Task 4b: selects cache entry `e` as "the current media" -- shared by
    /// clicking a thumbnail in the Recent-media grid and by loading a Profile
    /// that references cached media (`load_profile`). Mirrors what a freshly
    /// completed import/load does: video loads its `.lcdv` straight into a
    /// fresh preview decoder (no background worker needed, it's already on
    /// disk); image/GIF re-runs the same background loader "Add Media..."
    /// uses, but tags the load as `loading_existing_id` so the completion
    /// handler in `update` doesn't re-add a duplicate Recent entry.
    fn select_cache_entry(&mut self, e: &cache::CacheEntry) {
        // Defense-in-depth: the Recent-grid thumbnail button (and the Profile
        // "Load" button that calls through here) already disables itself while
        // `self.loading` is in flight, but guard here too so a stale worker's
        // completion can't be raced by a selection change mid-load.
        if self.loading.is_some() {
            return;
        }
        if e.kind == "video" {
            self.media_obj = None;
            self.gif_composed = None;
            self.gif_compose_key = None;
            self.video_preview = None;
            self.current_video_lcdv = Some(e.path.clone());
            // The original source file isn't tracked in the cache entry (only
            // the already-encoded `.lcdv`), so Re-import stays unavailable for
            // a video selected this way -- exactly as it does for a fresh
            // import until fit/zoom/pan/bitrate is changed.
            self.current_video_src = None;
            self.video_import_settings = None;
            self.current_media_id = Some(e.id.clone());
            self.load_video_preview(&e.path);
            self.set_status("Selected (click Apply)", 3);
        } else {
            self.current_video_lcdv = None;
            self.current_video_src = None;
            self.video_import_settings = None;
            self.video_preview = None;
            self.spawn_media_load(PathBuf::from(&e.path));
            self.loading_existing_id = Some(e.id.clone());
        }
    }

    /// Task 4b: removes Recent-media entry `id` -- deletes its cache files,
    /// drops it from the index, and persists. Refuses to remove a pinned
    /// entry (the grid disables its ✕ button, but this is the authoritative
    /// guard). If the removed entry was the current media selection, clears
    /// that selection too so `apply`/`preview_rgba` don't keep referencing
    /// now-deleted files.
    fn remove_recent_entry(&mut self, id: &str) {
        // Defense-in-depth: the ✕ button already disables itself while
        // `self.loading` is in flight; guard here too so a stale worker's
        // completion can't be raced by a removal mid-load.
        if self.loading.is_some() {
            return;
        }
        let Some(e) = self.recent.get(id).cloned() else { return };
        if e.pinned {
            return;
        }
        let _ = std::fs::remove_file(&e.path);
        let _ = std::fs::remove_file(&e.thumb);
        self.recent.entries.retain(|x| x.id != id);
        let _ = self.recent.save();
        self.recent_thumbs.remove(id);
        if self.current_media_id.as_deref() == Some(id) {
            self.current_media_id = None;
            self.current_video_lcdv = None;
            self.current_video_src = None;
            self.video_import_settings = None;
            self.video_preview = None;
        }
    }

    /// Task 4b: "Save current as…" -- snapshots the whole current setup
    /// (mode, media selection, fit/zoom/pan/bitrate, brightness, dashboard)
    /// into a new `config::Profile` appended to `cfg.profiles`. If the
    /// snapshot references a cached media entry, pins it (`recent.set_pinned`)
    /// so it survives eviction/Clear Cache even after it ages out of the
    /// newest-10-unpinned window.
    fn save_current_profile(&mut self) {
        let name = self.new_profile_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let profile = config::Profile {
            name,
            mode: self.cfg.mode.clone(),
            media_id: self.current_media_id.clone(),
            fit: self.cfg.media.fit.clone(),
            zoom: self.cfg.media.zoom,
            pan: self.cfg.media.pan,
            bitrate_kbps: self.cfg.media.bitrate_kbps,
            brightness: self.cfg.brightness,
            dashboard: self.dashboard.clone(),
        };
        if let Some(id) = &profile.media_id {
            self.recent.set_pinned(id, true);
            let _ = self.recent.save();
            self.refresh_recent();
        }
        self.cfg.profiles.push(profile);
        match config::save(&self.cfg_path, &self.cfg) {
            Ok(_) => {
                self.new_profile_name.clear();
                self.set_status("Profile saved", 3);
            }
            Err(e) => self.set_status(format!("Save failed: {e}"), 6),
        }
    }

    /// Task 4b: applies `cfg.profiles[i]`'s whole snapshot -- mode, media
    /// selection, fit/zoom/pan/bitrate, brightness, dashboard -- then sends it
    /// to the device via the existing `apply()` path, identical to manually
    /// dialing in the same settings and clicking "Apply mode". `cfg.media.path`
    /// is set synchronously (even for an image/GIF, whose preview reload is
    /// asynchronous via `select_cache_entry`) so `apply()` -- which only reads
    /// `cfg.media.path`/`current_video_lcdv`, not the GUI preview state -- sends
    /// the right media to the device immediately regardless of preview timing.
    fn load_profile(&mut self, i: usize) {
        // Defense-in-depth: the "Load" button already disables itself while
        // `self.loading` is in flight; guard here too so a stale worker's
        // completion can't clobber a profile load that just ran (or vice versa).
        if self.loading.is_some() {
            return;
        }
        let Some(p) = self.cfg.profiles.get(i).cloned() else { return };
        self.cfg.mode = p.mode.clone();
        self.cfg.media.fit = p.fit.clone();
        self.cfg.media.zoom = p.zoom;
        self.cfg.media.pan = p.pan;
        // Defense-in-depth clamp: `config::load` already clamps every profile on
        // startup, but clamp again here in case an in-memory profile was ever
        // constructed/mutated without going through that path -- an unclamped
        // `bitrate_kbps` can overflow `u32` downstream in the encoder, and an
        // unclamped `brightness` is out of the device's valid percent range.
        self.cfg.media.bitrate_kbps = p.bitrate_kbps.clamp(300, 8000);
        self.cfg.brightness = p.brightness.clamp(1, 100);
        self.dashboard = p.dashboard.clone();

        match &p.media_id {
            Some(id) => match self.recent.get(id).cloned() {
                Some(entry) => {
                    self.select_cache_entry(&entry);
                    self.cfg.media.path = Some(entry.path.clone());
                }
                None => {
                    self.set_status("Profile's media is missing from cache", 6);
                    self.current_media_id = None;
                    self.current_video_lcdv = None;
                    self.current_video_src = None;
                    self.video_import_settings = None;
                    self.video_preview = None;
                    self.media_obj = None;
                }
            },
            None => {
                // Dashboard profile: clear any media selection.
                self.current_media_id = None;
                self.current_video_lcdv = None;
                self.current_video_src = None;
                self.video_import_settings = None;
                self.video_preview = None;
                self.media_obj = None;
                self.gif_composed = None;
                self.gif_compose_key = None;
                self.cfg.media.path = None;
            }
        }

        self.apply();
        // `apply()` deliberately doesn't send brightness (it's normally applied live
        // via the slider), but the pipeline only otherwise reads brightness at
        // startup / via an explicit `SetBrightness`, so a loaded profile's brightness
        // would silently not take effect on the device without this.
        let _ = self.tx.send(Command::SetBrightness(self.cfg.brightness));
        self.set_status("Profile loaded", 3);
    }

    /// Task 4b: removes `cfg.profiles[i]` and persists the config. Does not
    /// touch the pin state of any media it referenced -- another profile (or
    /// a future save) may still rely on it staying pinned, and safely
    /// unpinning would require checking every other profile too; left as a
    /// nice-to-have per the design spec.
    fn delete_profile(&mut self, i: usize) {
        if i < self.cfg.profiles.len() {
            self.cfg.profiles.remove(i);
            match config::save(&self.cfg_path, &self.cfg) {
                Ok(_) => self.set_status("Profile deleted", 3),
                Err(e) => self.set_status(format!("Save failed: {e}"), 6),
            }
        }
    }

    /// Sets a transient status message that renders in the bottom label (see `update`)
    /// for `secs` seconds and then disappears, rather than sitting there permanently.
    /// Use short durations for confirmations (~3s) and longer ones for errors (~6s) so
    /// the user has time to read them.
    fn set_status(&mut self, msg: impl Into<String>, secs: u64) {
        self.status = msg.into();
        self.status_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
    }

    /// Picks the frame index whose cumulative delay window contains the current point
    /// in the loop, based on wall-clock elapsed time since `epoch` -- shared by GIF
    /// playback (`preview_rgba`'s `gif_composed` path) and the live pan-drag fast path
    /// (which selects a frame from the source `MediaFrame`s directly), so both agree on
    /// which frame is "current" at any instant. `delays` must be non-empty (callers
    /// check this first); returns index 0 for a degenerate single-frame case.
    fn select_gif_frame_idx(delays: &[u32], epoch: std::time::Instant) -> usize {
        let elapsed_ms = epoch.elapsed().as_millis() as u32;
        let total: u32 = delays.iter().map(|d| (*d).max(40)).sum();
        let mut t = elapsed_ms % total.max(1);
        for (i, d) in delays.iter().enumerate() {
            let dd = (*d).max(40);
            if t < dd {
                return i;
            }
            t -= dd;
        }
        delays.len() - 1
    }

    /// Returns the current 320x320 RGBA preview frame by reusing the exact same
    /// render/compose pipeline the device output uses (WYSIWYG): dashboard via
    /// `render::Renderer::dashboard`, or media via `media::placement` +
    /// `render::compose_media`. Falls back to the dashboard if mode is "media" but
    /// nothing is loaded.
    /// Returns the preview RGBA frame plus whether it's currently an animated GIF
    /// being shown in media mode (used by `update` to pick a faster repaint rate).
    ///
    /// `interacting` is true while the user is actively dragging the preview (pan-drag
    /// in progress this frame -- see `update`). Bug fix (Task B-1): dragging a GIF used
    /// to feel dead because every dragged frame changed `pan`, invalidating
    /// `gif_compose_key` and triggering a full `render::compose_frames` rebuild (all N
    /// source frames re-cropped/resized) on every single mouse-move event -- fine for a
    /// handful of frames, a visible grind for a big GIF. While `interacting` is true we
    /// skip that rebuild entirely and instead compose *only* the one frame currently
    /// selected for playback directly from the source `MediaFrame`s, exactly as
    /// responsive as the static-image path. `gif_composed`/`gif_compose_key` are left
    /// untouched (stale) so the very next non-interacting call (the frame after the
    /// drag/button release) does the full rebuild once and smooth cached playback
    /// resumes.
    fn preview_rgba(&mut self, interacting: bool) -> (Vec<u8>, bool) {
        let want_media = self.cfg.mode.eq_ignore_ascii_case("media");

        // Task 4a: leaving Media mode stops cached-video preview playback (drops the
        // decoder) rather than leaving it decoding in the background; it's reloaded
        // from frame 0 below if/when the user switches back into Media mode with a
        // cached video still selected.
        if !want_media && self.video_preview.is_some() {
            self.video_preview = None;
        }

        if want_media {
            let fit = crate::media::Fit::from_str(&self.cfg.media.fit);
            let zoom = self.cfg.media.zoom;
            let pan = self.cfg.media.pan;

            // Task 4a: cached-video preview. If the current media is a cached video
            // (selected via a completed import, or -- Task 4b -- picked from Recent)
            // and nothing's currently decoding it (fresh selection, or just switched
            // back into Media mode after the clear above), (re)load the decoder.
            if self.current_video_lcdv.is_some() && self.media_obj.is_none() && self.video_preview.is_none() {
                if let Some(lcdv) = self.current_video_lcdv.clone() {
                    self.load_video_preview(&lcdv);
                }
            }

            // Decode+return a cached-video preview frame, in-process (no ffmpeg):
            // sequentially decode `packets[idx]`, advancing/wrapping as needed, and
            // return the frame (or the last good one) so it never flashes blank.
            if let Some(vp) = &mut self.video_preview {
                if vp.packets.is_empty() {
                    self.video_preview = None;
                } else {
                    let n = vp.packets.len();
                    let now = std::time::Instant::now();
                    let interval = std::time::Duration::from_millis(vp.frame_ms.max(1) as u64);
                    // Wall-clock pacing: advance to the next decoded frame only once
                    // `interval` of real time has elapsed, with bounded catch-up if egui
                    // repainted slower than the frame rate. This decouples preview speed
                    // from repaint frequency -- moving the mouse (which floods repaints) no
                    // longer fast-forwards the clip.
                    if vp.last.is_none() {
                        if let Some(raw) = decode_next_video_frame(vp, n) {
                            vp.last = Some(raw);
                        }
                        vp.last_advance = Some(now);
                    } else {
                        let mut budget = 4u8;
                        while budget > 0
                            && vp
                                .last_advance
                                .map(|t| now.duration_since(t) >= interval)
                                .unwrap_or(true)
                        {
                            if let Some(raw) = decode_next_video_frame(vp, n) {
                                vp.last = Some(raw);
                            }
                            vp.last_advance =
                                Some(vp.last_advance.map(|t| t + interval).unwrap_or(now));
                            budget -= 1;
                        }
                        // Big stall (e.g. window hidden a while): resync instead of
                        // bursting through a backlog of frames all at once.
                        if let Some(t) = vp.last_advance {
                            if now.duration_since(t) >= interval * 4 {
                                vp.last_advance = Some(now);
                            }
                        }
                    }
                    if let Some(last) = vp.last.clone() {
                        return (last, true);
                    }
                    // Nothing decodable yet (e.g. only SPS/PPS so far) -- blank rather
                    // than falling through to the dashboard while a video is selected.
                    let blank_len = (crate::media::D * crate::media::D * 4) as usize;
                    return (vec![0u8; blank_len], true);
                }
            }

            // Phase (1): (re)build `gif_composed` if the loaded media is an animated GIF
            // and the (path, fit, zoom, pan) it was last composed for is stale -- e.g. a
            // new file was opened, or the fit/zoom/pan changed (including live pan-drag
            // below, which mutates `self.cfg.media.pan` every dragged frame) -- but only
            // when NOT `interacting` (see doc comment above): the full N-frame rebuild is
            // deferred until the drag ends. Computed into a local first since `frames`
            // borrows `self.media_obj` immutably here, and assigning into
            // `self.gif_composed` needs a mutable borrow of `self`.
            let key = self.cfg.media.path.clone().map(|p| (p, fit, zoom, pan));
            let mut recomposed: Option<Vec<(Vec<u8>, u32)>> = None;
            match &self.media_obj {
                Some(crate::media::Media::Animated(frames)) => {
                    if !interacting && self.gif_compose_key != key {
                        recomposed = Some(crate::render::compose_frames(frames, fit, zoom, pan));
                    }
                }
                _ => {
                    if self.gif_composed.is_some() {
                        self.gif_composed = None;
                    }
                    self.gif_compose_key = None;
                }
            }
            if recomposed.is_some() {
                self.gif_composed = recomposed;
                self.gif_compose_key = key;
            }

            // Phase (2): render from whatever's loaded. Static composes on demand (cheap,
            // one full-res frame); Animated plays back the pre-composed 320x320 frames --
            // no per-repaint compose, only a frame-select + clone -- unless `interacting`,
            // in which case it composes just the current frame from the source instead
            // (see doc comment above).
            match &self.media_obj {
                Some(crate::media::Media::Static(img)) => {
                    let (w, h) = img.dimensions();
                    let p = crate::media::placement(w, h, fit, zoom, pan);
                    return (crate::render::compose_media(img, &p, [8, 10, 18, 255]), false);
                }
                Some(crate::media::Media::Animated(frames)) => {
                    if interacting {
                        if !frames.is_empty() {
                            let delays: Vec<u32> = frames.iter().map(|f| f.delay_ms).collect();
                            let idx = Self::select_gif_frame_idx(&delays, self.gif_epoch);
                            let f = &frames[idx];
                            let (sw, sh) = f.img.dimensions();
                            let p = crate::media::placement(sw, sh, fit, zoom, pan);
                            return (crate::render::compose_media(&f.img, &p, [8, 10, 18, 255]), true);
                        }
                    } else if let Some(composed) = &self.gif_composed {
                        if !composed.is_empty() {
                            let delays: Vec<u32> = composed.iter().map(|(_, d)| *d).collect();
                            let idx = Self::select_gif_frame_idx(&delays, self.gif_epoch);
                            return (composed[idx].0.clone(), true);
                        }
                    }
                }
                None => {}
            }
        }
        // Dashboard (or media mode with nothing loaded). Render through the same
        // data-driven engine (`render_dashboard`) the device pipeline uses, against the
        // in-progress `self.dashboard` (not `self.cfg.dashboard`) so the preview is
        // WYSIWYG with whatever the Template/Theme/Colors pickers currently hold, even
        // before Apply. Push each widget's current metric value into `self.dash_hist`
        // first so Sparkline widgets have a live series to draw.
        // Throttle sensor reads to ~1 Hz: egui may repaint dozens of times/sec (e.g. on
        // every mouse move), but sysinfo/NVML measure usage over the interval since the
        // last read, so reading that often makes CPU%/GPU% jump wildly. Reuse the cached
        // snapshot unless it's missing or stale (>=1000ms old); the pipeline's own 1 Hz
        // read cadence in pipeline.rs is separate and unaffected.
        let need_read = match &self.last_snap {
            None => true,
            Some(_) => self.last_snap_at.elapsed() >= std::time::Duration::from_millis(1000),
        };
        if need_read {
            let snap = self.sensors.read();
            self.last_snap = Some(snap);
            self.last_snap_at = std::time::Instant::now();
        }
        let snap = self.last_snap.clone().expect("populated above");
        for w in &self.dashboard.widgets {
            if let Some(v) = snap.value_for(w.kind) {
                self.dash_hist.push(w.kind, v);
            }
        }
        (self.renderer.render_dashboard(&self.dashboard, &snap, &self.dash_hist), false)
    }

    /// Commits the Dashboard/Media mode (and, in Media mode, the loaded media plus its
    /// fit/zoom/pan) to the running pipeline and saves it to disk. Brightness is applied
    /// live (see the slider in `update`), so it is intentionally NOT sent here.
    fn apply(&mut self) {
        let mode = Mode::from_str(&self.cfg.mode);
        let _ = self.tx.send(Command::SetMode(mode));
        // Whether this Apply will kick off a media (re)load/pre-encode on the pipeline
        // thread (which flips `media_busy` true until it finishes) -- true only for
        // Media mode with something actually loaded. When it will, we deliberately do
        // NOT set a status below: `update`'s `media_busy` falling-edge check shows
        // "Applied ✓" once the pipeline has truly finished, instead of the moment the
        // button is clicked (which used to be misleading -- the label said "Applied"
        // while the spinner simultaneously said "Applying to cooler…").
        // Task 4a: if the current media is a cached video, Apply sends
        // `Command::LoadCachedVideo` (send-only playback of the already-encoded
        // `.lcdv` packets) instead of `Command::LoadMedia` -- there's no source file
        // for the pipeline to (re)decode, just the cache path. Image/GIF Apply is
        // unchanged. `LoadCachedVideo` flips `media_busy` on the pipeline side same as
        // `LoadMedia` (reading the `.lcdv` off disk), so it counts toward
        // `will_reload_media` too.
        let will_reload_media = mode == Mode::Media
            && (self.current_video_lcdv.is_some() || self.cfg.media.path.is_some());
        if mode == Mode::Media {
            if let Some(lcdv_path) = &self.current_video_lcdv {
                let _ = self.tx.send(Command::LoadCachedVideo { lcdv_path: lcdv_path.clone() });
            } else if let Some(path) = &self.cfg.media.path {
                let _ = self.tx.send(Command::LoadMedia {
                    path: path.clone(),
                    fit: self.cfg.media.fit.clone(),
                    zoom: self.cfg.media.zoom,
                    pan: self.cfg.media.pan,
                    bitrate_kbps: self.cfg.media.bitrate_kbps,
                });
            }
        }
        // Commit the working dashboard (Template/Theme/Colors picker edits) to both the
        // running pipeline and the persisted config, in every mode (not just Dashboard
        // mode) so a customization made while previewing isn't silently dropped if the
        // user Applies from Media mode.
        self.cfg.dashboard = self.dashboard.clone();
        let _ = self.tx.send(Command::SetDashboard(self.dashboard.clone()));
        match config::save(&self.cfg_path, &self.cfg) {
            Ok(_) => {
                if !will_reload_media {
                    self.set_status("Applied", 3);
                }
            }
            Err(e) => self.set_status(format!("Save failed: {e}"), 6),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // On first frame with no tray, show the window so it's not invisible/unquittable.
        if !self.has_tray && !self.shown_on_first_frame {
            self.shown_on_first_frame = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        // TODO Task 4a: video import + cached preview. Video is no longer a live,
        // ffmpeg-backed `Media` variant, so there's no process to drop/reopen on a
        // Dashboard/Media mode switch anymore -- image/GIF/dashboard preview state
        // is unaffected either way.
        //
        // Task 4b (fixed): `current_media_id` tracks "the loaded media" regardless
        // of mode, mirroring `current_video_lcdv`/`video_preview`, which already
        // survive a Dashboard/Media round-trip. It used to be cleared here on a
        // Media->Dashboard mode edge, but that meant "Save current as..." after
        // switching back to Media (without reselecting anything) recorded
        // `media_id: None` and the profile wrongly became dashboard-only even
        // though the same media was still loaded. It's only cleared where the
        // media is actually replaced/cleared: a new load/import,
        // `remove_recent_entry`, a Clear-Cache deletion of the current selection,
        // or `select_cache_entry` picking a different Recent entry.

        // Bugfix: tie the "Applied ✓" confirmation to the pipeline actually finishing a
        // Media-mode apply, not to the button click. `apply()` skips setting a status
        // when it kicks off a media (re)load/pre-encode (`media_busy` goes true); this
        // falling edge -- busy just went back to false -- is what shows the transient
        // confirmation once that work has truly completed.
        let busy = self.media_busy.load(std::sync::atomic::Ordering::Relaxed);
        if self.was_busy && !busy {
            self.set_status("Applied", 3);
        }
        self.was_busy = busy;

        // Tray menu events (Open, Quit) and tray-icon events (click, double-click) are
        // handled on a dedicated thread (`spawn_tray_thread`, spawned in `main`'s
        // `app_creator`) that drives `ctx` directly via `send_viewport_cmd` +
        // `request_repaint`. eframe stops calling `update()` while the window is
        // hidden, so polling here (as this used to do) never runs when it matters
        // most -- right after Open is clicked from the tray. The background thread
        // has no such restriction, which is what actually fixes Open/double-click.

        // Handle close request: behavior depends on whether tray is available.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.has_tray {
                // (X) hides to tray instead of quitting. CancelClose still goes through
                // egui (update() is active on this path, so the command queue is being
                // flushed), but hiding goes straight through Win32 (crate::winwnd) for
                // consistency with the tray-thread show/hide path -- see winwnd.rs for
                // why Visible(false)/Visible(true) from a background thread can't be
                // relied on to take effect on a hidden window.
                log::info!("update: close_requested with tray; sending CancelClose + Win32 hide");
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                crate::winwnd::hide_window();
            } else {
                // No tray: (X) quits the app.
                log::info!("update: close_requested, no tray; sending Close, stop set");
                self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Phase 3b-i Task B: poll the background "Open image/GIF..." worker (if one is
        // in flight), applying its finished result (media + any pre-built GIF preview
        // cache) into `self.media_obj`/`gif_composed` here on the UI thread -- the
        // worker thread itself only ever touches the shared `LoadProgress`, never
        // `self` directly. While still running, stash `(done, total, stage)` locally so
        // the SidePanel below can draw a progress bar, and keep repainting so the bar
        // (and the eventual completion) are seen promptly.
        let mut load_progress_ui: Option<(usize, usize, String)> = None;
        if let Some(progress) = self.loading.clone() {
            let (done, total, stage, result, video_result) = {
                let mut p = progress.lock().expect("load progress mutex poisoned");
                (p.done, p.total, p.stage.clone(), p.result.take(), p.video_result.take())
            };
            match (result, video_result) {
                (Some(Ok((media, composed))), _) => {
                    // A new image/GIF replaces any cached-video selection: stop its
                    // preview and forget its cache entry so `apply`/`preview_rgba`
                    // treat this image/GIF as the current media from here on.
                    self.video_preview = None;
                    self.current_video_lcdv = None;
                    self.current_video_src = None;
                    self.video_import_settings = None;

                    let source_path = self.loading_path.take();
                    // Task 4b: `Some` when this load was triggered by re-selecting
                    // an ALREADY-cached Recent entry (or loading a Profile that
                    // references one) rather than a fresh "Add Media..." pick --
                    // in that case the media already has a Recent-media entry, so
                    // skip re-adding a duplicate one, and preserve the caller's
                    // zoom/pan (a Profile may have just set them) instead of
                    // resetting to defaults.
                    let existing_id = self.loading_existing_id.take();
                    let is_gif = matches!(&media, media::Media::Animated(_));
                    self.current_media_id = existing_id.clone();
                    // Task 4a: also populate the Recent-media index for image/GIF
                    // loads (thumbnail from the first composed GIF frame, or a fresh
                    // contain-fit compose for a static image) so Task 4b's grid has
                    // data. Best-effort: a thumbnail/index-write failure must not
                    // block the load itself from taking effect.
                    if existing_id.is_none() {
                        let thumb_rgba: Option<Vec<u8>> = match (&media, &composed) {
                            (_, Some(c)) => c.first().map(|(rgba, _)| rgba.clone()),
                            (media::Media::Static(img), None) => {
                                let (w, h) = img.dimensions();
                                let p = crate::media::placement(w, h, crate::media::Fit::Contain, 1.0, [0.0, 0.0]);
                                Some(crate::render::compose_media(img, &p, [8, 10, 18, 255]))
                            }
                            _ => None,
                        };
                        if let (Some(src), Some(rgba)) = (&source_path, thumb_rgba) {
                            if let Some(thumb_img) = image::RgbaImage::from_raw(320, 320, rgba) {
                                let id = cache::new_id(src);
                                let thumb_path = cache::cache_dir().join(format!("{id}.png"));
                                if cache::write_thumb(&thumb_path, &thumb_img).is_ok() {
                                    let name = std::path::Path::new(src)
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "media".to_string());
                                    let created = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs())
                                        .unwrap_or(0);
                                    let mut idx = cache::load_index();
                                    idx.add(cache::CacheEntry {
                                        id: id.clone(),
                                        kind: if is_gif { "gif" } else { "image" }.to_string(),
                                        name,
                                        path: src.clone(),
                                        thumb: thumb_path.to_string_lossy().to_string(),
                                        fps: 0.0,
                                        created,
                                        pinned: false,
                                    });
                                    idx.evict_unpinned(10);
                                    let _ = idx.save();
                                    self.refresh_recent();
                                    self.current_media_id = Some(id);
                                }
                            }
                        }
                    }

                    self.cfg.media.path = source_path;
                    if existing_id.is_none() {
                        self.cfg.media.zoom = 1.0;
                        self.cfg.media.pan = [0.0, 0.0];
                    }
                    self.gif_epoch = std::time::Instant::now();
                    match composed {
                        Some(c) => {
                            let fit = crate::media::Fit::from_str(&self.cfg.media.fit);
                            self.gif_compose_key = self
                                .cfg
                                .media
                                .path
                                .clone()
                                .map(|p| (p, fit, self.cfg.media.zoom, self.cfg.media.pan));
                            self.gif_composed = Some(c);
                        }
                        None => {
                            self.gif_composed = None;
                            self.gif_compose_key = None;
                        }
                    }
                    self.media_obj = Some(media);
                    self.set_status("Loaded (click Apply to show on cooler)", 3);
                    self.loading = None;
                }
                (Some(Err(e)), _) => {
                    self.set_status(format!("Load failed: {e}"), 6);
                    self.loading = None;
                    self.loading_path = None;
                    self.loading_existing_id = None;
                }
                (None, Some(Ok((cached, thumb)))) => {
                    // Task 4a: video import finished -- write the `.lcdv` + thumbnail,
                    // add a Recent-media entry (evicting down to 10 unpinned), and
                    // select it as the current media (preview reloads on the next
                    // `preview_rgba` call since `video_preview` is cleared here).
                    let src_path = self.loading_path.take().unwrap_or_default();
                    let name = std::path::Path::new(&src_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "video".to_string());
                    let id = cache::new_id(&src_path);
                    let lcdv_path = cache::cache_dir().join(format!("{id}.lcdv"));
                    let thumb_path = cache::cache_dir().join(format!("{id}.png"));
                    match cache::write_lcdv(&lcdv_path, &cached) {
                        Ok(()) => {
                            let _ = cache::write_thumb(&thumb_path, &thumb);
                            let created = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let mut idx = cache::load_index();
                            idx.add(cache::CacheEntry {
                                id: id.clone(),
                                kind: "video".to_string(),
                                name,
                                path: lcdv_path.to_string_lossy().to_string(),
                                thumb: thumb_path.to_string_lossy().to_string(),
                                fps: cached.fps,
                                created,
                                pinned: false,
                            });
                            idx.evict_unpinned(10);
                            let _ = idx.save();
                            self.refresh_recent();

                            let lcdv_str = lcdv_path.to_string_lossy().to_string();
                            self.media_obj = None;
                            self.gif_composed = None;
                            self.gif_compose_key = None;
                            self.video_preview = None;
                            self.current_video_lcdv = Some(lcdv_str.clone());
                            self.current_video_src = Some(src_path);
                            self.current_media_id = Some(id);
                            self.video_import_settings = Some((
                                self.cfg.media.fit.clone(),
                                self.cfg.media.zoom,
                                self.cfg.media.pan,
                                self.cfg.media.bitrate_kbps,
                            ));
                            self.load_video_preview(&lcdv_str);
                            self.set_status("Imported (click Apply to show on cooler)", 3);
                        }
                        Err(e) => {
                            self.set_status(format!("Video import failed: {e}"), 6);
                        }
                    }
                    self.loading = None;
                }
                (None, Some(Err(e))) => {
                    self.set_status(format!("Video import failed: {e}"), 6);
                    self.loading = None;
                    self.loading_path = None;
                }
                (None, None) => {
                    load_progress_ui = Some((done, total, stage));
                    ctx.request_repaint();
                }
            }
        }

        // Phase 2c: render the current dashboard/media frame through the shared
        // render pipeline (WYSIWYG with the device output) and upload it as an egui
        // texture. Repainting on a timer (rather than only on input) keeps the clock
        // and sensor readouts, and any animated media's first frame, visibly live.
        // `interacting` (pan-drag fast path, Task B-1) reflects whether the primary
        // mouse button is down *this frame* -- computed before `preview_rgba` per its
        // doc comment, since it changes which GIF compose path that call takes.
        let interacting = ctx.input(|i| i.pointer.primary_down());
        let (rgba, is_animated_gif) = self.preview_rgba(interacting);
        let color = egui::ColorImage::from_rgba_unmultiplied([320, 320], &rgba);
        match &mut self.preview_tex {
            Some(t) => t.set(color, egui::TextureOptions::LINEAR),
            None => self.preview_tex = Some(ctx.load_texture("preview", color, egui::TextureOptions::LINEAR)),
        }
        // Animated GIF previews repaint quickly so playback looks smooth; dashboard
        // and static-image previews stay on the slower 250ms tick (clock/sensors only
        // need to look "live", not animate).
        if is_animated_gif {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::SidePanel::left("preview").resizable(false).exact_width(340.0).show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("Preview");
            // Task B-2: background media load in progress -- draw a progress bar +
            // stage label over the (still-previous) preview image below. The image
            // itself keeps showing whatever was loaded before (or the dashboard, if
            // nothing was), since `self.media_obj` isn't touched until the worker's
            // result lands (see the poll above).
            if let Some((done, total, stage)) = &load_progress_ui {
                ui.add(
                    egui::ProgressBar::new(*done as f32 / (*total).max(1) as f32)
                        .show_percentage(),
                );
                ui.label(stage.clone());
                ui.add_space(4.0);
            }
            // Task 4a: a small label identifying a cached-video preview (distinct from
            // a plain image/GIF preview) and its stats -- also the one place `fps`
            // (stored on `VideoPreview` for exactly this) actually gets read.
            if load_progress_ui.is_none() {
                if let Some(vp) = &self.video_preview {
                    ui.weak(format!("Cached video · {:.0} fps · {} frames", vp.fps, vp.packets.len()));
                    ui.add_space(4.0);
                }
            }
            if let Some(tex) = &self.preview_tex {
                // `click_and_drag()` is a superset of `drag()` -- Media mode's pan
                // behavior below (which only reads `dragged()`/`drag_delta()`) is
                // unaffected by also sensing clicks; the extra `clicked()` signal is
                // only consumed by the Dashboard-mode editor branch further down.
                let mut resp = ui.add(
                    egui::Image::new((tex.id(), egui::vec2(320.0, 320.0)))
                        .sense(egui::Sense::click_and_drag()),
                );
                let img_rect = resp.rect;

                // Cursor and tooltip only show when pannable (media mode with media loaded).
                let is_pannable = self.cfg.mode.eq_ignore_ascii_case("media") && self.media_obj.is_some();

                if resp.dragged() && is_pannable {
                    let d = resp.drag_delta();
                    // 320px preview -> pan range 2.0 (-1..1); dragging right should move
                    // the visible window right, i.e. pan the source content left.
                    self.cfg.media.pan[0] = (self.cfg.media.pan[0] - d.x / 160.0).clamp(-1.0, 1.0);
                    self.cfg.media.pan[1] = (self.cfg.media.pan[1] - d.y / 160.0).clamp(-1.0, 1.0);
                    if self.cfg.media.fit != "manual" {
                        self.cfg.media.fit = "manual".to_string();
                    }
                }

                if is_pannable {
                    // Set cursor: grabbing while dragging, grab otherwise on hover.
                    if resp.dragged() {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    } else if resp.hovered() {
                        ctx.set_cursor_icon(egui::CursorIcon::Grab);
                    }
                    // Add tooltip on hover. `on_hover_text` takes `self` by value and
                    // returns the (possibly rewrapped) `Response`, so reassign it back
                    // rather than discarding it -- the Dashboard-mode editor logic below
                    // still needs `resp`.
                    resp = resp.on_hover_text("Drag to pan · use Zoom to zoom in");
                }

                // Phase 3a-ii Task 3: direct manipulation of widgets on the preview,
                // Dashboard mode only (Media mode keeps only the pan behavior above).
                // The preview is drawn at a fixed 320x320 points, 1:1 with `dashboard::CANVAS`
                // device pixels, so pointer-to-canvas mapping is a plain offset subtraction,
                // no scaling.
                if self.cfg.mode.eq_ignore_ascii_case("dashboard") {
                    // Plain click (no drag past the threshold): (re)select whatever's under
                    // the pointer, or deselect on empty space.
                    if resp.clicked() {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            let cx = pos.x - img_rect.min.x;
                            let cy = pos.y - img_rect.min.y;
                            if cx >= 0.0 && cx < dashboard::CANVAS as f32 && cy >= 0.0 && cy < dashboard::CANVAS as f32 {
                                self.selected = editor::hit_widget(&self.dashboard.widgets, cx as u32, cy as u32);
                            } else {
                                self.selected = None;
                            }
                        }
                    }

                    // Drag start: figure out which part of the *selected* widget (if any)
                    // the press landed on -- body (move) or a corner (resize). If the press
                    // wasn't on the selected widget/its handles, `drag_grab` stays `None`
                    // and the subsequent `dragged()` deltas below are simply ignored.
                    if resp.drag_started() {
                        self.drag_grab = None;
                        if let Some(sel) = self.selected {
                            if let Some(w) = self.dashboard.widgets.get(sel) {
                                if let Some(pos) = resp.interact_pointer_pos() {
                                    let cx = pos.x - img_rect.min.x;
                                    let cy = pos.y - img_rect.min.y;
                                    self.drag_grab = editor::hit_handle(&w.rect, cx, cy);
                                }
                            }
                        }
                    }

                    // Dragging: apply the accumulated per-frame delta (canvas px == points,
                    // 1:1) to the selected widget's rect via the `editor` geometry helpers,
                    // which handle min-size/canvas clamping.
                    if resp.dragged() {
                        if let (Some(sel), Some(grab)) = (self.selected, self.drag_grab) {
                            if let Some(w) = self.dashboard.widgets.get_mut(sel) {
                                let d = resp.drag_delta();
                                let dx = d.x.round() as i32;
                                let dy = d.y.round() as i32;
                                if dx != 0 || dy != 0 {
                                    w.rect = match grab {
                                        editor::Grab::Body => editor::apply_move(w.rect, dx, dy),
                                        _ => editor::apply_resize(w.rect, grab, dx, dy),
                                    };
                                }
                            }
                        }
                    }

                    if resp.drag_stopped() {
                        self.drag_grab = None;
                    }

                    // Overlay: selection outline + 4 corner-handle squares for the
                    // selected widget, drawn in screen space (image top-left + canvas pos).
                    if let Some(sel) = self.selected {
                        if let Some(w) = self.dashboard.widgets.get(sel) {
                            let r = w.rect;
                            let screen_min = img_rect.min + egui::vec2(r.x as f32, r.y as f32);
                            let screen_max =
                                img_rect.min + egui::vec2((r.x + r.w) as f32, (r.y + r.h) as f32);
                            let screen_rect = egui::Rect::from_min_max(screen_min, screen_max);
                            let painter = ui.painter();
                            painter.rect_stroke(
                                screen_rect,
                                0.0,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(0, 224, 255)),
                            );
                            for corner in [
                                screen_rect.left_top(),
                                screen_rect.right_top(),
                                screen_rect.left_bottom(),
                                screen_rect.right_bottom(),
                            ] {
                                let handle_rect = egui::Rect::from_center_size(
                                    corner,
                                    egui::vec2(editor::HANDLE, editor::HANDLE),
                                );
                                painter.rect_filled(handle_rect, 0.0, egui::Color32::WHITE);
                                painter.rect_stroke(
                                    handle_rect,
                                    0.0,
                                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0, 224, 255)),
                                );
                            }
                        } else {
                            self.selected = None;
                        }
                    }
                }
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Astroshel Lean Display");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.cfg.mode, "dashboard".to_string(), "Dashboard");
                ui.selectable_value(&mut self.cfg.mode, "media".to_string(), "Media");
            });
            ui.separator();

            // Media controls: file picker, fit presets, zoom. Editing any of these
            // updates `self.cfg.media`, which `preview_rgba` reads every frame -- so the
            // live preview reflects changes immediately. Nothing is sent to the device
            // here; that only happens via the "Apply mode" button below (`self.apply`).
            if self.cfg.mode.eq_ignore_ascii_case("media") {
                // Disabled while a load is already in flight -- picking a second file
                // before the first finishes would race two workers writing the same
                // `self.media_obj`/`gif_composed` on completion.
                let open_enabled = self.loading.is_none();
                if ui
                    .add_enabled(open_enabled, egui::Button::new("Add Media…"))
                    .clicked()
                {
                    const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "bmp", "webp"];
                    const VIDEO_EXTS: [&str; 12] =
                        ["mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "ts", "mpg", "mpeg", "m2ts"];
                    let mut all_exts: Vec<&str> = IMAGE_EXTS.to_vec();
                    all_exts.extend_from_slice(&VIDEO_EXTS);
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Media", &all_exts)
                        .add_filter("Image/GIF", &IMAGE_EXTS)
                        .add_filter("Video", &VIDEO_EXTS)
                        .pick_file()
                    {
                        // Task 4a: a video-extension pick runs the background
                        // import-to-cache worker (progress bar via `LoadProgress`)
                        // instead of the image/GIF loader below -- `media::load` would
                        // bail on a video path anyway (see `media.rs`).
                        if media::is_video_path(&path) {
                            self.spawn_video_import(path);
                        } else {
                            self.loading_existing_id = None;
                            self.spawn_media_load(path);
                        }
                    }
                }

                ui.horizontal(|ui| {
                    ui.label("Fit:");
                    for (label, val) in [("Contain", "contain"), ("Cover", "cover"), ("Stretch", "stretch")] {
                        if ui.selectable_label(self.cfg.media.fit == val, label).clicked() {
                            self.cfg.media.fit = val.to_string();
                        }
                    }
                });

                let zoom_resp =
                    ui.add(egui::Slider::new(&mut self.cfg.media.zoom, 1.0..=4.0).text("Zoom"));
                if zoom_resp.changed() {
                    self.cfg.media.fit = "manual".to_string();
                }

                // Bitrate slider: adjusts the streaming H.264 encoder's target bitrate
                // (see `encode::new_stream_encoder`/`config::MediaCfg::bitrate_kbps`).
                // Live-edits `self.cfg.media.bitrate_kbps` only; the pipeline doesn't see
                // the new value (and the GIF isn't re-encoded) until Apply sends it via
                // `Command::LoadMedia` (see `apply`). Gated to pop at most once per app
                // run via `bitrate_warned_session`, regardless of how many times the
                // slider is dragged, so it doesn't re-open on every drag tick.
                let bitrate_resp = ui.add(
                    egui::Slider::new(&mut self.cfg.media.bitrate_kbps, 500..=6000).text("Bitrate (advanced)"),
                );
                if bitrate_resp.changed() && self.cfg.show_bitrate_warning && !self.bitrate_warned_session {
                    self.bitrate_modal = true;
                    self.bitrate_warned_session = true;
                }
                ui.weak(
                    "Default 1500. Higher = sharper but may stutter or overrun the display; \
                     lower = smoother but softer. Applies on Apply.",
                );

                // Task 4a: Re-import affordance. Only relevant when the current media is
                // a cached video (`current_video_lcdv`/`current_video_src` both `Some`);
                // enabled only once fit/zoom/pan/bitrate have actually changed since the
                // last (re-)import (`video_import_settings`), re-running the same
                // background import worker (`spawn_video_import`) with the new settings,
                // which overwrites this selection's cache entry with a fresh id.
                if let (Some(src), Some(_)) = (self.current_video_src.clone(), &self.current_video_lcdv) {
                    let cur_settings = (
                        self.cfg.media.fit.clone(),
                        self.cfg.media.zoom,
                        self.cfg.media.pan,
                        self.cfg.media.bitrate_kbps,
                    );
                    let dirty = self.video_import_settings.as_ref() != Some(&cur_settings);
                    ui.horizontal(|ui| {
                        let can_reimport = dirty && self.loading.is_none();
                        if ui.add_enabled(can_reimport, egui::Button::new("Re-import")).clicked() {
                            self.spawn_video_import(PathBuf::from(src));
                        }
                        if dirty {
                            ui.weak("Fit/zoom/bitrate changed since import");
                        }
                    });
                }

                ui.separator();

                // Task 4b: Recent-media grid -- up to 10 entries, newest first,
                // as ~56px thumbnails. Clicking a thumbnail selects that media
                // (mirrors what a completed import/load does, see
                // `select_cache_entry`); the ✕ removes an entry (disabled for a
                // pinned one, e.g. one a saved Profile references).
                //
                // Both are disabled while a background load/import is in flight
                // (`self.loading.is_some()`), same as "Add Media…"/"Re-import" --
                // otherwise a stale worker's completion (which lands in `update`
                // after this frame) can clobber whatever the user just selected.
                ui.heading("Recent");
                {
                    let can_select = self.loading.is_none();
                    let mut entries: Vec<cache::CacheEntry> = self.recent.entries.clone();
                    entries.sort_by_key(|e| std::cmp::Reverse(e.created));
                    entries.truncate(10);
                    let mut to_select: Option<cache::CacheEntry> = None;
                    let mut to_remove: Option<String> = None;
                    egui::ScrollArea::vertical()
                        .max_height(110.0)
                        .id_salt("recent_scroll")
                        .show(ui, |ui| {
                            if entries.is_empty() {
                                ui.weak("No recent media yet.");
                            }
                            ui.horizontal_wrapped(|ui| {
                                for e in &entries {
                                    let src_missing = !std::path::Path::new(&e.path).exists();
                                    let thumb_missing = !std::path::Path::new(&e.thumb).exists();
                                    let missing = src_missing || thumb_missing;
                                    ui.vertical(|ui| {
                                        ui.set_width(56.0);
                                        if missing {
                                            let (rect, _resp) = ui.allocate_exact_size(
                                                egui::vec2(56.0, 56.0),
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                4.0,
                                                egui::Color32::from_gray(60),
                                            );
                                        } else {
                                            if !self.recent_thumbs.contains_key(&e.id) {
                                                if let Ok(img) = image::open(&e.thumb) {
                                                    let rgba = img.to_rgba8();
                                                    let (w, h) = rgba.dimensions();
                                                    let color = egui::ColorImage::from_rgba_unmultiplied(
                                                        [w as usize, h as usize],
                                                        rgba.as_raw(),
                                                    );
                                                    let tex = ui.ctx().load_texture(
                                                        format!("recent_{}", e.id),
                                                        color,
                                                        egui::TextureOptions::LINEAR,
                                                    );
                                                    self.recent_thumbs.insert(e.id.clone(), tex);
                                                }
                                            }
                                            if let Some(tex) = self.recent_thumbs.get(&e.id) {
                                                let resp = ui
                                                    .add_enabled(
                                                        can_select,
                                                        egui::ImageButton::new(egui::Image::new((
                                                            tex.id(),
                                                            egui::vec2(56.0, 56.0),
                                                        ))),
                                                    )
                                                    .on_hover_text(&e.name);
                                                if resp.clicked() {
                                                    to_select = Some(e.clone());
                                                }
                                            } else {
                                                let (rect, _resp) = ui.allocate_exact_size(
                                                    egui::vec2(56.0, 56.0),
                                                    egui::Sense::hover(),
                                                );
                                                ui.painter().rect_filled(
                                                    rect,
                                                    4.0,
                                                    egui::Color32::from_gray(60),
                                                );
                                            }
                                        }
                                        if ui
                                            .add_enabled(
                                                !e.pinned && can_select,
                                                egui::Button::new("✕").small(),
                                            )
                                            .clicked()
                                        {
                                            to_remove = Some(e.id.clone());
                                        }
                                    });
                                }
                            });
                        });
                    if let Some(e) = to_select {
                        self.select_cache_entry(&e);
                    }
                    if let Some(id) = to_remove {
                        self.remove_recent_entry(&id);
                    }
                }

                ui.separator();
            }

            // Dashboard controls: Template/Theme pickers + theme color editors. Editing
            // any of these updates the in-progress `self.dashboard` (NOT
            // `self.cfg.dashboard`), which `preview_rgba` renders every frame -- so the
            // live preview reflects changes immediately, WYSIWYG with the device output
            // after Apply commits `self.dashboard` into `self.cfg.dashboard` and sends
            // `Command::SetDashboard`.
            if self.cfg.mode.eq_ignore_ascii_case("dashboard") {
                ui.horizontal(|ui| {
                    ui.label("Template:");
                    let current_name = dashboard::templates()
                        .into_iter()
                        .find(|(_, d)| d.widgets == self.dashboard.widgets)
                        .map(|(n, _)| n)
                        .unwrap_or("Custom");
                    egui::ComboBox::from_id_salt("dash_template")
                        .selected_text(current_name)
                        .show_ui(ui, |ui| {
                            for (name, tmpl) in dashboard::templates() {
                                if ui.selectable_label(current_name == name, name).clicked() {
                                    self.dashboard.widgets = tmpl.widgets;
                                    // The old selection almost certainly no longer refers to
                                    // the same widget (different template, different widget
                                    // count/order), so drop it rather than risk it silently
                                    // pointing at an unrelated widget after the swap.
                                    self.selected = None;
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Theme:");
                    let current_name = dashboard::Theme::presets()
                        .into_iter()
                        .find(|(_, t)| *t == self.dashboard.theme)
                        .map(|(n, _)| n)
                        .unwrap_or("Custom");
                    egui::ComboBox::from_id_salt("dash_theme")
                        .selected_text(current_name)
                        .show_ui(ui, |ui| {
                            for (name, theme) in dashboard::Theme::presets() {
                                if ui.selectable_label(current_name == name, name).clicked() {
                                    self.dashboard.theme = theme;
                                }
                            }
                        });
                });

                if ui.button("Colors…").clicked() {
                    self.show_colors = true;
                }

                ui.separator();

                // Widgets editor: list + add/select/delete + per-widget settings. All
                // edits below mutate `self.dashboard.widgets` (never `self.cfg.dashboard`
                // directly), so the WYSIWYG preview (`preview_rgba`, above) reflects them
                // live, same as the Template/Theme/Colors pickers; Apply commits them.
                ui.heading("Widgets");

                egui::ScrollArea::vertical().max_height(120.0).id_salt("widget_list_scroll").show(ui, |ui| {
                    for i in 0..self.dashboard.widgets.len() {
                        ui.push_id(i, |ui| {
                            let w = &self.dashboard.widgets[i];
                            let row_label = format!("{} · {}", w.kind.as_str(), w.viz.as_str());
                            let is_selected = self.selected == Some(i);
                            if ui.selectable_label(is_selected, row_label).clicked() {
                                self.selected = Some(i);
                            }
                        });
                    }
                });

                ui.horizontal(|ui| {
                    // Add widget: picking a kind pushes a new `Widget::new(kind)` at a
                    // default centered rect and selects it. `selected_text` is a static
                    // prompt (not a persisted "current kind") since this combo is a
                    // one-shot action, not a bound value.
                    let mut add_kind: Option<dashboard::WidgetKind> = None;
                    egui::ComboBox::from_id_salt("add_widget_kind")
                        .selected_text("Add widget…")
                        .show_ui(ui, |ui| {
                            for kind in ALL_WIDGET_KINDS {
                                if ui.selectable_label(false, kind.as_str()).clicked() {
                                    add_kind = Some(kind);
                                }
                            }
                        });
                    if let Some(kind) = add_kind {
                        let mut w = dashboard::Widget::new(kind);
                        w.rect = dashboard::Rect { x: 100, y: 115, w: 120, h: 90 };
                        self.dashboard.widgets.push(w);
                        self.selected = Some(self.dashboard.widgets.len() - 1);
                    }

                    let can_delete = self.selected.is_some();
                    if ui.add_enabled(can_delete, egui::Button::new("🗑 Delete")).clicked() {
                        if let Some(i) = self.selected {
                            self.dashboard.widgets.remove(i);
                            let len = self.dashboard.widgets.len();
                            self.selected = if len == 0 {
                                None
                            } else {
                                Some(i.min(len - 1))
                            };
                        }
                    }
                });

                // Settings for the selected widget, if any. Guarded against
                // out-of-range (shouldn't happen given the add/delete/template-switch
                // handling above, but this keeps indexing panic-free if it ever does).
                if let Some(i) = self.selected {
                    if i < self.dashboard.widgets.len() {
                        ui.separator();
                        ui.label("Widget settings");

                        let kind = self.dashboard.widgets[i].kind;
                        let metric = is_metric_kind(kind);

                        if metric || kind == dashboard::WidgetKind::Clock {
                            ui.horizontal(|ui| {
                                ui.label("Viz:");
                                let current = self.dashboard.widgets[i].viz;
                                egui::ComboBox::from_id_salt("widget_viz")
                                    .selected_text(current.as_str())
                                    .show_ui(ui, |ui| {
                                        let options: &[dashboard::Viz] = if metric {
                                            &[
                                                dashboard::Viz::Gauge,
                                                dashboard::Viz::Ring,
                                                dashboard::Viz::Bar,
                                                dashboard::Viz::Number,
                                                dashboard::Viz::Sparkline,
                                            ]
                                        } else {
                                            &[dashboard::Viz::Number, dashboard::Viz::Analog]
                                        };
                                        for viz in options {
                                            if ui.selectable_label(current == *viz, viz.as_str()).clicked() {
                                                self.dashboard.widgets[i].viz = *viz;
                                            }
                                        }
                                    });
                            });
                        }

                        if metric {
                            ui.horizontal(|ui| {
                                ui.label("Min:");
                                ui.add(egui::DragValue::new(&mut self.dashboard.widgets[i].min));
                                ui.label("Max:");
                                ui.add(egui::DragValue::new(&mut self.dashboard.widgets[i].max));
                            });
                        }

                        ui.checkbox(&mut self.dashboard.widgets[i].label, "Show label");
                        ui.add(
                            egui::Slider::new(&mut self.dashboard.widgets[i].font_scale, 0.5..=2.0)
                                .text("Font scale"),
                        );

                        // Accent override: `None` means "use the theme accent" (the
                        // renderer falls back to `theme.accent` when a widget's `accent`
                        // is `None`); checking the box seeds it from the current theme
                        // accent so the color picker starts from something sane rather
                        // than black.
                        let mut custom_accent = self.dashboard.widgets[i].accent.is_some();
                        if ui.checkbox(&mut custom_accent, "Custom accent").changed() {
                            self.dashboard.widgets[i].accent = if custom_accent {
                                Some(self.dashboard.theme.accent)
                            } else {
                                None
                            };
                        }
                        if let Some(accent) = &mut self.dashboard.widgets[i].accent {
                            ui.color_edit_button_srgb(accent);
                        }

                        match kind {
                            dashboard::WidgetKind::Clock => {
                                ui.checkbox(&mut self.dashboard.widgets[i].clock_24h, "24-hour");
                                ui.checkbox(&mut self.dashboard.widgets[i].show_seconds, "Show seconds");
                            }
                            dashboard::WidgetKind::Date => {
                                ui.horizontal(|ui| {
                                    ui.label("Date format:");
                                    ui.text_edit_singleline(&mut self.dashboard.widgets[i].date_fmt);
                                });
                            }
                            dashboard::WidgetKind::Text => {
                                ui.horizontal(|ui| {
                                    ui.label("Text:");
                                    ui.text_edit_singleline(&mut self.dashboard.widgets[i].text);
                                });
                            }
                            _ => {}
                        }
                    } else {
                        // Defensive: index somehow outlived the widgets it pointed at.
                        self.selected = None;
                    }
                }

                ui.separator();
            }

            // Task 4b: Profiles -- full-setup snapshots (mode, media selection,
            // fit/zoom/pan/bitrate, brightness, dashboard). Shown regardless of
            // the current mode (a profile can switch it). The ⚙ button opens
            // the Settings modal (Clear Cache + cache size), rendered below.
            ui.horizontal(|ui| {
                ui.heading("Profiles");
                if ui.small_button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = true;
                }
            });
            {
                // Disabled while a background load/import is in flight, same as the
                // Recent-media grid above: a stale worker's completion landing after
                // a profile Load/Delete would otherwise clobber the newer selection.
                let can_act = self.loading.is_none();
                let mut load_idx: Option<usize> = None;
                let mut delete_idx: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .max_height(90.0)
                    .id_salt("profiles_scroll")
                    .show(ui, |ui| {
                        if self.cfg.profiles.is_empty() {
                            ui.weak("No profiles saved yet.");
                        }
                        for (i, p) in self.cfg.profiles.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(&p.name);
                                if ui
                                    .add_enabled(can_act, egui::Button::new("Load").small())
                                    .clicked()
                                {
                                    load_idx = Some(i);
                                }
                                if ui
                                    .add_enabled(can_act, egui::Button::new("🗑").small())
                                    .clicked()
                                {
                                    delete_idx = Some(i);
                                }
                            });
                        }
                    });
                if let Some(i) = load_idx {
                    self.load_profile(i);
                }
                if let Some(i) = delete_idx {
                    self.delete_profile(i);
                }
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_profile_name)
                        .hint_text("Profile name…")
                        .desired_width(140.0),
                );
                let can_save = !self.new_profile_name.trim().is_empty();
                if ui
                    .add_enabled(can_save, egui::Button::new("Save current as…"))
                    .clicked()
                {
                    self.save_current_profile();
                }
            });
            ui.separator();

            // Brightness is live: send device update on every change, but debounce config
            // save to only when drag stops (or discrete change), not during drags.
            // Slider carries a percent (1..=100); device.rs maps percent -> the device's
            // native 0..=255 byte, so this value is exact/accurate as a percentage.
            let brightness_resp =
                ui.add(egui::Slider::new(&mut self.cfg.brightness, 1..=100).text("Brightness %"));
            if brightness_resp.changed() {
                let _ = self.tx.send(Command::SetBrightness(self.cfg.brightness));
                // Save only when drag stops or on discrete changes (changed && !dragged).
                if !brightness_resp.dragged() {
                    if let Err(e) = config::save(&self.cfg_path, &self.cfg) {
                        self.set_status(format!("save failed: {e}"), 6);
                    } else {
                        self.set_status("Brightness updated", 3);
                    }
                }
            }

            ui.horizontal(|ui| {
                if ui.button("Apply mode").clicked() {
                    self.apply();
                }
                // Task B-3: the pipeline sets `media_busy` true while it (re)loads and
                // pre-encodes media after this Apply (GIF pre-encode in particular can
                // take a moment), then false when done -- show a small spinner so the
                // window doesn't look like it's just ignoring the click.
                if self.media_busy.load(std::sync::atomic::Ordering::Relaxed) {
                    ui.add(egui::Spinner::new());
                    ui.label("Applying to cooler…");
                }
            });
            ui.separator();
            // Bugfix: only render the status while its transient window is still open
            // (see `set_status`/`status_until`) -- otherwise a confirmation like
            // "Applied ✓" would sit there forever. Reserve the line's height either way
            // (empty label) so the rest of the panel doesn't jump when it clears, and
            // request a repaint for exactly when it expires so it disappears on time
            // even if nothing else is driving repaints at that moment.
            match self.status_until {
                Some(t) if std::time::Instant::now() < t => {
                    ui.label(&self.status);
                    ctx.request_repaint_after(t.saturating_duration_since(std::time::Instant::now()));
                }
                _ => {
                    ui.label("");
                }
            }
        });

        // Closable "Colors" modal (replaces the old inline color-edit rows). The
        // egui::Window title bar's X sets `self.show_colors` back to false via
        // `.open(&mut self.show_colors)`; the pickers mutate `self.dashboard.theme.*`
        // exactly as the old inline rows did.
        let mut show_colors = self.show_colors;
        egui::Window::new("Colors")
            .open(&mut show_colors)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Accent:");
                    ui.color_edit_button_srgb(&mut self.dashboard.theme.accent);
                });
                ui.horizontal(|ui| {
                    ui.label("Background:");
                    ui.color_edit_button_srgb(&mut self.dashboard.theme.bg);
                });
                ui.horizontal(|ui| {
                    ui.label("Text:");
                    ui.color_edit_button_srgb(&mut self.dashboard.theme.text);
                });
                ui.horizontal(|ui| {
                    ui.label("Track:");
                    ui.color_edit_button_srgb(&mut self.dashboard.theme.track);
                });
            });
        self.show_colors = show_colors;

        // One-time (per session) bitrate warning modal, opened from the Bitrate
        // slider's `.changed()` handler above. Mirrors the "Colors" modal's
        // open/close pattern (`.open(&mut local)` + write-back), but "Got it" is the
        // primary way to close it -- it also commits "Don't show this again" (if
        // ticked) by clearing `cfg.show_bitrate_warning` and saving, so future
        // sessions/slider edits no longer pop the modal at all.
        let mut bitrate_modal = self.bitrate_modal;
        egui::Window::new("Bitrate — please read")
            .open(&mut bitrate_modal)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(
                    "Changing the media bitrate affects how the cooler's display decodes \
                     GIF/video playback.",
                );
                ui.label(
                    "Too high: the display's decoder can stutter, or in the worst case need \
                     a full power-cycle to recover.",
                );
                ui.label("Too low: motion looks softer/blockier.");
                ui.label("Default is 1500 kbps.");
                ui.label(
                    "The change takes effect when you click Apply, and re-encodes the \
                     current media.",
                );
                ui.add_space(8.0);
                ui.checkbox(&mut self.bitrate_dont_show, "Don't show this again");
                ui.add_space(4.0);
                if ui.button("Got it").clicked() {
                    self.bitrate_modal = false;
                    if self.bitrate_dont_show {
                        self.cfg.show_bitrate_warning = false;
                        let _ = config::save(&self.cfg_path, &self.cfg);
                    }
                }
            });
        // Only let the title-bar X close it (not silently re-open it): the button
        // handler above may have already flipped `self.bitrate_modal` to false this
        // same frame, and this write-back must not undo that.
        if !bitrate_modal {
            self.bitrate_modal = false;
        }

        // Task 4b: ⚙ Settings modal -- Clear Cache (deletes every unpinned
        // Recent-media entry's files) + a shown cache-on-disk size. Mirrors the
        // "Colors"/"Bitrate" modals' open/close pattern (`.open(&mut local)` +
        // write-back via the title-bar X).
        let mut show_settings = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut show_settings)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                if ui.button("Clear Cache").clicked() {
                    self.recent.clear_unpinned();
                    let _ = self.recent.save();
                    // `refresh_recent()` reloads the index from disk (dropping every
                    // just-deleted unpinned entry) and prunes `recent_thumbs` to match.
                    self.refresh_recent();
                    // Bugfix: if the currently-selected/previewing media was itself
                    // unpinned (and just got deleted above), its cache entry is gone
                    // from `self.recent` now -- clear the selection too, mirroring
                    // `remove_recent_entry`, so a later Apply doesn't send
                    // `Command::LoadCachedVideo`/keep showing a preview for a path that
                    // no longer exists while the GUI still claims "Applied ✓".
                    let media_removed = self
                        .current_media_id
                        .as_ref()
                        .map(|id| self.recent.get(id).is_none())
                        .unwrap_or(false);
                    if media_removed {
                        self.current_media_id = None;
                        self.current_video_lcdv = None;
                        self.current_video_src = None;
                        self.video_import_settings = None;
                        self.video_preview = None;
                        self.set_status("Cleared — media removed", 3);
                    } else {
                        self.set_status("Cache cleared", 3);
                    }
                }
                ui.weak("Pinned media (referenced by a saved Profile) is kept.");
                ui.add_space(8.0);
                let mb = cache_dir_size() as f64 / (1024.0 * 1024.0);
                ui.label(format!("Cache: {mb:.1} MB"));
            });
        self.show_settings = show_settings;
    }
}

/// Spawns the dedicated tray-event thread.
///
/// While the window is hidden, eframe stops invoking `App::update`, so polling tray
/// events from inside `update` (as a previous version of this app did) misses Open /
/// double-click while minimized to tray -- exactly when they matter. This thread polls
/// the tray's event receivers directly regardless of window visibility, but instead of
/// driving `egui::Context::send_viewport_cmd` (which is only flushed the next time
/// `update()` runs -- never, while hidden, since `request_repaint()` does not wake a
/// hidden eframe window on Windows), Open/DoubleClick/Click show the window via direct
/// Win32 calls (`crate::winwnd`) and Quit exits the process directly. `ctx` is kept as a
/// parameter (unused) so callers don't need to change; it may be dropped later.
///
/// This is the sole consumer of both `tray_icon` event receivers; nothing else must
/// drain them, or events could be split between two pollers and missed.
pub fn spawn_tray_thread(
    _ctx: egui::Context,
    tray_ids: TrayIds,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let menu_rx = tray_icon::menu::MenuEvent::receiver();
        let tray_rx = tray_icon::TrayIconEvent::receiver();

        // DIAGNOSTIC (task 4c): log the ids this thread will compare incoming
        // MenuEvents against, so a log capture can show whether they match the
        // ids actually assigned to the menu items built in `build_tray`.
        log::info!(
            "tray thread started; open_id={:?} quit_id={:?}",
            tray_ids.open_id,
            tray_ids.quit_id
        );

        while !stop.load(Ordering::Relaxed) {
            let mut acted = false;

            while let Ok(ev) = menu_rx.try_recv() {
                acted = true;
                // DIAGNOSTIC (task 4c): raw event id as received from tray_icon/muda.
                log::info!("MenuEvent received: id={:?}", ev.id);
                if ev.id == tray_ids.open_id {
                    log::info!("MenuEvent branch matched: open");
                    log::info!("tray: Win32 show_window (menu Open)");
                    crate::winwnd::show_window();
                } else if ev.id == tray_ids.quit_id {
                    log::info!("MenuEvent branch matched: quit");
                    // eframe cannot flush a Close viewport command while the window
                    // is hidden (see module doc comment + winwnd.rs), so quitting
                    // from the tray exits the process directly. This is the intended
                    // Quit behavior: the OS tears down the process, which drops the
                    // pipeline's COM3 handle and blanks the LCD.
                    log::info!("tray: process::exit(0) (menu Quit)");
                    stop.store(true, Ordering::Relaxed);
                    std::process::exit(0);
                } else {
                    log::info!("MenuEvent branch matched: none (id did not match open_id or quit_id)");
                }
            }

            while let Ok(ev) = tray_rx.try_recv() {
                acted = true;
                // DIAGNOSTIC (task 4c): Debug-print the whole event so we can see
                // Click/DoubleClick/Enter/Leave/Move/etc. and their fields.
                log::info!("TrayIconEvent received: {:?}", ev);
                match ev {
                    tray_icon::TrayIconEvent::DoubleClick { .. } => {
                        log::info!("tray: Win32 show_window (tray DoubleClick)");
                        crate::winwnd::show_window();
                    }
                    tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } => {
                        log::info!("tray: Win32 show_window (tray Click Left/Up)");
                        crate::winwnd::show_window();
                    }
                    _ => {}
                }
            }

            if !acted {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
    })
}
