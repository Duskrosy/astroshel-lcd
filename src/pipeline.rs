use crate::command::{Command, Mode};
use crate::media::{self, Fit, Media, MediaFrame};
use crate::render::compose_media;
use crate::{cache, config::Config, device, encode, render, sensors};
use encode::StreamEncoder;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Clamp the frame interval so a static frame never outlives the device's ~5s watchdog.
pub fn clamp_tick(update_ms: u64) -> Duration {
    Duration::from_millis(update_ms.clamp(200, 3000))
}

/// Pre-encodes an entire GIF's frames to H.264 packets exactly once, so playback
/// becomes pure send-I/O (no per-tick openh264 encode) -- the fix for a large GIF
/// not holding target framerate under the old re-encode-every-tick scheme.
///
/// Each source frame is cropped+resized to the device's fixed 320x320 output via
/// `render::compose_frames`, `stream_enc` is forced to emit a self-contained IDR
/// for the first packet, and every composed RGBA frame is encoded in order. The
/// composed RGBA buffers are dropped when this function returns -- only the
/// encoded packet bytes (+ each frame's delay) are kept, capping steady-state RAM.
pub fn preencode_gif(
    stream_enc: &mut StreamEncoder,
    frames: &[MediaFrame],
    fit: Fit,
    zoom: f32,
    pan: [f32; 2],
) -> anyhow::Result<Vec<(Vec<u8>, u32)>> {
    let composed = render::compose_frames(frames, fit, zoom, pan);
    stream_enc.force_keyframe();
    let mut packets = Vec::with_capacity(composed.len());
    for (rgba, delay) in &composed {
        let pkt = stream_enc.encode_frame(rgba)?;
        packets.push((pkt, *delay));
    }
    Ok(packets)
}

