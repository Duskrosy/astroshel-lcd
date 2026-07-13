use crate::sensors::Snapshot;
use anyhow::Result;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

const W: u32 = 320;
const H: u32 = 320;
const ACCENT: (u8, u8, u8) = (0, 224, 150);
const TRACK: (u8, u8, u8) = (38, 42, 58);
const BG: (u8, u8, u8) = (8, 10, 18);
const LABEL: (u8, u8, u8) = (150, 160, 180);
const VALUE: (u8, u8, u8) = (240, 245, 255);

/// Map a value into a 0.0..=1.0 fraction of the gauge arc (clamped).
pub fn gauge_sweep(value: f32, min: f32, max: f32) -> f32 {
    ((value - min) / (max - min)).clamp(0.0, 1.0)
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
    use crate::media::{placement, Fit, D};

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
