use crate::command::{Command, Mode};
use crate::config::{self, Config};
use crate::media;
use crate::render;
use crate::sensors;
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
    // Phase 3b: wall-clock epoch used to animate GIF previews. Reset whenever a new
    // media file is loaded (Open handler) so playback always restarts from frame 0.
    gif_epoch: std::time::Instant,
}

impl App {
    pub fn new(
        cfg: Config,
        cfg_path: PathBuf,
        tx: Sender<Command>,
        tray: Option<tray_icon::TrayIcon>,
        has_tray: bool,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        // Renderer init only fails if the embedded font asset fails to parse, which
        // would be a build-time asset defect, not a runtime condition -- so treat it
        // like the other places (render.rs tests) that unwrap-on-init.
        let mut renderer = render::new().expect("renderer init");
        renderer.twelve_hour = cfg.twelve_hour;
        let sensors = sensors::new();
        let media_obj = cfg
            .media
            .path
            .as_ref()
            .and_then(|p| media::load(std::path::Path::new(p)).ok());
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
        }
    }

    /// Returns the current 320x320 RGBA preview frame by reusing the exact same
    /// render/compose pipeline the device output uses (WYSIWYG): dashboard via
    /// `render::Renderer::dashboard`, or media via `media::placement` +
    /// `render::compose_media`. Falls back to the dashboard if mode is "media" but
    /// nothing is loaded.
    /// Returns the preview RGBA frame plus whether it's currently an animated GIF
    /// being shown in media mode (used by `update` to pick a faster repaint rate).
    fn preview_rgba(&mut self) -> (Vec<u8>, bool) {
        let want_media = self.cfg.mode.eq_ignore_ascii_case("media");
        if want_media {
            let fit = crate::media::Fit::from_str(&self.cfg.media.fit);
            let zoom = self.cfg.media.zoom;
            let pan = self.cfg.media.pan;
            match &self.media_obj {
                Some(crate::media::Media::Static(img)) => {
                    let (w, h) = img.dimensions();
                    let p = crate::media::placement(w, h, fit, zoom, pan);
                    return (crate::render::compose_media(img, &p, [8, 10, 18, 255]), false);
                }
                Some(crate::media::Media::Animated(frames)) => {
                    // Pick the frame whose cumulative delay window contains the current
                    // point in the loop, based on wall-clock elapsed time since
                    // `gif_epoch` (reset whenever a new media file is loaded).
                    if !frames.is_empty() {
                        let elapsed_ms = self.gif_epoch.elapsed().as_millis() as u32;
                        let total: u32 = frames.iter().map(|f| f.delay_ms.max(40)).sum();
                        let mut t = elapsed_ms % total.max(1);
                        let mut chosen = &frames[0];
                        for f in frames.iter() {
                            let d = f.delay_ms.max(40);
                            if t < d {
                                chosen = f;
                                break;
                            }
                            t -= d;
                        }
                        let (w, h) = chosen.img.dimensions();
                        let p = crate::media::placement(w, h, fit, zoom, pan);
                        return (crate::render::compose_media(&chosen.img, &p, [8, 10, 18, 255]), true);
                    }
                }
                None => {}
            }
        }
        // Dashboard (or media mode with nothing loaded).
        let snap = self.sensors.read();
        (self.renderer.dashboard(&snap), false)
    }

    /// Commits the Dashboard/Media mode (and, in Media mode, the loaded media plus its
    /// fit/zoom/pan) to the running pipeline and saves it to disk. Brightness is applied
    /// live (see the slider in `update`), so it is intentionally NOT sent here.
    fn apply(&mut self) {
        let mode = Mode::from_str(&self.cfg.mode);
        let _ = self.tx.send(Command::SetMode(mode));
        if mode == Mode::Media {
            if let Some(path) = &self.cfg.media.path {
                let _ = self.tx.send(Command::LoadMedia {
                    path: path.clone(),
                    fit: self.cfg.media.fit.clone(),
                    zoom: self.cfg.media.zoom,
                    pan: self.cfg.media.pan,
                });
            }
        }
        match config::save(&self.cfg_path, &self.cfg) {
            Ok(_) => self.status = "Applied".into(),
            Err(e) => self.status = format!("Save failed: {e}"),
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

        // Phase 2c: render the current dashboard/media frame through the shared
        // render pipeline (WYSIWYG with the device output) and upload it as an egui
        // texture. Repainting on a timer (rather than only on input) keeps the clock
        // and sensor readouts, and any animated media's first frame, visibly live.
        let (rgba, is_animated_gif) = self.preview_rgba();
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
            if let Some(tex) = &self.preview_tex {
                let mut resp = ui.add(
                    egui::Image::new((tex.id(), egui::vec2(320.0, 320.0)))
                        .sense(egui::Sense::drag()),
                );

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
                    // Add tooltip on hover.
                    let _ = resp.on_hover_text("Drag to pan · use Zoom to zoom in");
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
                if ui.button("Open image/GIF…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Image/GIF", &["png", "jpg", "jpeg", "gif", "bmp", "webp"])
                        .pick_file()
                    {
                        let ps = path.display().to_string();
                        match media::load(&path) {
                            Ok(m) => {
                                self.media_obj = Some(m);
                                self.cfg.media.path = Some(ps);
                                self.cfg.media.zoom = 1.0;
                                self.cfg.media.pan = [0.0, 0.0];
                                self.gif_epoch = std::time::Instant::now();
                                self.status = "Loaded (click Apply to show on cooler)".into();
                            }
                            Err(e) => {
                                self.status = format!("Load failed: {e}");
                            }
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

                ui.separator();
            }

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
                        self.status = format!("save failed: {e}");
                    } else {
                        self.status = "Brightness updated".into();
                    }
                }
            }

            if ui.button("Apply mode").clicked() {
                self.apply();
            }
            ui.separator();
            ui.label(&self.status);
        });
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
