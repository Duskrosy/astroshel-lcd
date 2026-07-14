//! One-shot video import: shells out to a bundled `ffmpeg` binary (see
//! `vendor/ffmpeg.exe`, staged alongside the app at build/install time) to
//! decode ANY container/codec ffmpeg supports (H.264, H.265/HEVC, VP9, AV1,
//! ...) to raw RGBA frames on its stdout pipe -- ONCE, at import time. Each
//! decoded source frame is composed to the device's fixed 320x320 output
//! (`crate::render::compose_media`) and re-encoded to H.264
//! (`crate::encode::StreamEncoder`), and the resulting packets are cached as a
//! `.lcdv` file (`crate::cache`) for later send-only playback -- there is no
//! live/persistent ffmpeg process; it decodes the whole source and exits.
//!
//! ffmpeg is located via (in order): the `ASTRO_FFMPEG` env var (dev/test
//! override), `ffmpeg.exe` next to the running exe (the normal bundled/
//! installed case), or the bare name `"ffmpeg"` resolved via PATH by
//! `Command` itself.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, bail, Result};

/// Windows `CREATE_NO_WINDOW` process creation flag: suppresses the console
/// window that would otherwise flash briefly every time a bundled `ffmpeg.exe`
/// (a console subsystem binary) is spawned from this GUI app -- applied to
/// every `Command` that runs ffmpeg below (both the probe and the decode spawn).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Longest source side an incoming video is scaled to fit within (matches
/// `media::MAX_SOURCE_SIDE`): caps ffmpeg's per-frame RGBA output size (and
/// thus this process's steady-state memory/CPU) while leaving ample detail
/// for the device's fixed 320x320 output.
const MAX_SOURCE_SIDE: f32 = 640.0;

/// Resolves the ffmpeg binary to invoke: `ASTRO_FFMPEG` env var, then
/// `ffmpeg.exe` next to the running exe (the bundled/installed layout), then
/// falls back to the bare name `"ffmpeg"` (resolved via PATH by `Command`
/// itself -- if that also fails to spawn, callers surface a clear error).
fn ffmpeg_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ASTRO_FFMPEG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ffmpeg.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    Some(PathBuf::from("ffmpeg"))
}

/// Parses the first `Video:` line of `ffmpeg -i <path>`'s stderr (ffmpeg's
/// stream-info dump) for the source's `<width>x<height>` and its `fps`/`tbr`
/// token, e.g.:
/// `  Stream #0:0[0x1](und): Video: hevc (Main) (hvc1 / 0x31637668),
/// yuv420p(tv, bt709), 1080x1080 [SAR 1:1 DAR 1:1], 9896 kb/s, 30 fps, 30
/// tbr, 30 tbn (default)` -> `(1080, 1080, 30.0)`. Returns `None` if no line
/// contains `Video:`, or if no `WxH` token is found on it. fps falls back to
/// 25.0 (clamped to `1.0..=30.0`) if no `fps`/`tbr` token is found.
fn parse_video_line(stderr: &str) -> Option<(u32, u32, f32)> {
    let line = stderr.lines().find(|l| l.contains("Video:"))?;

    // Tokenize on ffmpeg's usual punctuation so `1080x1080` (or `30 fps`)
    // comes out as standalone tokens even amid `[SAR 1:1 DAR 1:1]`, `(hvc1 /
    // 0x31637668)`, `yuv420p(tv, bt709)`, etc.
    let tokens: Vec<&str> = line
        .split(|c: char| c == ',' || c == ' ' || c == '[' || c == ']' || c == '(' || c == ')')
        .filter(|t| !t.is_empty())
        .collect();

    let is_dim = |s: &str| s.len() >= 2 && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());

    let mut dims: Option<(u32, u32)> = None;
    for tok in &tokens {
        if let Some(pos) = tok.find('x') {
            let (w_str, h_str) = (&tok[..pos], &tok[pos + 1..]);
            if is_dim(w_str) && is_dim(h_str) {
                if let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>()) {
                    dims = Some((w, h));
                    break;
                }
            }
        }
    }
    let (w, h) = dims?;

    let mut fps: Option<f32> = None;
    for (i, tok) in tokens.iter().enumerate() {
        if (*tok == "fps" || *tok == "tbr") && i > 0 {
            if let Ok(f) = tokens[i - 1].parse::<f32>() {
                fps = Some(f);
                break;
            }
        }
    }
    let fps = fps.unwrap_or(25.0).clamp(1.0, 30.0);
    Some((w, h, fps))
}

