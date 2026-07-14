#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cache;
mod command;
mod config;
mod dashboard;
mod device;
mod editor;
mod encode;
mod gui;
mod icon;
mod media;
mod pipeline;
mod proto;
mod render;
mod sensors;
mod video;
mod winwnd;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn config_path() -> std::path::PathBuf {
    let dir = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&dir).join("astroshel-lcd").join("config.toml")
}

fn set_logon(enable: bool) {
    // HKCU\Software\Microsoft\Windows\CurrentVersion\Run
    let exe = std::env::current_exe().ok();
    let cmd = if enable { exe } else { None };
    // Use `winreg` if added; for Phase 1 a best-effort shell to reg.exe keeps deps minimal:
    if let Some(path) = cmd {
        let _ = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "AstroshelLcd",
                "/t",
                "REG_SZ",
                "/d",
                &path.display().to_string(),
                "/f",
            ])
            .status();
    } else {
        let _ = std::process::Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "AstroshelLcd",
                "/f",
            ])
            .status();
    }
}

fn init_logging() {
    let dir = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("AstroshelLcd");
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("astroshel-lcd.log"));
    let level = log::LevelFilter::Info;
    match file {
        Ok(f) => { let _ = simplelog::WriteLogger::init(level, simplelog::Config::default(), f); }
        Err(_) => { let _ = simplelog::SimpleLogger::init(level, simplelog::Config::default()); }
    }
}

fn main() -> eframe::Result<()> {
    init_logging();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));

    // Windows' default timer granularity is ~15.6ms, which makes the pipeline's short
    // (down to 5ms) interruptible sleeps imprecise -- e.g. a nominal 5ms sleep can
    // actually take up to ~15ms, which quantizes GIF frame pacing coarsely and
    // reintroduces stutter. Raising the global timer resolution to 1ms for this
    // process fixes that; it's a machine-wide setting while any process holds it, so
    // we pair it with `timeEndPeriod(1)` on the way out (best-effort: the process may
    // also exit via `process::exit`/panic, in which case Windows reclaims it anyway).
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }

    let cfg_path = config_path();
    let cfg = config::load(&cfg_path);
    set_logon(cfg.start_at_logon);

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<command::Command>();
    // Shared "busy" flag: set by the pipeline worker while it's (re)loading/pre-encoding
    // media (GIF pre-encode or a static image), so the GUI can show an "Applying..."
    // indicator instead of appearing to hang.
    let media_busy = Arc::new(AtomicBool::new(false));

    // Worker: the pipeline (owns COM3).
    let worker_cfg = cfg.clone();
    let stop_worker = stop.clone();
    let worker_media_busy = media_busy.clone();
    let worker = std::thread::spawn(move || {
        if let Err(e) = pipeline::run(worker_cfg, stop_worker, rx, worker_media_busy) {
            log::error!("pipeline: {e:#}");
        }
    });

    // Inner size tall enough to show the full 320x320 preview (heading + "Preview"
    // label + margins on top of the 320px image) without the SidePanel/CentralPanel
    // clipping the bottom rows (e.g. the CPU/GPU bars) -- see gui.rs `update`.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([600.0, 460.0])
        .with_min_inner_size([560.0, 440.0])
        .with_visible(false); // start hidden (tray-first)
    // Brand icon for the window/taskbar. Rendered at 256px for a crisp Alt-Tab /
    // taskbar thumbnail; falls back to eframe's default (no icon) if rasterizing
    // the SVG asset fails for any reason.
    if let Some((rgba, width, height)) = icon::load_icon_rgba(256) {
        viewport = viewport.with_icon(std::sync::Arc::new(egui::IconData { rgba, width, height }));
    } else {
        log::warn!("window icon: failed to rasterize assets/logo.svg; using default icon");
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let stop_gui = stop.clone();
    let res = eframe::run_native(
        "Astroshel Lean Display",
        native_options,
        Box::new(move |cc| {
            // Build the tray icon here: this closure runs inside eframe's winit
            // `resumed` callback, i.e. after the event loop is actually running.
            // Building it earlier is unreliable on some platforms
            // (see tauri-apps/tray-icon#90).
            let (tray, tray_ids, has_tray) = match build_tray() {
                Some((tray, tray_ids)) => (Some(tray), tray_ids, true),
                None => (None, gui::TrayIds::default(), false),
            };
            if has_tray {
                // Dedicated thread: drives `ctx` directly so tray Open/double-click
                // work even while eframe stops calling `update()` on a hidden window.
                gui::spawn_tray_thread(cc.egui_ctx.clone(), tray_ids, stop_gui.clone());
            }
            Ok(Box::new(gui::App::new(cfg, cfg_path, tx, tray, has_tray, stop_gui, media_busy)))
        }),
    );

    // Ensure the worker's interruptible sleeps observe `stop` and `join()` returns,
    // even if `run_native` exits some other way than via the tray Quit path.
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();

    // Best-effort: releases the 1ms timer resolution requested above. Safe to skip if
    // the process is exiting via `process::exit`/panic -- Windows reclaims the
    // resolution request when the process terminates regardless.
    unsafe {
        windows::Win32::Media::timeEndPeriod(1);
    }

    res
}

