//! Direct Win32 window show/hide helpers.
//!
//! `eframe` stops calling `App::update` while the viewport is hidden (see
//! `gui::spawn_tray_thread`'s doc comment for the full story), so
//! `send_viewport_cmd(Visible(true))` sent from a background thread is queued but
//! never flushed until *something* wakes the hidden window back up -- and
//! `request_repaint()` does not do that reliably for a hidden window on Windows.
//!
//! To break that chicken-and-egg problem, tray show/hide/focus goes straight through
//! Win32 instead of through egui's viewport-command queue: `FindWindowW` locates our
//! top-level window by its exact title (works even while hidden -- `FindWindow`
//! ignores visibility), then `ShowWindow` / `SetForegroundWindow` act on it directly.
//!
//! The window title must exactly match the string passed as the first argument to
//! `eframe::run_native` in `main.rs` ("Astroshel Lean Display"), since eframe uses
//! that as the native window's title by default.

use windows::core::w;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
};

/// Finds our top-level window by its exact title. Returns `None` if it isn't found
/// (e.g. called before the window is created, or the title ever changes) rather than
/// panicking -- callers must treat a miss as a harmless no-op.
fn find_main_window() -> Option<HWND> {
    // SAFETY: FindWindowW only reads its (static, null-terminated) arguments and
    // returns a handle or null; it has no other side effects.
    let hwnd = unsafe { FindWindowW(None, w!("Astroshel Lean Display")) }.ok()?;
    if hwnd.is_invalid() {
        None
    } else {
        Some(hwnd)
    }
}

/// Shows (and restores/focuses) the main window via Win32, bypassing eframe's
/// viewport-command queue so it works even while the window is hidden.
pub fn show_window() {
    let Some(hwnd) = find_main_window() else {
        log::warn!("winwnd::show_window: main window not found");
        return;
    };
    // SAFETY: hwnd came from a successful FindWindowW just above; these calls just
    // change window visibility/focus state and return a status BOOL we intentionally
    // ignore (best-effort UI action, nothing to recover from on failure).
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Hides the main window via Win32 (minimize-to-tray), bypassing eframe's
/// viewport-command queue.
pub fn hide_window() {
    let Some(hwnd) = find_main_window() else {
        log::warn!("winwnd::hide_window: main window not found");
        return;
    };
    // SAFETY: see show_window; same reasoning applies.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}