/// Parses the `Duration: HH:MM:SS.ss` line of `ffmpeg -i <path>`'s stderr
/// stream-info dump, returning the duration in seconds. Returns `None` if no
/// `Duration:` line is present or its timestamp doesn't parse (e.g. `N/A` for
/// a live/streamed source) -- callers treat that as `0.0`.
fn parse_duration_secs(stderr: &str) -> Option<f32> {
    let line = stderr.lines().find(|l| l.trim_start().starts_with("Duration:"))?;
    let after = line.split("Duration:").nth(1)?;
    let ts = after.split(',').next()?.trim();
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f32 = parts[0].trim().parse().ok()?;
    let m: f32 = parts[1].trim().parse().ok()?;
    let s: f32 = parts[2].trim().parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Scales `(sw, sh)` to fit within `MAX_SOURCE_SIDE` (preserving aspect,
/// never upscaling), then rounds both dims up to the nearest even number
/// (ffmpeg's `-vf scale=`/`rawvideo rgba` pipeline wants even dims), clamped
/// to a minimum of 2.
fn fit_even(sw: u32, sh: u32) -> (u32, u32) {
    let long = (sw.max(sh) as f32).max(1.0);
    let scale = if long > MAX_SOURCE_SIDE { MAX_SOURCE_SIDE / long } else { 1.0 };
    let even = |v: f32| {
        let mut n = v.round() as i64;
        if n % 2 != 0 {
            n += 1;
        }
        n.max(2) as u32
    };
    (even(sw as f32 * scale), even(sh as f32 * scale))
}

/// Probes `path` with ffmpeg (never spawns a persistent process -- `ffmpeg -i`
/// with no output just dumps stream info to stderr and exits non-zero, which
/// is expected here, not a failure signal): returns `(width, height, fps,
/// duration_secs)` of its first video stream. `duration_secs` is `0.0` if no
/// parseable `Duration:` line is present.
pub fn probe(path: &Path) -> Result<(u32, u32, f32, f32)> {
    let ff = ffmpeg_path().unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let probe = Command::new(&ff)
        .args(["-hide_banner", "-i"])
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let probe = match probe {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("ffmpeg not found — bundle ffmpeg.exe next to the app");
        }
        Err(e) => return Err(anyhow!("failed to run ffmpeg: {e}")),
    };
    let stderr = String::from_utf8_lossy(&probe.stderr);
    let (w, h, fps) = parse_video_line(&stderr).ok_or_else(|| anyhow!("no video stream"))?;
    let duration = parse_duration_secs(&stderr).unwrap_or(0.0);
    Ok((w, h, fps, duration))
}

/// Owns the decode-phase ffmpeg child and guarantees it is killed and reaped
/// on every exit path out of `import_to_cache` -- including early `?`/`bail!`
/// returns from mid-decode errors -- by killing+waiting on it in `Drop`.
/// Without this, an error partway through the read loop (e.g. a bad frame or
/// an encoder failure) used to skip the happy-path `child.wait()` entirely
/// and leak a zombie ffmpeg process.
struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // `kill` errors (e.g. "already exited") are expected on the normal
        // EOF path and are not actionable here; `wait` reaps the process
        // either way so no zombie lingers.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drains `pipe` to EOF on a background thread, keeping only the trailing
/// ~`KEEP_BYTES` of output in `tail`. Continuously draining the pipe (rather
/// than reading it only after ffmpeg exits) means a chatty ffmpeg can never
/// fill the OS pipe buffer and stall; bounding the retained text means a
/// long-running decode can't grow this without bound. Used so a decode
/// failure (or an unexpected 0-frame result) can be reported with ffmpeg's
/// own diagnostic output instead of being silently swallowed.
fn drain_stderr_tail(mut pipe: impl Read, tail: Arc<Mutex<String>>) {
    const KEEP_BYTES: usize = 4096;
    let mut buf = [0u8; 4096];
    let mut acc = String::new();
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if acc.len() > KEEP_BYTES {
                    let excess = acc.len() - KEEP_BYTES;
                    let cut = acc
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i >= excess)
                        .unwrap_or(acc.len());
                    acc.drain(..cut);
                }
                if let Ok(mut guard) = tail.lock() {
                    *guard = acc.clone();
                }
            }
        }
    }
}