/// Builds the tray icon image: the rasterized brand logo, or (if that fails for any
/// reason) a small solid-color square fallback so the tray icon is never missing.
fn tray_icon_image() -> tray_icon::Icon {
    const SIZE: u32 = 32;
    if let Some((rgba, w, h)) = icon::load_icon_rgba(SIZE) {
        if let Ok(icon) = tray_icon::Icon::from_rgba(rgba, w, h) {
            return icon;
        }
        log::warn!("tray icon: rasterized logo rejected by tray_icon::Icon::from_rgba");
    } else {
        log::warn!("tray icon: failed to rasterize assets/logo.svg; using fallback square");
    }

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 0; // R
        px[1] = 224; // G
        px[2] = 150; // B
        px[3] = 255; // A
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("valid fallback icon buffer")
}

/// Builds the tray icon + its (Open, Quit) menu. `tray-icon` (via `muda`) talks to Win32
/// directly and does not own an event loop of its own, so it coexists on the same OS
/// thread/message queue that eframe's winit event loop pumps -- no competing event loops.
///
/// Returns `None` if tray creation fails (e.g. no notification area / locked-down session).
/// The app degrades gracefully to windowed-only mode.
fn build_tray() -> Option<(tray_icon::TrayIcon, gui::TrayIds)> {
    use tray_icon::menu::{Menu, MenuId, MenuItem};
    use tray_icon::TrayIconBuilder;

    let open_id = MenuId::new("astroshel-lcd-open");
    let quit_id = MenuId::new("astroshel-lcd-quit");

    let menu = Menu::new();
    let open_item = MenuItem::with_id(open_id.clone(), "Open", true, None);
    let quit_item = MenuItem::with_id(quit_id.clone(), "Quit Astroshel LCD", true, None);
    // DIAGNOSTIC (task 4c): the ids assigned to the menu items as constructed here.
    // `MenuItem::with_id` is documented/expected to preserve the id passed in, but
    // logging both the requested id and the item's own `.id()` lets a log capture
    // confirm whether they actually match what `spawn_tray_thread` compares against.
    log::info!(
        "build_tray: created open_item id={:?} (requested open_id={:?}), quit_item id={:?} (requested quit_id={:?})",
        open_item.id(),
        open_id,
        quit_item.id(),
        quit_id
    );
    if let Err(e) = menu.append(&open_item) {
        log::error!("tray menu append failed: {e:#}");
    }
    if let Err(e) = menu.append(&quit_item) {
        log::error!("tray menu append failed: {e:#}");
    }

    let tray = TrayIconBuilder::new()
        .with_tooltip("Astroshel Lean Display")
        .with_icon(tray_icon_image())
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| {
            log::error!("tray build failed: {e:#}");
            e
        })
        .ok()?;

    log::info!(
        "tray built ok; TrayIds.open_id={:?} TrayIds.quit_id={:?}",
        open_id,
        quit_id
    );
    Some((tray, gui::TrayIds { open_id, quit_id }))
}
