#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dashboard,
    Media,
}

impl Mode {
    pub fn from_str(s: &str) -> Mode {
        if s.eq_ignore_ascii_case("media") { Mode::Media } else { Mode::Dashboard }
    }
    pub fn as_str(self) -> &'static str {
        match self { Mode::Dashboard => "dashboard", Mode::Media => "media" }
    }
}

#[derive(Clone, Debug)]
pub enum Command {
    /// Brightness percent, 1..=100. Mapped to the device's native 0..=255 byte
    /// only inside `device::set_brightness`; every other layer speaks percent.
    SetBrightness(u8),
    SetMode(Mode),
    /// Load (or reload) the media file at `path` with the given fit/zoom/pan.
    /// Errors are logged and ignored by the pipeline, keeping any prior media.
    LoadMedia {
        path: String,
        fit: String,
        zoom: f32,
        pan: [f32; 2],
        bitrate_kbps: u32,
    },
    /// Clear any loaded media, reverting `Mode::Media` to the solid placeholder.
    ClearMedia,
    /// Load a cached, pre-encoded video (`.lcdv`, see `crate::cache`) built by
    /// `crate::video::import_to_cache`. Playback is pure send-I/O -- the same
    /// `frame_packets` path a pre-encoded GIF uses -- with no live ffmpeg
    /// process involved. Errors (missing/corrupt file) are logged and ignored,
    /// keeping whatever was previously displayed.
    LoadCachedVideo { lcdv_path: String },
    /// Replace the active dashboard (theme + widgets) used by the Dashboard render arm.
    SetDashboard(crate::dashboard::Dashboard),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_roundtrip() {
        assert_eq!(Mode::from_str("media"), Mode::Media);
        assert_eq!(Mode::from_str("Dashboard"), Mode::Dashboard);
        assert_eq!(Mode::from_str("garbage"), Mode::Dashboard);
        assert_eq!(Mode::Media.as_str(), "media");
    }
}
