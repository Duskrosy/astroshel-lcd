use crate::dashboard;
use crate::sensors::Snapshot;
use anyhow::Result;
use chrono::Timelike;
use std::collections::VecDeque;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

const W: u32 = 320;
const H: u32 = 320;
const ACCENT: (u8, u8, u8) = (0, 224, 150);
const TRACK: (u8, u8, u8) = (38, 42, 58);
const BG: (u8, u8, u8) = (8, 10, 18);
const LABEL: (u8, u8, u8) = (150, 160, 180);
const VALUE: (u8, u8, u8) = (240, 245, 255);

/// Ring-buffer cap for `History`: keep the most recent N samples per widget kind.
const HISTORY_CAP: usize = 60;

/// Map a value into a 0.0..=1.0 fraction between `min` and `max` (clamped).
/// Used by Gauge/Ring/Bar to compute sweep/fill fraction, and by Sparkline to
/// map series values into the plot rect.
pub fn frac(value: f32, min: f32, max: f32) -> f32 {
    if !(max > min) {
        return 0.0;
    }
    ((value - min) / (max - min)).clamp(0.0, 1.0)
}

/// Map a value into a 0.0..=1.0 fraction of the gauge arc (clamped).
/// Kept for backward compatibility with the legacy `dashboard()` renderer.
pub fn gauge_sweep(value: f32, min: f32, max: f32) -> f32 {
    frac(value, min, max)
}

fn kind_idx(kind: dashboard::WidgetKind) -> usize {
    use dashboard::WidgetKind::*;
    match kind {
        GpuTemp => 0,
        GpuUsage => 1,
        CpuUsage => 2,
        RamUsage => 3,
        Clock => 4,
        Date => 5,
        Text => 6,
    }
}

/// Per-`WidgetKind` ring buffer of recent `f32` samples (cap `HISTORY_CAP`),
/// used by the Sparkline visualization. Oldest samples are dropped first
/// (FIFO); `series()` returns samples oldest-first.
#[derive(Debug, Clone)]
pub struct History {
    buffers: [VecDeque<f32>; 7],
}

impl History {
    pub fn new() -> History {
        History {
            buffers: Default::default(),
        }
    }

    pub fn push(&mut self, kind: dashboard::WidgetKind, value: f32) {
        let buf = &mut self.buffers[kind_idx(kind)];
        buf.push_back(value);
        while buf.len() > HISTORY_CAP {
            buf.pop_front();
        }
    }

    /// Samples for `kind`, oldest first.
    pub fn series(&self, kind: dashboard::WidgetKind) -> Vec<f32> {
        self.buffers[kind_idx(kind)].iter().copied().collect()
    }
}

impl Default for History {
    fn default() -> Self {
        History::new()
    }
}

pub struct Renderer {
    font: fontdue::Font,
    pixmap: Pixmap,
    pub twelve_hour: bool,
}

pub fn new() -> Result<Renderer> {
    let bytes = include_bytes!("../assets/font.ttf");
    let font = fontdue::Font::from_bytes(bytes as &[u8], fontdue::FontSettings::default())
        .map_err(|e| anyhow::anyhow!("font load: {e}"))?;
    let pixmap = Pixmap::new(W, H).ok_or_else(|| anyhow::anyhow!("pixmap alloc"))?;
    Ok(Renderer { font, pixmap, twelve_hour: true })
}

// ---------------------------------------------------------------------------
// Free-standing draw primitives shared by the new rendering engine. These take
// the target `Pixmap` explicitly (rather than mutating `Renderer::pixmap`) so
// `render_dashboard` can work on a fresh, appropriately-sized canvas via `&self`.
// ---------------------------------------------------------------------------

fn tsk_color(rgb: dashboard::Color) -> Color {
    Color::from_rgba8(rgb[0], rgb[1], rgb[2], 255)
}

/// Fill an axis-aligned rect. No-op (does not panic) for degenerate/negative sizes.
fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgb: dashboard::Color) {
    if !(w > 0.0) || !(h > 0.0) {
        return;
    }
    let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) else { return };
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(tsk_color(rgb));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Build a rounded-rect path, clamping the corner radius to fit within `w`/`h`.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    if !(w > 0.0) || !(h > 0.0) {
        return None;
    }
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    if r <= 0.01 {
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
    } else {
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
    }
    pb.finish()
}

