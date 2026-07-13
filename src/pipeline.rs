use crate::command::{Command, Mode};
use crate::media::{self, Fit, Media};
use crate::render::compose_media;
use crate::{config::Config, device, encode, render, sensors};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Clamp the frame interval so a static frame never outlives the device's ~5s watchdog.
pub fn clamp_tick(update_ms: u64) -> Duration {
    Duration::from_millis(update_ms.clamp(200, 3000))
}

pub fn run(cfg: Config, stop: Arc<AtomicBool>, rx: Receiver<Command>) -> anyhow::Result<()> {
    let mut renderer = render::new()?;
    renderer.twelve_hour = cfg.twelve_hour;
    let mut encoder = encode::new_encoder(320, 320)?;
    let mut sensors = sensors::new();
    let tick = clamp_tick(cfg.update_ms);
    // `brightness` is a percent (1..=100) end-to-end here; `device::open` /
    // `device::set_brightness` are the sole places that map it to the device's
    // native 0..=255 byte, so no mapping happens in this module.
    let mut brightness = cfg.brightness;
    let mut mode = Mode::from_str(&cfg.mode);

    // Media state: decoded media (if any), its fit/zoom/pan, GIF frame cursor, and the
    // encoded-keyframe cache for static images (invalidated whenever media/fit/zoom/pan
    // changes via `Command::LoadMedia`/`ClearMedia`).
    let mut media_obj: Option<Media> = None;
    let mut fit = Fit::from_str(&cfg.media.fit);
    let mut zoom = cfg.media.zoom;
    let mut pan = cfg.media.pan;
    let mut gif_idx: usize = 0;
    let mut static_cache: Option<Vec<u8>> = None;
    if let Some(pth) = cfg.media.path.clone() {
        match media::load(std::path::Path::new(&pth)) {
            Ok(m) => media_obj = Some(m),
            Err(e) => log::warn!("media load: {e:#}"),
        }
    }

    while !stop.load(Ordering::Relaxed) {
        // (Re)connect
        let port = cfg.port.clone().or_else(device::find_port).unwrap_or_else(|| "COM3".into());
        let mut lcd = match device::open(&port, brightness) {
            Ok(l) => {
                log::info!("connected on {port}");
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
                    }
                    Command::LoadMedia { path, fit: f, zoom: z, pan: pn } => {
                        fit = Fit::from_str(&f);
                        zoom = z;
                        pan = pn;
                        gif_idx = 0;
                        static_cache = None;
                        match media::load(std::path::Path::new(&path)) {
                            Ok(m) => media_obj = Some(m),
                            Err(e) => log::warn!("media load: {e:#}"),
                        }
                    }
                    Command::ClearMedia => {
                        media_obj = None;
                        static_cache = None;
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
                    let rgba = renderer.dashboard(&snap);
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
                Mode::Media => match &media_obj {
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
                    Some(Media::Animated(frames)) => {
                        let fr = &frames[gif_idx % frames.len()];
                        let (sw, sh) = fr.img.dimensions();
                        let p = media::placement(sw, sh, fit, zoom, pan);
                        let rgba = compose_media(&fr.img, &p, [8, 10, 18, 255]);
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
                        let d = Duration::from_millis(fr.delay_ms.max(40) as u64);
                        gif_idx = gif_idx.wrapping_add(1);
                        sleep_interruptible(d, &stop);
                        continue;
                    }
                    None => {
                        // No media loaded: solid placeholder (unchanged from 2a).
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
                },
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_tick;

    #[test]
    fn clamp_tick_bounds() {
        assert_eq!(clamp_tick(50).as_millis(), 200);
        assert_eq!(clamp_tick(1000).as_millis(), 1000);
        assert_eq!(clamp_tick(99999).as_millis(), 3000);
    }
}