/// One-shot video import: probes `src`, spawns ffmpeg exactly ONCE (no `-re`,
/// no `-stream_loop` -- it decodes the whole source and exits at EOF) to
/// decode+scale it to raw RGBA frames, and for each source frame: composes it
/// to the device's fixed 320x320 output under `fit`/`zoom`/`pan`
/// (`crate::render::compose_media`) and encodes it via a single
/// `crate::encode::StreamEncoder` (targeting `bitrate_kbps`), appending the
/// resulting `(packet, delay_ms)` to the cache's frame list. `progress(done,
/// total_est)` is invoked once per decoded frame (`total_est` is estimated
/// from `duration * fps`, at least 1 -- ffmpeg doesn't report an exact frame
/// count up front). Only one source RGBA frame is held at a time (plus the
/// growing packet vec), keeping memory light regardless of source length.
///
/// Returns the cached video (ready for `crate::cache::write_lcdv`) and the
/// first composed 320x320 frame as a thumbnail image.
pub fn import_to_cache(
    src: &Path,
    fit: crate::media::Fit,
    zoom: f32,
    pan: [f32; 2],
    bitrate_kbps: u32,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(crate::cache::CachedVideo, image::RgbaImage)> {
    let (sw, sh, fps, duration) = probe(src)?;
    let total_est = ((duration * fps).round() as usize).max(1);
    let (ow, oh) = fit_even(sw, sh);

    let ff = ffmpeg_path().unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let child = Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(src)
        .args([
            "-an",
            "-vf",
            &format!("scale={ow}:{oh}"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("ffmpeg not found — bundle ffmpeg.exe next to the app")
            } else {
                anyhow!("failed to spawn ffmpeg: {e}")
            }
        })?;
    // From this point on `guard` owns the child: whether we return `Ok`
    // below, bail out early, or propagate an error via `?`, `guard`'s `Drop`
    // runs as this function unwinds/returns and kills+reaps ffmpeg -- the
    // child can never outlive `import_to_cache`.
    let mut guard = ChildGuard { child };

    let mut stdout =
        guard.child.stdout.take().ok_or_else(|| anyhow!("ffmpeg stdout not piped"))?;
    let stderr_pipe =
        guard.child.stderr.take().ok_or_else(|| anyhow!("ffmpeg stderr not piped"))?;

    // Drain ffmpeg's stderr on a background thread so decode-phase failures
    // are diagnosable (previously `Stdio::null()` discarded it entirely) and
    // so a chatty ffmpeg can't stall by filling the pipe buffer.
    let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let stderr_tail_writer = Arc::clone(&stderr_tail);
    let stderr_thread = thread::spawn(move || drain_stderr_tail(stderr_pipe, stderr_tail_writer));

    let frame_len = (ow as usize) * (oh as usize) * 4;
    // Guard against a zero/negative/NaN fps (shouldn't happen -- `probe`
    // already clamps to 1.0..=30.0 -- but this is the last line of defense
    // before a division) collapsing the delay math into 0/NaN/garbage.
    let safe_fps = if fps.is_finite() && fps >= 1.0 { fps } else { 1.0 };
    let delay_ms = ((1000.0 / safe_fps).round() as u32).max(40);

    let mut stream_enc = crate::encode::new_stream_encoder(320, 320, bitrate_kbps)?;
    let mut frames: Vec<(Vec<u8>, u32)> = Vec::new();
    let mut thumbnail: Option<image::RgbaImage> = None;
    let mut buf = vec![0u8; frame_len];
    let mut done = 0usize;

    loop {
        if stdout.read_exact(&mut buf).is_err() {
            break; // EOF (or a short final read) -- ffmpeg is done decoding.
        }
        let Some(img) = image::RgbaImage::from_raw(ow, oh, buf.clone()) else {
            break;
        };
        let p = crate::media::placement(ow, oh, fit, zoom, pan);
        let rgba320 = crate::render::compose_media(&img, &p, [8, 10, 18, 255]);
        let pkt = stream_enc.encode_frame(&rgba320)?;
        frames.push((pkt, delay_ms));
        if thumbnail.is_none() {
            thumbnail = image::RgbaImage::from_raw(320, 320, rgba320);
        }
        done += 1;
        progress(done, total_est);
    }

    // Drop the read end so ffmpeg (once killed/exited via `guard`'s `Drop`)
    // can't block on a full stderr pipe, then wait for the drain thread to
    // finish collecting the tail before we potentially report it below.
    drop(stdout);
    let _ = stderr_thread.join();

    if frames.is_empty() {
        let tail = stderr_tail.lock().map(|g| g.clone()).unwrap_or_default();
        if tail.trim().is_empty() {
            bail!("no decodable video frames");
        } else {
            bail!("no decodable video frames -- ffmpeg stderr:\n{}", tail.trim());
        }
    }

    let thumbnail = thumbnail
        .unwrap_or_else(|| image::RgbaImage::from_pixel(320, 320, image::Rgba([8, 10, 18, 255])));

    // `guard` (and thus ffmpeg) is dropped here at function exit, on every
    // path -- success included.
    drop(guard);
    Ok((crate::cache::CachedVideo { width: 320, height: 320, fps, frames }, thumbnail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dims_and_fps_from_a_real_ffmpeg_video_line() {
        let line = "  Stream #0:0[0x1](und): Video: hevc (Main) (hvc1 / 0x31637668), \
                     yuv420p(tv, bt709), 1080x1080 [SAR 1:1 DAR 1:1], 9896 kb/s, 30 fps, \
                     30 tbr, 30 tbn (default)";
        assert_eq!(parse_video_line(line), Some((1080, 1080, 30.0)));
    }

    #[test]
    fn no_video_line_returns_none() {
        let stderr = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':\n  Duration: 00:00:01.00\n";
        assert_eq!(parse_video_line(stderr), None);
    }

    #[test]
    fn missing_dims_on_a_video_line_returns_none() {
        let line = "  Stream #0:0(und): Video: hevc (Main), yuv420p, 30 fps, 30 tbr";
        assert_eq!(parse_video_line(line), None);
    }

    #[test]
    fn falls_back_to_25fps_when_no_fps_or_tbr_token_present() {
        let line = "  Stream #0:0(und): Video: hevc (Main), yuv420p, 640x480";
        assert_eq!(parse_video_line(line), Some((640, 480, 25.0)));
    }

    #[test]
    fn fit_even_scales_down_and_forces_even_dims() {
        assert_eq!(fit_even(1920, 1080), (640, 360));
        assert_eq!(fit_even(100, 101), (100, 102));
        assert_eq!(fit_even(1, 1), (2, 2));
    }

    #[test]
    fn parses_duration_hms() {
        let stderr = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':\n  \
                       Duration: 00:01:02.50, start: 0.000000, bitrate: 900 kb/s\n";
        assert_eq!(parse_duration_secs(stderr), Some(62.5));
    }

    #[test]
    fn missing_duration_line_returns_none() {
        let stderr = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':\n";
        assert_eq!(parse_duration_secs(stderr), None);
    }

    /// Real integration test: only runs when both `ASTRO_FFMPEG` (path to a
    /// real ffmpeg.exe) and `ASTRO_TEST_MP4` (path to a real video file) are
    /// set -- e.g. a controller verifying the one-shot import path actually
    /// decodes+encodes+exits. `#[ignore]`d so a normal `cargo test` never
    /// needs ffmpeg installed.
    #[test]
    #[ignore]
    fn imports_a_real_video_to_cache() {
        let test_mp4 = std::env::var("ASTRO_TEST_MP4").expect("set ASTRO_TEST_MP4 to run this test");
        std::env::var("ASTRO_FFMPEG").expect("set ASTRO_FFMPEG to run this test");
        let mut calls = 0usize;
        let (cv, thumb) = import_to_cache(
            Path::new(&test_mp4),
            crate::media::Fit::Cover,
            1.0,
            [0.0, 0.0],
            1500,
            &mut |_done, _total| calls += 1,
        )
        .expect("import a real video");
        assert!(!cv.frames.is_empty(), "expected at least one encoded frame");
        assert_eq!((cv.width, cv.height), (320, 320));
        assert_eq!(thumb.dimensions(), (320, 320));
        assert!(calls > 0, "progress callback should have been invoked");
    }
}