fn fill_rounded_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, radius: f32, rgb: dashboard::Color) {
    if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
        let mut paint = Paint::default();
        paint.set_color(tsk_color(rgb));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Stroke an arc centered at `(cx, cy)`, starting at `start_deg` and sweeping
/// `total_deg * frac.clamp(0..1)` degrees. Angle convention matches the legacy
/// `arc()` helper (screen coords, angle measured via cos/sin directly). No-op
/// for a non-positive radius/stroke width or a ~zero fraction.
#[allow(clippy::too_many_arguments)]
fn stroke_arc(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, start_deg: f32, total_deg: f32, frac: f32, stroke_w: f32, rgb: dashboard::Color) {
    let frac = frac.clamp(0.0, 1.0);
    if !(r > 0.0) || !(stroke_w > 0.0) || frac <= 0.0001 {
        return;
    }
    let start = start_deg.to_radians();
    let total = total_deg.to_radians();
    let end = start + total * frac;
    let mut pb = PathBuilder::new();
    let steps = 96;
    for i in 0..=steps {
        let a = start + (end - start) * (i as f32 / steps as f32);
        let (px, py) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(px, py);
        } else {
            pb.line_to(px, py);
        }
    }
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(tsk_color(rgb));
        paint.anti_alias = true;
        let mut stroke = Stroke::default();
        stroke.width = stroke_w;
        stroke.line_cap = tiny_skia::LineCap::Round;
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Stroke a straight line segment from `(x0, y0)` to `(x1, y1)`. Used by the
/// analog clock for tick marks and hands. No-op for a non-positive stroke width.
fn stroke_line(pixmap: &mut Pixmap, x0: f32, y0: f32, x1: f32, y1: f32, stroke_w: f32, rgb: dashboard::Color) {
    if !(stroke_w > 0.0) {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(tsk_color(rgb));
        paint.anti_alias = true;
        let mut stroke = Stroke::default();
        stroke.width = stroke_w;
        stroke.line_cap = tiny_skia::LineCap::Round;
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Point at `len` from `(cx, cy)` for a clock-hand/tick angle expressed as
/// `frac` of a full turn (0.0 == 12 o'clock, 0.25 == 3 o'clock, clockwise).
fn clock_point(cx: f32, cy: f32, frac: f32, len: f32) -> (f32, f32) {
    let deg = frac * 360.0 - 90.0;
    let rad = deg.to_radians();
    (cx + len * rad.cos(), cy + len * rad.sin())
}

/// Blit a rasterized glyph bitmap into `pixmap` at top-left `(x, y)` (pixmap
/// coordinates), alpha-blending onto whatever is already there. Bounds-checked
/// against `pixmap`'s actual dimensions -- never panics, even for glyphs that
/// fall partially or fully outside the canvas.
fn blit_glyph_to(pixmap: &mut Pixmap, bitmap: &[u8], gw: usize, gh: usize, x: f32, y: f32, rgb: dashboard::Color) {
    if gw == 0 || gh == 0 {
        return;
    }
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let data = pixmap.data_mut();
    for gy in 0..gh {
        for gx in 0..gw {
            let a = bitmap[gy * gw + gx] as u32;
            if a == 0 {
                continue;
            }
            let ppx = x as i32 + gx as i32;
            let ppy = y as i32 + gy as i32;
            if ppx < 0 || ppy < 0 || ppx >= pw || ppy >= ph {
                continue;
            }
            let idx = ((ppy as u32 * pw as u32 + ppx as u32) * 4) as usize;
            if idx + 3 >= data.len() {
                continue;
            }
            for (k, c) in [rgb[0], rgb[1], rgb[2]].iter().enumerate() {
                let bg = data[idx + k] as u32;
                data[idx + k] = ((*c as u32 * a + bg * (255 - a)) / 255) as u8;
            }
            data[idx + 3] = 255;
        }
    }
}

impl Renderer {
    pub fn dashboard(&mut self, s: &Snapshot) -> Vec<u8> {
        self.pixmap.fill(Color::from_rgba8(BG.0, BG.1, BG.2, 255));

        // Clock top-center
        let clock = if self.twelve_hour {
            s.time.format("%-I:%M %p").to_string()
        } else {
            s.time.format("%H:%M").to_string()
        };
        let date = s.time.format("%a %d %b").to_string();
        self.text_center(&clock, 160.0, 12.0, 34.0, VALUE);
        self.text_center(&date, 160.0, 50.0, 16.0, LABEL);

        // Center GPU-temp arc gauge
        let temp = s.gpu_temp_c.unwrap_or(0) as f32;
        let sweep = gauge_sweep(temp, 30.0, 90.0);
        self.arc(160.0, 172.0, 92.0, 1.0, TRACK); // full track
        self.arc(160.0, 172.0, 92.0, sweep, ACCENT); // value
        let temp_txt = s
            .gpu_temp_c
            .map(|t| format!("{t}\u{00B0}"))
            .unwrap_or_else(|| "--".into());
        self.text_center(&temp_txt, 160.0, 138.0, 64.0, VALUE);
        self.text_center("GPU TEMP", 160.0, 210.0, 16.0, LABEL);

        // Bottom: CPU% / GPU% bars
        self.bar(28.0, 276.0, 120.0, s.cpu_usage_pct, "CPU");
        self.bar(172.0, 276.0, 120.0, s.gpu_usage_pct.unwrap_or(0), "GPU");

        self.pixmap.data().to_vec()
    }

    fn arc(&mut self, cx: f32, cy: f32, r: f32, sweep: f32, rgb: (u8, u8, u8)) {
        // 270 degree gauge from 135deg to 45deg (clockwise), starting bottom-left.
        let start = 135f32.to_radians();
        let total = 270f32.to_radians();
        let end = start + total * sweep;
        let mut pb = PathBuilder::new();
        let steps = 96;
        for i in 0..=steps {
            let a = start + (end - start) * (i as f32 / steps as f32);
            let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(rgb.0, rgb.1, rgb.2, 255));
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = 16.0;
            stroke.line_cap = tiny_skia::LineCap::Round;
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn bar(&mut self, x: f32, y: f32, w: f32, pct: u32, label: &str) {
        let h = 10.0;
        self.rect(x, y, w, h, TRACK);
        let fill = w * (pct.min(100) as f32 / 100.0);
        self.rect(x, y, fill, h, ACCENT);
        self.text_left(&format!("{label} {pct}%"), x, y - 20.0, 15.0, LABEL);
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: (u8, u8, u8)) {
        if w <= 0.0 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h).unwrap());
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(rgb.0, rgb.1, rgb.2, 255));
            self.pixmap
                .fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    fn text_left(&mut self, text: &str, x: f32, y: f32, px: f32, rgb: (u8, u8, u8)) {
        let mut pen_x = x;
        for ch in text.chars() {
            let (m, bitmap) = self.font.rasterize(ch, px);
            self.blit_glyph(
                &bitmap,
                m.width,
                m.height,
                pen_x + m.xmin as f32,
                y - m.ymin as f32 - m.height as f32 + px,
                rgb,
            );
            pen_x += m.advance_width;
        }
    }

    fn text_center(&mut self, text: &str, cx: f32, y: f32, px: f32, rgb: (u8, u8, u8)) {
        let width: f32 = text.chars().map(|c| self.font.metrics(c, px).advance_width).sum();
        self.text_left(text, cx - width / 2.0, y, px, rgb);
    }

    fn blit_glyph(&mut self, bitmap: &[u8], gw: usize, gh: usize, x: f32, y: f32, rgb: (u8, u8, u8)) {
        let data = self.pixmap.data_mut();
        for gy in 0..gh {
            for gx in 0..gw {
                let a = bitmap[gy * gw + gx] as u32;
                if a == 0 {
                    continue;
                }
                let px = x as i32 + gx as i32;
                let py = y as i32 + gy as i32;
                if px < 0 || py < 0 || px >= W as i32 || py >= H as i32 {
                    continue;
                }
                let idx = ((py as u32 * W + px as u32) * 4) as usize;
                for (k, c) in [rgb.0, rgb.1, rgb.2].iter().enumerate() {
                    let bg = data[idx + k] as u32;
                    data[idx + k] = ((*c as u32 * a + bg * (255 - a)) / 255) as u8;
                }
                data[idx + 3] = 255;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Data-driven dashboard rendering engine (Phase 3a Task 3).
    // -----------------------------------------------------------------------

    /// Render a full `Dashboard` (theme + widgets) against a sensor `Snapshot`
    /// and metric `History`, returning `CANVAS*CANVAS*4` RGBA bytes. Never
    /// panics: degenerate widget rects, missing sensor values, out-of-canvas
    /// rects, and empty history series are all handled gracefully.
    pub fn render_dashboard(&self, dash: &dashboard::Dashboard, snap: &Snapshot, hist: &History) -> Vec<u8> {
        let side = dashboard::CANVAS;
        let mut pixmap = match Pixmap::new(side, side) {
            Some(p) => p,
            None => return Vec::new(),
        };
        pixmap.fill(tsk_color(dash.theme.bg));
        for w in &dash.widgets {
            self.draw_widget(&mut pixmap, dash.theme, w, snap, hist);
        }
        pixmap.data().to_vec()
    }

    /// Sum of per-glyph `advance_width` for `text` rendered at pixel size `px`
    /// (fontdue metrics) -- the actual rendered width used for auto-fit and
    /// centering.
    fn text_width(&self, text: &str, px: f32) -> f32 {
        text.chars().map(|c| self.font.metrics(c, px).advance_width).sum()
    }

    /// Vertically-centered text draw, used by every text path in the new
    /// engine. Uses fontdue's line metrics (ascent/descent) rather than the
    /// nominal font size to compute the baseline, which is what actually
    /// centers text regardless of a glyph's specific bounding box (this is
    /// the fix for the old off-center GPU-temp readout).
    ///
    /// `max_w` is the target width the text must fit within (typically the
    /// widget rect width minus small padding). If the text rendered at `px`
    /// would be wider than `max_w`, the pixel size is scaled down (never up)
    /// so it fits -- this is what stops e.g. Big Clock's digital readout from
    /// overflowing its widget rect. Pass a non-positive `max_w` to skip
    /// fitting entirely.
    fn draw_text_centered(&self, pixmap: &mut Pixmap, text: &str, cx: f32, cy: f32, px: f32, max_w: f32, rgb: dashboard::Color) {
        if text.is_empty() || !(px > 0.0) {
            return;
        }
        let mut px = px;
        if max_w > 0.0 {
            let raw_w = self.text_width(text, px);
            if raw_w > max_w {
                let scale = (max_w / raw_w).clamp(0.05, 1.0);
                px = (px * scale).max(4.0);
            }
        }
        let metrics = self.font.horizontal_line_metrics(px).unwrap_or(fontdue::LineMetrics {
            ascent: px * 0.8,
            descent: -px * 0.2,
            line_gap: 0.0,
            new_line_size: px,
        });
        // Baseline that centers the [ascent, descent] box on cy. ascent > 0
        // (above baseline), descent < 0 (below baseline) in fontdue's convention.
        let baseline_y = cy + (metrics.ascent + metrics.descent) / 2.0;
        let width: f32 = self.text_width(text, px);
        let mut pen_x = cx - width / 2.0;
        for ch in text.chars() {
            let (m, bitmap) = self.font.rasterize(ch, px);
            let gx = pen_x + m.xmin as f32;
            let gy = baseline_y - m.ymin as f32 - m.height as f32;
            blit_glyph_to(pixmap, &bitmap, m.width, m.height, gx, gy, rgb);
            pen_x += m.advance_width;
        }
    }

    fn draw_widget(&self, pixmap: &mut Pixmap, theme: dashboard::Theme, w: &dashboard::Widget, snap: &Snapshot, hist: &History) {
        use dashboard::{Viz, WidgetKind};

        let rect = w.rect;
        if rect.w == 0 || rect.h == 0 {
            return; // degenerate rect: nothing to draw, nothing to panic on
        }

        let value = snap.value_for(w.kind);
        let (primary, caption) = snap.display_for(w);
        let accent = w.accent.unwrap_or(theme.accent);

        let (x, y, rw, rh) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
        let cx = x + rw / 2.0;
        let cy = y + rh / 2.0;

        // Clock with an analog face bypasses the text-readout path entirely.
        if w.kind == WidgetKind::Clock && w.viz == Viz::Analog {
            self.draw_analog_clock(pixmap, theme, accent, snap, x, y, rw, rh, cx, cy);
            return;
        }

        // Clock/Date/Text are kind-driven and ignore `viz` otherwise.
        match w.kind {
            WidgetKind::Clock | WidgetKind::Date | WidgetKind::Text => {
                let px = (rh * 0.4 * w.font_scale).clamp(6.0, 140.0);
                let max_w = (rw - 8.0).max(4.0);
                self.draw_text_centered(pixmap, &primary, cx, cy, px, max_w, theme.text);
                return;
            }
            _ => {}
        }

        match w.viz {
            Viz::Gauge => self.draw_gauge(pixmap, w, theme, accent, value, &primary, caption.as_deref(), x, y, rw, rh, cx, cy),
            Viz::Ring => self.draw_ring(pixmap, w, theme, accent, value, &primary, caption.as_deref(), x, y, rw, rh, cx, cy),
            Viz::Bar => self.draw_bar(pixmap, w, theme, accent, value, &primary, caption.as_deref(), x, y, rw, rh, cx, cy),
            Viz::Number | Viz::Analog => self.draw_number(pixmap, w, theme, &primary, caption.as_deref(), rw, rh, cx, cy),
            Viz::Sparkline => self.draw_sparkline(pixmap, w, theme, accent, hist, &primary, x, y, rw, rh, cx),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_gauge(
        &self,
        pixmap: &mut Pixmap,
        w: &dashboard::Widget,
        theme: dashboard::Theme,
        accent: dashboard::Color,
        value: Option<f32>,
        primary: &str,
        caption: Option<&str>,
        _x: f32,
        _y: f32,
        rw: f32,
        rh: f32,
        cx: f32,
        cy: f32,
    ) {
        let r = (rw.min(rh) / 2.0 - 4.0).max(1.0);
        let stroke_w = (r * 0.18).clamp(4.0, 24.0);
        // 270deg arc, same convention as the legacy single-gauge renderer.
        stroke_arc(pixmap, cx, cy, r, 135.0, 270.0, 1.0, stroke_w, theme.track);
        let f = frac(value.unwrap_or(w.min), w.min, w.max);
        stroke_arc(pixmap, cx, cy, r, 135.0, 270.0, f, stroke_w, accent);

        let has_caption = w.label && caption.is_some();
        let value_px = (r * 0.55 * w.font_scale).clamp(6.0, 140.0);
        let value_cy = if has_caption { cy - r * 0.12 } else { cy };
        let max_w = (r * 1.7).max(4.0);
        self.draw_text_centered(pixmap, primary, cx, value_cy, value_px, max_w, theme.text);
        if let (true, Some(cap)) = (w.label, caption) {
            let cap_px = (value_px * 0.28).clamp(6.0, 26.0);
            self.draw_text_centered(pixmap, cap, cx, cy + r * 0.55, cap_px, max_w, theme.muted);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_ring(
        &self,
        pixmap: &mut Pixmap,
        w: &dashboard::Widget,
        theme: dashboard::Theme,
        accent: dashboard::Color,
        value: Option<f32>,
        primary: &str,
        caption: Option<&str>,
        _x: f32,
        _y: f32,
        rw: f32,
        rh: f32,
        cx: f32,
        cy: f32,
    ) {
        let r = (rw.min(rh) / 2.0 - 4.0).max(1.0);
        let stroke_w = (r * 0.18).clamp(4.0, 24.0);
        // Full 360deg ring, starting at the top (-90deg).
        stroke_arc(pixmap, cx, cy, r, -90.0, 360.0, 1.0, stroke_w, theme.track);
        let f = frac(value.unwrap_or(w.min), w.min, w.max);
        stroke_arc(pixmap, cx, cy, r, -90.0, 360.0, f, stroke_w, accent);

        let has_caption = w.label && caption.is_some();
        let value_px = (r * 0.55 * w.font_scale).clamp(6.0, 140.0);
        let value_cy = if has_caption { cy - r * 0.12 } else { cy };
        let max_w = (r * 1.7).max(4.0);
        self.draw_text_centered(pixmap, primary, cx, value_cy, value_px, max_w, theme.text);
        if let (true, Some(cap)) = (w.label, caption) {
            let cap_px = (value_px * 0.28).clamp(6.0, 26.0);
            self.draw_text_centered(pixmap, cap, cx, cy + r * 0.55, cap_px, max_w, theme.muted);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_bar(
        &self,
        pixmap: &mut Pixmap,
        w: &dashboard::Widget,
        theme: dashboard::Theme,
        accent: dashboard::Color,
        value: Option<f32>,
        primary: &str,
        caption: Option<&str>,
        x: f32,
        y: f32,
        rw: f32,
        rh: f32,
        cx: f32,
        _cy: f32,
    ) {
        // Outer padding kept inside the rect on every edge -- this, plus
        // clamping `bar_h` to `max_bar_h` below, is what guarantees the bar
        // track/fill never draws outside (or below) `rect`, even for a short
        // cell or a large `font_scale`.
        let pad = (rh * 0.05).clamp(3.0, 10.0);
        let max_bar_h = (rh - pad * 2.0).max(1.0);
        let bar_h = (rh * 0.14 * w.font_scale).clamp(4.0, max_bar_h);
        // Bottom-aligned within the rect, `pad` above the bottom edge; since
        // bar_h <= max_bar_h == rh - 2*pad, bar_y is always >= y + pad.
        let bar_y = y + rh - pad - bar_h;
        let bar_x = x + rw * 0.08;
        let bar_w = (rw * 0.84).max(0.0);
        let radius = bar_h / 2.0;
        fill_rounded_rect(pixmap, bar_x, bar_y, bar_w, bar_h, radius, theme.track);

        let f = frac(value.unwrap_or(w.min), w.min, w.max);
        let fill_w = bar_w * f;
        if fill_w > 0.5 {
            fill_rounded_rect(pixmap, bar_x, bar_y, fill_w, bar_h, radius.min(fill_w / 2.0), accent);
        }

        // Caption + value text share the space above the bar (between the top
        // padding and the bar's top edge), value bottom-aligned just above the
        // bar and caption top-aligned just below the rect's top edge -- a
        // consistent stack regardless of cell size.
        let text_top = y + pad;
        let text_bottom = (bar_y - pad).max(text_top);
        let avail_h = (text_bottom - text_top).max(1.0);
        let max_w = (rw - pad * 2.0).max(4.0);

        let value_px = (avail_h * 0.55 * w.font_scale).clamp(6.0, 72.0);
        let has_caption = w.label && caption.is_some();
        let value_cy = if has_caption {
            (text_bottom - value_px * 0.45).max(text_top + value_px * 0.5)
        } else {
            (text_top + text_bottom) / 2.0
        };
        self.draw_text_centered(pixmap, primary, cx, value_cy, value_px, max_w, theme.text);
        if let (true, Some(cap)) = (w.label, caption) {
            let cap_px = (value_px * 0.42).clamp(6.0, 20.0);
            let cap_cy = text_top + cap_px * 0.6;
            self.draw_text_centered(pixmap, cap, cx, cap_cy, cap_px, max_w, theme.muted);
        }
    }

    fn draw_number(&self, pixmap: &mut Pixmap, w: &dashboard::Widget, theme: dashboard::Theme, primary: &str, caption: Option<&str>, rw: f32, rh: f32, cx: f32, cy: f32) {
        let has_caption = w.label && caption.is_some();
        let value_px = (rh * 0.5 * w.font_scale).clamp(6.0, 180.0);
        let value_cy = if has_caption { cy - rh * 0.12 } else { cy };
        let max_w = (rw - 8.0).max(4.0);
        self.draw_text_centered(pixmap, primary, cx, value_cy, value_px, max_w, theme.text);
        if let (true, Some(cap)) = (w.label, caption) {
            let cap_px = (value_px * 0.28).clamp(6.0, 28.0);
            self.draw_text_centered(pixmap, cap, cx, cy + rh * 0.3, cap_px, max_w, theme.muted);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_sparkline(
        &self,
        pixmap: &mut Pixmap,
        w: &dashboard::Widget,
        theme: dashboard::Theme,
        accent: dashboard::Color,
        hist: &History,
        primary: &str,
        x: f32,
        y: f32,
        rw: f32,
        rh: f32,
        cx: f32,
    ) {
        // Small primary readout at the top of the widget.
        let value_px = (rh * 0.18 * w.font_scale).clamp(6.0, 30.0);
        let max_w = (rw - 8.0).max(4.0);
        self.draw_text_centered(pixmap, primary, cx, y + rh * 0.12, value_px, max_w, theme.text);

        // Baseline near the bottom of the plot area.
        let baseline_y = y + rh * 0.85;
        fill_rect(pixmap, x, baseline_y, rw, 1.0, theme.track);

        let series = hist.series(w.kind);
        if series.len() >= 2 {
            let plot_top = y + rh * 0.28;
            let plot_h = (baseline_y - plot_top).max(1.0);
            let n = series.len();
            let step = rw / (n as f32 - 1.0).max(1.0);
            let mut pb = PathBuilder::new();
            for (i, v) in series.iter().enumerate() {
                let f = frac(*v, w.min, w.max);
                let px = x + step * i as f32;
                let py = plot_top + plot_h * (1.0 - f);
                if i == 0 {
                    pb.move_to(px, py);
                } else {
                    pb.line_to(px, py);
                }
            }
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(tsk_color(accent));
                paint.anti_alias = true;
                let mut stroke = Stroke::default();
                stroke.width = (rh * 0.03).clamp(1.5, 4.0);
                stroke.line_cap = tiny_skia::LineCap::Round;
                stroke.line_join = tiny_skia::LineJoin::Round;
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        // Empty/single-sample history: nothing more to draw -- baseline + label suffice.
    }

    /// Analog clock face: a circular outline (theme.track), 12 tick marks, and
    /// hour/minute hands (theme.text) plus a second hand (accent), all derived
    /// from `snap.time`. Centered in the widget rect; radius is
    /// `min(w, h) / 2` minus a small margin so the face never touches the
    /// rect edges.
    #[allow(clippy::too_many_arguments)]
    fn draw_analog_clock(&self, pixmap: &mut Pixmap, theme: dashboard::Theme, accent: dashboard::Color, snap: &Snapshot, _x: f32, _y: f32, rw: f32, rh: f32, cx: f32, cy: f32) {
        let margin = (rw.min(rh) * 0.04).clamp(2.0, 10.0);
        let r = (rw.min(rh) / 2.0 - margin).max(1.0);

        // Face outline.
        let face_stroke = (r * 0.045).clamp(1.5, 6.0);
        stroke_arc(pixmap, cx, cy, r, 0.0, 360.0, 1.0, face_stroke, theme.track);

        // 12 tick marks, one every 30 degrees.
        let tick_len = r * 0.12;
        let tick_stroke = (r * 0.03).clamp(1.0, 4.0);
        for i in 0..12 {
            let f = i as f32 / 12.0;
            let (x0, y0) = clock_point(cx, cy, f, r - tick_len);
            let (x1, y1) = clock_point(cx, cy, f, r);
            stroke_line(pixmap, x0, y0, x1, y1, tick_stroke, theme.track);
        }

        let hour = snap.time.hour() % 12;
        let minute = snap.time.minute();
        let second = snap.time.second();

        // Hour hand: (hour % 12) / 12 of a turn, plus the within-hour minute
        // fraction so it creeps smoothly between hour ticks.
        let hour_frac = (hour as f32 + minute as f32 / 60.0) / 12.0;
        let minute_frac = (minute as f32 + second as f32 / 60.0) / 60.0;
        let second_frac = second as f32 / 60.0;

        let (hx, hy) = clock_point(cx, cy, hour_frac, r * 0.5);
        stroke_line(pixmap, cx, cy, hx, hy, (r * 0.07).clamp(2.0, 8.0), theme.text);

        let (mx, my) = clock_point(cx, cy, minute_frac, r * 0.75);
        stroke_line(pixmap, cx, cy, mx, my, (r * 0.05).clamp(1.5, 6.0), theme.text);

        let (sx, sy) = clock_point(cx, cy, second_frac, r * 0.85);
        stroke_line(pixmap, cx, cy, sx, sy, (r * 0.02).clamp(1.0, 3.0), accent);
    }
}

pub fn compose_media(frame: &image::RgbaImage, p: &crate::media::Placement, fill: [u8; 4]) -> Vec<u8> {
    let mut canvas = image::RgbaImage::from_pixel(crate::media::D, crate::media::D, image::Rgba(fill));
    let (sx, sy, sw, sh) = p.src;
    let (dx, dy, dw, dh) = p.dst;
    if sw > 0 && sh > 0 && dw > 0 && dh > 0 {
        // clamp crop to frame bounds
        let (fw, fh) = frame.dimensions();
        let cw = sw.min(fw.saturating_sub(sx));
        let ch = sh.min(fh.saturating_sub(sy));
        if cw > 0 && ch > 0 {
            let cropped = image::imageops::crop_imm(frame, sx, sy, cw, ch).to_image();
            let scaled = image::imageops::resize(&cropped, dw, dh, image::imageops::FilterType::Triangle);
            image::imageops::overlay(&mut canvas, &scaled, dx as i64, dy as i64);
        }
    }
    canvas.into_raw()
}

/// Pre-composes every source `MediaFrame` of an animated GIF to the device's fixed
/// 320x320 RGBA output size, once, under the given `fit`/`zoom`/`pan`. Each source
/// frame's `placement` is computed independently (frames can vary in size), then
/// `compose_media` crops+resizes it exactly as the per-frame playback path used to.
///
/// This exists so playback (pipeline device output and the GUI preview alike) can
/// encode/display these pre-composed bytes directly every tick instead of re-running
/// the crop+resize on the full-resolution source frame every single tick -- for a
/// large GIF that per-frame resize was the actual playback-rate bottleneck. Doing the
/// resize once here (and letting the caller drop the source frames afterward) trades a
/// one-time cost at load time for cheap encode-only playback and lower steady-state RAM.
pub fn compose_frames(
    frames: &[crate::media::MediaFrame],
    fit: crate::media::Fit,
    zoom: f32,
    pan: [f32; 2],
) -> Vec<(Vec<u8>, u32)> {
    frames
        .iter()
        .map(|f| {
            let (sw, sh) = f.img.dimensions();
            let p = crate::media::placement(sw, sh, fit, zoom, pan);
            let rgba = compose_media(&f.img, &p, [8, 10, 18, 255]);
            (rgba, f.delay_ms)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_clamps_and_maps() {
        assert!((gauge_sweep(30.0, 30.0, 90.0) - 0.0).abs() < 1e-6);
        assert!((gauge_sweep(90.0, 30.0, 90.0) - 1.0).abs() < 1e-6);
        assert!((gauge_sweep(60.0, 30.0, 90.0) - 0.5).abs() < 1e-6);
        assert_eq!(gauge_sweep(200.0, 30.0, 90.0), 1.0); // clamp high
        assert_eq!(gauge_sweep(0.0, 30.0, 90.0), 0.0); // clamp low
    }

    #[test]
    #[ignore]
    fn renders_sample_png() {
        use crate::sensors::Snapshot;
        use chrono::Local;

        let snapshot = Snapshot {
            gpu_temp_c: Some(52),
            gpu_usage_pct: Some(37),
            cpu_usage_pct: 18,
            ram_pct: Some(41),
            time: Local::now(),
        };

        let mut renderer = new().expect("renderer init");
        let rgba = renderer.dashboard(&snapshot);
        assert_eq!(rgba.len(), (W * H * 4) as usize);

        let img: image::RgbaImage = image::ImageBuffer::from_raw(W, H, rgba)
            .expect("rgba buffer matches dimensions");
        let out_path = r"C:\Users\Gavril\AppData\Local\Temp\claude\C--Users-Gavril-Documents-Sandbox\ebda49d4-a528-451a-97d5-e28ba1191385\scratchpad\dashboard_sample.png";
        img.save(out_path).expect("write sample png");
    }
}

#[cfg(test)]
mod compose_tests {
    use super::*;
    use crate::media::{placement, Fit, MediaFrame, D};

    #[test]
    fn compose_frames_produces_320_rgba_frames_with_delays_preserved() {
        let frames = vec![
            MediaFrame { img: image::RgbaImage::from_pixel(100, 100, image::Rgba([1, 2, 3, 255])), delay_ms: 50 },
            MediaFrame { img: image::RgbaImage::from_pixel(100, 100, image::Rgba([4, 5, 6, 255])), delay_ms: 80 },
        ];
        let out = compose_frames(&frames, Fit::Cover, 1.0, [0.0, 0.0]);
        assert_eq!(out.len(), 2);
        for (rgba, _delay) in &out {
            assert_eq!(rgba.len(), (D * D * 4) as usize);
        }
        assert_eq!(out[0].1, 50);
        assert_eq!(out[1].1, 80);
        // Center pixel of each pre-composed frame should carry that frame's color
        // (Fit::Cover fills the whole 320x320 canvas from the square source).
        let c = ((D / 2 * D + D / 2) * 4) as usize;
        assert_eq!(&out[0].0[c..c + 3], &[1, 2, 3]);
        assert_eq!(&out[1].0[c..c + 3], &[4, 5, 6]);
    }

    #[test]
    fn full_cover_fills_canvas_with_image_color() {
        let src = image::RgbaImage::from_pixel(100, 100, image::Rgba([12, 34, 56, 255]));
        let p = placement(100, 100, Fit::Cover, 1.0, [0.0, 0.0]);
        let out = compose_media(&src, &p, [0, 0, 0, 255]);
        assert_eq!(out.len(), (D * D * 4) as usize);
        // center pixel should be the image color
        let c = ((D / 2 * D + D / 2) * 4) as usize;
        assert_eq!(&out[c..c + 3], &[12, 34, 56]);
    }

    #[test]
    fn contain_shows_fill_in_letterbox() {
        // wide image -> vertical letterbox with fill color at very top row
        let src = image::RgbaImage::from_pixel(320, 80, image::Rgba([200, 200, 200, 255]));
        let p = placement(320, 80, Fit::Contain, 1.0, [0.0, 0.0]);
        let out = compose_media(&src, &p, [7, 8, 9, 255]);
        assert_eq!(&out[0..3], &[7, 8, 9]); // top-left is letterbox fill
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::dashboard::{Dashboard, Rect, Theme, Viz, Widget, WidgetKind, CANVAS};
    use chrono::Local;

    fn test_snapshot() -> Snapshot {
        Snapshot {
            gpu_temp_c: Some(54),
            gpu_usage_pct: Some(37),
            cpu_usage_pct: 18,
            ram_pct: Some(62),
            time: Local::now(),
        }
    }

    // --- frac ---------------------------------------------------------

    #[test]
    fn frac_bounds_and_clamps() {
        assert!((frac(30.0, 30.0, 90.0) - 0.0).abs() < 1e-6);
        assert!((frac(90.0, 30.0, 90.0) - 1.0).abs() < 1e-6);
        assert!((frac(60.0, 30.0, 90.0) - 0.5).abs() < 1e-6);
        assert_eq!(frac(200.0, 30.0, 90.0), 1.0); // clamp high
        assert_eq!(frac(-10.0, 30.0, 90.0), 0.0); // clamp low
    }

    #[test]
    fn frac_degenerate_range_is_zero_not_nan() {
        assert_eq!(frac(50.0, 10.0, 10.0), 0.0); // max == min
        assert_eq!(frac(50.0, 90.0, 30.0), 0.0); // max < min
    }

    // --- History --------------------------------------------------------

    #[test]
    fn history_push_and_series_preserve_fifo_order() {
        let mut h = History::new();
        for i in 0..5 {
            h.push(WidgetKind::CpuUsage, i as f32);
        }
        assert_eq!(h.series(WidgetKind::CpuUsage), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn history_caps_and_drops_oldest_first() {
        let mut h = History::new();
        for i in 0..70 {
            h.push(WidgetKind::GpuTemp, i as f32);
        }
        let s = h.series(WidgetKind::GpuTemp);
        assert_eq!(s.len(), 60);
        assert_eq!(s[0], 10.0); // 70 pushes, cap 60 -> oldest 10 dropped
        assert_eq!(*s.last().unwrap(), 69.0);
    }

    #[test]
    fn history_series_is_per_kind_independent() {
        let mut h = History::new();
        h.push(WidgetKind::CpuUsage, 1.0);
        h.push(WidgetKind::GpuTemp, 99.0);
        assert_eq!(h.series(WidgetKind::CpuUsage), vec![1.0]);
        assert_eq!(h.series(WidgetKind::GpuTemp), vec![99.0]);
        assert!(h.series(WidgetKind::RamUsage).is_empty());
    }

    #[test]
    fn history_empty_series_is_empty_not_panicking() {
        let h = History::new();
        assert!(h.series(WidgetKind::Text).is_empty());
    }

    // --- render_dashboard -------------------------------------------------

    #[test]
    fn render_dashboard_default_returns_full_frame() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let hist = History::new();
        let dash = Dashboard::default();
        let out = renderer.render_dashboard(&dash, &snap, &hist);
        assert_eq!(out.len(), (CANVAS * CANVAS * 4) as usize);
    }

    #[test]
    fn render_dashboard_all_templates_render_without_panic() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let mut hist = History::new();
        for kind in [WidgetKind::GpuTemp, WidgetKind::GpuUsage, WidgetKind::CpuUsage, WidgetKind::RamUsage] {
            for i in 0..10 {
                hist.push(kind, i as f32 * 3.0);
            }
        }
        for (_name, dash) in crate::dashboard::templates() {
            let out = renderer.render_dashboard(&dash, &snap, &hist);
            assert_eq!(out.len(), (CANVAS * CANVAS * 4) as usize);
        }
    }

    #[test]
    fn render_dashboard_accent_override_and_empty_sparkline_history_no_panic() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let hist = History::new(); // deliberately empty -- exercises the empty-series path

        let mut accented = Widget::new(WidgetKind::GpuTemp);
        accented.rect = Rect::new(0, 0, 150, 150);
        accented.accent = Some([255, 20, 60]);

        let mut spark = Widget::new(WidgetKind::CpuUsage);
        spark.viz = Viz::Sparkline;
        spark.rect = Rect::new(150, 0, 170, 150);

        let dash = Dashboard {
            theme: Theme::default(),
            widgets: vec![accented, spark],
        };
        let out = renderer.render_dashboard(&dash, &snap, &hist);
        assert_eq!(out.len(), (CANVAS * CANVAS * 4) as usize);
    }

    #[test]
    fn render_dashboard_widget_at_canvas_edge_no_panic() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let hist = History::new();

        let mut edge = Widget::new(WidgetKind::RamUsage);
        edge.viz = Viz::Ring;
        edge.rect = Rect::new(CANVAS - 20, CANVAS - 20, 20, 20); // flush against bottom-right corner

        let dash = Dashboard {
            theme: Theme::default(),
            widgets: vec![edge],
        };
        let out = renderer.render_dashboard(&dash, &snap, &hist);
        assert_eq!(out.len(), (CANVAS * CANVAS * 4) as usize);
    }

    #[test]
    fn render_dashboard_zero_size_rect_no_panic() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let hist = History::new();

        let mut zero = Widget::new(WidgetKind::Text);
        zero.rect = Rect::new(10, 10, 0, 0);

        let dash = Dashboard {
            theme: Theme::default(),
            widgets: vec![zero],
        };
        let out = renderer.render_dashboard(&dash, &snap, &hist);
        assert_eq!(out.len(), (CANVAS * CANVAS * 4) as usize);
    }

    /// Not run by default (visual review only). Renders the default "Gauge
    /// Focus" dashboard and the "Stats Grid" template to PNGs in the
    /// scratchpad for eyeballing.
    #[test]
    #[ignore]
    fn renders_sample_dashboards_png() {
        let renderer = new().expect("renderer init");
        let snap = test_snapshot();
        let mut hist = History::new();
        for kind in [WidgetKind::GpuTemp, WidgetKind::GpuUsage, WidgetKind::CpuUsage, WidgetKind::RamUsage] {
            for i in 0..40 {
                let phase = i as f32 * 0.25 + kind_idx(kind) as f32;
                let v = 50.0 + 30.0 * phase.sin();
                hist.push(kind, v);
            }
        }

        let templates = crate::dashboard::templates();
        let gauge_focus = templates.iter().find(|(name, _)| *name == "Gauge Focus").expect("Gauge Focus template").1.clone();
        let stats_grid = templates.iter().find(|(name, _)| *name == "Stats Grid").expect("Stats Grid template").1.clone();

        let save = |dash: &Dashboard, path: &str| {
            let rgba = renderer.render_dashboard(dash, &snap, &hist);
            assert_eq!(rgba.len(), (CANVAS * CANVAS * 4) as usize);
            let img: image::RgbaImage = image::ImageBuffer::from_raw(CANVAS, CANVAS, rgba).expect("rgba buffer matches dimensions");
            img.save(path).expect("write sample png");
        };

        save(
            &gauge_focus,
            r"C:\Users\Gavril\AppData\Local\Temp\claude\C--Users-Gavril-Documents-Sandbox\ebda49d4-a528-451a-97d5-e28ba1191385\scratchpad\dash_gaugefocus.png",
        );
        save(
            &stats_grid,
            r"C:\Users\Gavril\AppData\Local\Temp\claude\C--Users-Gavril-Documents-Sandbox\ebda49d4-a528-451a-97d5-e28ba1191385\scratchpad\dash_statsgrid.png",
        );
    }
}
