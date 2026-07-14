use anyhow::{Context, Result};
use std::path::Path;

pub const D: u32 = 320;

pub struct MediaFrame {
    pub img: image::RgbaImage,
    pub delay_ms: u32,
}

pub enum Media {
    Static(image::RgbaImage),
    Animated(Vec<MediaFrame>),
}

impl Media {
    pub fn source_size(&self) -> (u32, u32) {
        match self {
            Media::Static(i) => i.dimensions(),
            Media::Animated(f) => f.first().map(|x| x.img.dimensions()).unwrap_or((1, 1)),
        }
    }
}

/// Safety guard against oversized source media: a huge GIF/image decoded at full
/// source resolution can balloon RAM (a large GIF's frames alone reached ~370 MB) and
/// raise per-frame compose/encode cost. Downscaling here (once, at decode time) caps
/// that regardless of fit/zoom settings applied later. 640 keeps ample detail for a
/// 320x320 LCD output while bounding worst-case memory/CPU.
const MAX_SOURCE_SIDE: u32 = 640;

/// Downscales `img` in place (preserving aspect ratio) so its longest side is at most
/// `MAX_SOURCE_SIDE`, leaving smaller images untouched.
fn downscale_if_needed(img: image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= MAX_SOURCE_SIDE {
        return img;
    }
    let scale = MAX_SOURCE_SIDE as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

/// Extensions handled by the one-shot video *importer* (`crate::video::import_to_cache`)
/// rather than `load`'s image/GIF decoders. Video is no longer a live `Media` variant --
/// it's imported once to a cached `.lcdv` (see `crate::cache`) and played back as
/// pre-encoded packets, same as a GIF. Shared by callers (GUI/pipeline) that need to know
/// whether a media path is a video *without* attempting to decode it as an image.
const VIDEO_EXTS: [&str; 12] =
    ["mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "ts", "mpg", "mpeg", "m2ts"];

/// True if `path`'s extension is one routed to the video importer rather than `load`'s
/// image/GIF decoders.
pub fn is_video_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map(|e| VIDEO_EXTS.iter().any(|v| e.eq_ignore_ascii_case(v))).unwrap_or(false)
}

/// Decodes an image or GIF file into a `Media`. Video files are no longer handled here --
/// they're imported once via `crate::video::import_to_cache` into a cached `.lcdv` -- so a
/// video-extension path bails immediately with a friendly, non-panicking error instead of
/// being misread as a still image.
pub fn load(path: &Path) -> Result<Media> {
    if is_video_path(path) {
        anyhow::bail!("video files are imported separately (Add Media > video import)");
    }
    let is_gif = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gif"))
        .unwrap_or(false);
    if is_gif {
        use image::AnimationDecoder;
        let file = std::fs::File::open(path).with_context(|| "open gif")?;
        let dec = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
            .context("gif decode")?;
        let mut out = Vec::new();
        for fr in dec.into_frames().collect_frames().context("gif frames")? {
            let delay = fr.delay().numer_denom_ms();
            let delay_ms = (delay.0 / delay.1.max(1)).max(20);
            out.push(MediaFrame {
                img: downscale_if_needed(fr.into_buffer()),
                delay_ms,
            });
        }
        if out.is_empty() {
            anyhow::bail!("gif had no frames");
        }
        Ok(Media::Animated(out))
    } else {
        let img = image::open(path).with_context(|| "open image")?.to_rgba8();
        Ok(Media::Static(downscale_if_needed(img)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit { Contain, Cover, Stretch, Manual }

impl Fit {
    pub fn from_str(s: &str) -> Fit {
        match s.to_ascii_lowercase().as_str() {
            "contain" => Fit::Contain,
            "stretch" => Fit::Stretch,
            "manual" => Fit::Manual,
            _ => Fit::Cover,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self { Fit::Contain=>"contain", Fit::Cover=>"cover", Fit::Stretch=>"stretch", Fit::Manual=>"manual" }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement { pub src: (u32,u32,u32,u32), pub dst: (u32,u32,u32,u32) }

pub fn placement(sw: u32, sh: u32, fit: Fit, zoom: f32, pan: [f32;2]) -> Placement {
    if sw == 0 || sh == 0 {
        return Placement { src: (0,0,sw.max(1),sh.max(1)), dst: (0,0,D,D) };
    }
    match fit {
        Fit::Stretch => Placement { src: (0,0,sw,sh), dst: (0,0,D,D) },
        Fit::Contain => {
            let scale = (D as f32 / sw as f32).min(D as f32 / sh as f32);
            let dw = (sw as f32 * scale).round() as u32;
            let dh = (sh as f32 * scale).round() as u32;
            Placement { src: (0,0,sw,sh), dst: ((D-dw)/2, (D-dh)/2, dw, dh) }
        }
        Fit::Cover => {
            let side = sw.min(sh);
            Placement { src: ((sw-side)/2, (sh-side)/2, side, side), dst: (0,0,D,D) }
        }
        Fit::Manual => {
            let base = sw.min(sh);                 // cover-square side at zoom 1
            let z = zoom.max(1.0);
            let crop = ((base as f32 / z).round() as u32).max(1).min(base);
            let rx = sw - crop;                    // full horizontal pan range
            let ry = sh - crop;                    // full vertical pan range
            let cx0 = rx / 2;
            let cy0 = ry / 2;
            let px = (pan[0].clamp(-1.0,1.0) * (rx as f32 / 2.0)).round() as i64;
            let py = (pan[1].clamp(-1.0,1.0) * (ry as f32 / 2.0)).round() as i64;
            let x = (cx0 as i64 + px).clamp(0, rx as i64) as u32;
            let y = (cy0 as i64 + py).clamp(0, ry as i64) as u32;
            Placement { src: (x, y, crop, crop), dst: (0,0,D,D) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretch_uses_whole_source_and_full_dest() {
        let p = placement(640, 480, Fit::Stretch, 1.0, [0.0, 0.0]);
        assert_eq!(p.src, (0, 0, 640, 480));
        assert_eq!(p.dst, (0, 0, 320, 320));
    }

    #[test]
    fn contain_letterboxes_landscape() {
        // 640x480 (4:3) into 320 square: scale=320/640=0.5 -> 320x240, centered vertically
        let p = placement(640, 480, Fit::Contain, 1.0, [0.0, 0.0]);
        assert_eq!(p.src, (0, 0, 640, 480));
        assert_eq!(p.dst, (0, 40, 320, 240)); // (320-240)/2 = 40 top offset
    }

    #[test]
    fn cover_center_crops_square_from_landscape() {
        // 640x480 -> largest centered square is 480x480 at x=80
        let p = placement(640, 480, Fit::Cover, 1.0, [0.0, 0.0]);
        assert_eq!(p.src, (80, 0, 480, 480));
        assert_eq!(p.dst, (0, 0, 320, 320));
    }

    #[test]
    fn manual_zoom_shrinks_crop_centered() {
        // base square side=480; zoom 2.0 -> crop 240x240 centered in the 480 square (which sits at x=80)
        let p = placement(640, 480, Fit::Manual, 2.0, [0.0, 0.0]);
        assert_eq!(p.src.2, 240);
        assert_eq!(p.src.3, 240);
        // centered: base square x=80,y=0,side=480 -> inner 240 crop at x=80+120=200, y=120
        assert_eq!(p.src.0, 200);
        assert_eq!(p.src.1, 120);
        assert_eq!(p.dst, (0, 0, 320, 320));
    }

    #[test]
    fn manual_pan_clamps_inside_source() {
        // extreme right pan must not exceed source bounds
        let p = placement(640, 480, Fit::Manual, 2.0, [1.0, 0.0]);
        assert!(p.src.0 + p.src.2 <= 640);
        assert!(p.src.1 + p.src.3 <= 480);
    }

    #[test]
    fn manual_pan_works_along_long_axis_at_zoom_1() {
        // 640x480 landscape at zoom=1: crop is the 480x480 cover-square, sliding
        // horizontally across the full 640-wide source. Panning left vs right
        // must move the crop, and it must stay within source bounds.
        let left = placement(640, 480, Fit::Manual, 1.0, [-1.0, 0.0]);
        let right = placement(640, 480, Fit::Manual, 1.0, [1.0, 0.0]);
        assert_ne!(left.src.0, right.src.0);
        assert!(left.src.0 + left.src.2 <= 640);
        assert!(right.src.0 + right.src.2 <= 640);
        assert!(left.src.1 + left.src.3 <= 480);
        assert!(right.src.1 + right.src.3 <= 480);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_static_png() {
        let dir = std::env::temp_dir().join("astro_media_png");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        let img = image::RgbaImage::from_pixel(10, 8, image::Rgba([255, 0, 0, 255]));
        img.save(&path).unwrap();
        let m = load(&path).unwrap();
        match m {
            Media::Static(i) => assert_eq!(i.dimensions(), (10, 8)),
            _ => panic!("expected static"),
        }
    }

    #[test]
    fn loads_animated_gif_with_frames() {
        let dir = std::env::temp_dir().join("astro_media_gif");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.gif");
        // encode a 2-frame gif
        let mut bytes: Vec<u8> = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut bytes);
            for c in [[0u8, 0, 255, 255], [0, 255, 0, 255]] {
                let fr = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(6, 6, image::Rgba(c)),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(60, 1),
                );
                enc.encode_frame(fr).unwrap();
            }
        }
        std::fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        let m = load(&path).unwrap();
        match m {
            Media::Animated(f) => {
                assert_eq!(f.len(), 2);
                assert!(f[0].delay_ms >= 20);
            }
            _ => panic!("expected animated"),
        }
    }

    #[test]
    fn missing_file_errors_no_panic() {
        assert!(load(std::path::Path::new("Z:/nope.png")).is_err());
    }

    #[test]
    fn video_path_returns_friendly_error_no_panic() {
        // `media::load` no longer decodes video at all (see `crate::video::import_to_cache`
        // for the one-shot importer) -- a video-extension path must bail immediately with a
        // friendly message and never panic, regardless of whether the bytes are actually a
        // valid video (garbage bytes never even reach a decoder now).
        let dir = std::env::temp_dir().join("astro_media_bogus_mp4");
        std::fs::create_dir_all(&dir).unwrap();

        let path1 = dir.join("bogus_tiny.mp4");
        std::fs::write(&path1, [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 0, 0]).unwrap();

        let path2 = dir.join("bogus_random.mp4");
        let mut buf = vec![0xFFu8; 512];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((i * 2654435761u32.wrapping_add(i as u32) as usize) % 256) as u8;
        }
        std::fs::write(&path2, &buf).unwrap();

        for path in [&path1, &path2] {
            let result = std::panic::catch_unwind(|| load(path));
            match result {
                Ok(Err(e)) => {
                    let msg = format!("{e:#}");
                    assert!(
                        msg.contains("imported separately"),
                        "expected friendly message, got: {msg}"
                    );
                }
                Ok(Ok(_)) => panic!("video path should not decode as image/gif media: {path:?}"),
                Err(_) => panic!("load() panicked on video-extension path {path:?}"),
            }
        }
    }
}