pub fn run(
    cfg: Config,
    stop: Arc<AtomicBool>,
    rx: Receiver<Command>,
    media_busy: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut renderer = render::new()?;
    renderer.twelve_hour = cfg.twelve_hour;
    let mut encoder = encode::new_encoder(320, 320)?;
    let mut cur_bitrate = cfg.media.bitrate_kbps;
    let mut stream_enc = encode::new_stream_encoder(320, 320, cur_bitrate)?;
    let mut sensors = sensors::new();
    let tick = clamp_tick(cfg.update_ms);
    // `brightness` is a percent (1..=100) end-to-end here; `device::open` /
    // `device::set_brightness` are the sole places that map it to the device's
    // native 0..=255 byte, so no mapping happens in this module.
    let mut brightness = cfg.brightness;
    let mut mode = Mode::from_str(&cfg.mode);
    let mut dash = cfg.dashboard.clone();
    let mut hist = render::History::new();

    // Media state: decoded media (if any), its fit/zoom/pan, playback frame cursor, and
    // the encoded-keyframe cache for static images (invalidated whenever media/fit/zoom/pan
    // changes via `Command::LoadMedia`/`ClearMedia`).
    //
    // Animated GIFs and cached videos are both handled the same way: rather than keeping
    // full-resolution decoded frames around and re-encoding one of them every single
    // playback tick -- which for a large GIF still couldn't keep up with the target
    // framerate on openh264, and which is exactly what the old live-ffmpeg video path did
    // -- the whole thing is pre-ENCODED to H.264 packets exactly once (via `preencode_gif`
    // for a GIF, or `crate::video::import_to_cache` + `cache::write_lcdv` ahead of time for
    // a video) and stashed in `frame_packets`. Playback then just sends the pre-encoded
    // bytes and paces by each frame's own delay -- no per-tick encode, and no live decode
    // process, at all; `media_obj` is dropped (`None`) in this case since neither the
    // full-resolution frames nor composed RGBA are needed anymore, which also caps
    // steady-state RAM.
    let mut media_obj: Option<Media> = None;
    let mut fit = Fit::from_str(&cfg.media.fit);
    let mut zoom = cfg.media.zoom;
    let mut pan = cfg.media.pan;
    let mut gif_idx: usize = 0;
    let mut static_cache: Option<Vec<u8>> = None;
    let mut frame_packets: Option<Vec<(Vec<u8>, u32)>> = None;
    if let Some(pth) = cfg.media.path.clone() {
        let p = std::path::Path::new(&pth);
        // Video is imported separately (GUI Task 4a wires cached-video selection via
        // `Command::LoadCachedVideo`) -- a video-extension path configured in
        // `cfg.media.path` is simply skipped at startup for now. Static/GIF preload
        // unconditionally (cheap, and keeps switching into Media mode instant).
        if !media::is_video_path(p) {
            media_busy.store(true, Ordering::Relaxed);
            match media::load(p) {
                Ok(Media::Animated(frames)) => match preencode_gif(&mut stream_enc, &frames, fit, zoom, pan) {
                    Ok(pkts) => frame_packets = Some(pkts),
                    Err(e) => log::warn!("gif pre-encode: {e:#}"),
                },
                Ok(m) => media_obj = Some(m),
                Err(e) => log::warn!("media load: {e:#}"),
            }
            media_busy.store(false, Ordering::Relaxed);
        }
    }

    while !stop.load(Ordering::Relaxed) {
        // (Re)connect
        let port = cfg.port.clone().or_else(device::find_port).unwrap_or_else(|| "COM3".into());
        let mut lcd = match device::open(&port, brightness) {
            Ok(l) => {
                log::info!("connected on {port}");
                stream_enc.force_keyframe();
                // The freshly-connected device's decoder must not resume mid-stream on a
                // P-frame: restart pre-encoded playback (GIF or cached video) from
                // packet[0], which both `preencode_gif` and `video::import_to_cache`
                // always make a self-contained IDR.
                gif_idx = 0;
                l
            }
            Err(e) => {
                log::warn!("connect failed ({e:#}); retrying in 3s");
                sleep_interruptible(Duration::from_secs(3), &stop);
                continue;
            }
        };

        // Frame loop
        while !stop.load(Ordering::Relaxed) {
            // Drain pending commands (non-blocking); coalesce brightness to the latest value.
            let mut pending_brightness: Option<u8> = None;
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Command::SetBrightness(v) => {
                        brightness = v;
                        pending_brightness = Some(v);
                    }
                    Command::SetMode(m) => {
                        mode = m;
                        log::info!("mode -> {}", m.as_str());
                        // No process lifecycle to manage on a mode switch anymore: cached
                        // video (like GIF) is just packets already sitting in
                        // `frame_packets`, not a live ffmpeg child to drop/reopen. But the
                        // playback cursor into `frame_packets` must restart at packet[0]
                        // (a self-contained IDR) on every mode switch: leaving `gif_idx`
                        // wherever it was left off would resume Media playback mid-stream
                        // on a P-frame the freshly-(re)shown decoder can't decode, causing
                        // visible corruption until playback wraps back to 0. Mirrors the
                        // same reset done on reconnect above.
                        gif_idx = 0;
                    }
                    Command::LoadMedia { path, fit: f, zoom: z, pan: pn, bitrate_kbps } => {
                        fit = Fit::from_str(&f);
                        zoom = z;
                        pan = pn;
                        gif_idx = 0;
                        static_cache = None;
                        frame_packets = None;
                        media_obj = None;
                        // Recreate the streaming encoder BEFORE pre-encoding below, so any
                        // GIF pre-encode that follows bakes in the new bitrate rather than
                        // the stale one. On encoder-init failure, keep the previous
                        // `stream_enc`/`cur_bitrate` (log and carry on with the old bitrate)
                        // rather than leaving `stream_enc` in a half-updated state.
                        if bitrate_kbps != cur_bitrate {
                            match encode::new_stream_encoder(320, 320, bitrate_kbps) {
                                Ok(e) => {
                                    stream_enc = e;
                                    cur_bitrate = bitrate_kbps;
                                }
                                Err(e) => log::warn!("stream encoder re-init at {bitrate_kbps}kbps failed: {e:#}"),
                            }
                        }
                        media_busy.store(true, Ordering::Relaxed);
                        match media::load(std::path::Path::new(&path)) {
                            Ok(Media::Animated(frames)) => {
                                match preencode_gif(&mut stream_enc, &frames, fit, zoom, pan) {
                                    Ok(pkts) => frame_packets = Some(pkts),
                                    Err(e) => log::warn!("gif pre-encode: {e:#}"),
                                }
                            }
                            Ok(m) => media_obj = Some(m),
                            Err(e) => log::warn!("media load: {e:#}"),
                        }
                        media_busy.store(false, Ordering::Relaxed);
                    }
                    Command::LoadCachedVideo { lcdv_path } => {
                        // Cached video is played back exactly like a pre-encoded GIF: no
                        // live ffmpeg process, just the packets from `.lcdv` streamed
                        // send-only via `frame_packets`. A read error is logged and the
                        // current display is left alone.
                        media_busy.store(true, Ordering::Relaxed);
                        match cache::read_lcdv(std::path::Path::new(&lcdv_path)) {
                            Ok(cv) => {
                                frame_packets = Some(cv.frames);
                                media_obj = None;
                                gif_idx = 0;
                                static_cache = None;
                            }
                            Err(e) => log::warn!("load cached video {lcdv_path}: {e:#}"),
                        }
                        media_busy.store(false, Ordering::Relaxed);
                    }
                    Command::ClearMedia => {
                        media_obj = None;
                        static_cache = None;
                        frame_packets = None;
                    }
                    Command::SetDashboard(d) => {
                        dash = d;
                    }
                }
            }
            // Apply pending brightness once after draining all commands.
            if let Some(v) = pending_brightness {
                if let Err(e) = lcd.set_brightness(v) {
                    log::warn!("brightness: {e:#}");
                }
            }

            let t0 = Instant::now();
            match mode {
                Mode::Dashboard => {
                    let snap = sensors.read();
                    for w in &dash.widgets {
                        if let Some(v) = snap.value_for(w.kind) {
                            hist.push(w.kind, v);
                        }
                    }
                    let rgba = renderer.render_dashboard(&dash, &snap, &hist);
                    let h264 = match encoder.encode_keyframe(&rgba) {
                        Ok(b) => b,
                        Err(e) => {
                            log::error!("encode: {e:#}");
                            break;
                        }
                    };
                    if let Err(e) = lcd.send_video(&h264) {
                        log::warn!("send failed ({e:#}); reconnecting");
                        break; // drop out to reconnect
                    }
                    let elapsed = t0.elapsed();
                    if elapsed < tick {
                        sleep_interruptible(tick - elapsed, &stop);
                    }
                }
                Mode::Media => {
                    // Pre-encoded packet path: a GIF (`preencode_gif`) or a cached video
                    // (`.lcdv`, loaded via `Command::LoadCachedVideo`) was already encoded to
                    // H.264 packets once, so playback here is pure send-I/O -- no per-tick
                    // encode (or live decode process) at all, which is what previously
                    // couldn't keep up for a large GIF even after pre-composing RGBA, and is
                    // what the old live-ffmpeg video path no longer needs to do either.
                    if let Some(pkts) = &frame_packets {
                        if !pkts.is_empty() {
                            // Captured before the send so the frame's on-wire period is
                            // `frame_start + delay`, not `send_time + delay`: pacing off a
                            // deadline (rather than sleeping a fixed duration after send)
                            // keeps playback on-schedule instead of drifting later every
                            // frame by however long the send took.
                            let frame_start = Instant::now();
                            let (bytes, delay_ms) = &pkts[gif_idx % pkts.len()];
                            if !bytes.is_empty() {
                                if let Err(e) = lcd.send_video(bytes) {
                                    log::warn!("send failed ({e:#}); reconnecting");
                                    break; // drop out to reconnect
                                }
                            }
                            // GIF frames streamed as P-frames via `stream_enc` at pre-encode
                            // time (IDR forced as packet[0], and again on reconnect via
                            // `gif_idx = 0`), so bitrate stays within the device's tolerance
                            // at up to 25 fps.
                            let deadline = frame_start + Duration::from_millis((*delay_ms).max(40) as u64);
                            gif_idx = gif_idx.wrapping_add(1);
                            sleep_until(deadline, &stop);
                            continue;
                        }
                    }
                    match &media_obj {
                        Some(Media::Static(img)) => {
                            if static_cache.is_none() {
                                let (sw, sh) = img.dimensions();
                                let p = media::placement(sw, sh, fit, zoom, pan);
                                let rgba = compose_media(img, &p, [8, 10, 18, 255]);
                                match encoder.encode_keyframe(&rgba) {
                                    Ok(b) => static_cache = Some(b),
                                    Err(e) => {
                                        log::error!("encode: {e:#}");
                                        break;
                                    }
                                }
                            }
                            if let Some(bytes) = &static_cache {
                                if let Err(e) = lcd.send_video(bytes) {
                                    log::warn!("send failed ({e:#}); reconnecting");
                                    break; // drop out to reconnect
                                }
                            }
                            sleep_interruptible(tick, &stop); // keepalive cadence, no re-encode
                            continue;
                        }
                        _ => {
                            // No media loaded (or, defensively, an `Animated` object that
                            // wasn't converted to `frame_packets` for some reason): solid
                            // placeholder (unchanged from 2a).
                            let rgba: Vec<u8> = vec![10u8, 12, 18, 255]
                                .iter()
                                .cloned()
                                .cycle()
                                .take(320 * 320 * 4)
                                .collect();
                            let h264 = match encoder.encode_keyframe(&rgba) {
                                Ok(b) => b,
                                Err(e) => {
                                    log::error!("encode: {e:#}");
                                    break;
                                }
                            };
                            if let Err(e) = lcd.send_video(&h264) {
                                log::warn!("send failed ({e:#}); reconnecting");
                                break; // drop out to reconnect
                            }
                            let elapsed = t0.elapsed();
                            if elapsed < tick {
                                sleep_interruptible(tick - elapsed, &stop);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn sleep_interruptible(dur: Duration, stop: &Arc<AtomicBool>) {
    let end = Instant::now() + dur;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sleeps in short (<=5ms) slices until `deadline`, so callers can pace playback to an
/// absolute point in time (deadline-based pacing) rather than a fixed duration after the
/// call -- this is what lets GIF playback pace to `frame_start + delay` instead of
/// quantizing every frame's delay up to the poll granularity. Returns promptly if
/// `deadline` is already in the past or if `stop` is set.
fn sleep_until(deadline: Instant, stop: &Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_tick_bounds() {
        assert_eq!(clamp_tick(50).as_millis(), 200);
        assert_eq!(clamp_tick(1000).as_millis(), 1000);
        assert_eq!(clamp_tick(99999).as_millis(), 3000);
    }

    #[test]
    fn sleep_until_returns_promptly_when_deadline_already_passed() {
        let stop = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() - Duration::from_millis(50);
        let t0 = Instant::now();
        sleep_until(deadline, &stop);
        assert!(t0.elapsed() < Duration::from_millis(20), "should return immediately for a past deadline");
    }

    #[test]
    fn sleep_until_returns_promptly_when_stop_is_set() {
        let stop = Arc::new(AtomicBool::new(true));
        let deadline = Instant::now() + Duration::from_secs(10);
        let t0 = Instant::now();
        sleep_until(deadline, &stop);
        assert!(t0.elapsed() < Duration::from_millis(20), "should return immediately when stop is set");
    }

    #[test]
    fn sleep_until_waits_until_the_deadline() {
        let stop = Arc::new(AtomicBool::new(false));
        let wait = Duration::from_millis(40);
        let deadline = Instant::now() + wait;
        let t0 = Instant::now();
        sleep_until(deadline, &stop);
        assert!(t0.elapsed() >= wait, "should not return before the deadline");
    }

    // Mirrors the NAL-sniffing helper in encode.rs's `stream_tests`.
    fn has(nal: &[u8], t: u8) -> bool {
        nal.windows(5).any(|w| w == [0, 0, 0, 1, t]) || nal.windows(4).any(|w| w == [0, 0, 1, t])
    }

    #[test]
    fn preencode_gif_yields_leading_idr_then_nonempty_packets() {
        let mut stream_enc = encode::new_stream_encoder(320, 320, 1500).unwrap();
        // Three distinct-ish 8x8 source frames (compose_frames will letterbox/scale
        // each up to the device's fixed 320x320 output).
        let mk = |v: u8| MediaFrame {
            img: image::RgbaImage::from_raw(8, 8, vec![v; 8 * 8 * 4]).unwrap(),
            delay_ms: 30,
        };
        let frames = vec![mk(20), mk(120), mk(220)];

        let packets = preencode_gif(&mut stream_enc, &frames, Fit::from_str("cover"), 1.0, [0.0, 0.0]).unwrap();

        assert_eq!(packets.len(), 3);
        let (first, first_delay) = &packets[0];
        assert!(has(first, 0x67) && has(first, 0x65), "first packet must be a self-contained IDR");
        assert_eq!(*first_delay, 30);
        for (pkt, delay) in &packets[1..] {
            assert!(!pkt.is_empty(), "every composed frame should yield a non-empty packet");
            assert_eq!(*delay, 30);
        }
    }
}
