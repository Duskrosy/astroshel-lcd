use crate::dashboard;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MediaCfg {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_fit")]
    pub fit: String,
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    #[serde(default)]
    pub pan: [f32; 2],
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u32,
}

fn default_fit() -> String {
    "cover".into()
}

fn default_zoom() -> f32 {
    1.0
}

fn default_bitrate() -> u32 {
    1500
}

impl Default for MediaCfg {
    fn default() -> Self {
        MediaCfg {
            path: None,
            fit: default_fit(),
            zoom: default_zoom(),
            pan: [0.0, 0.0],
            bitrate_kbps: default_bitrate(),
        }
    }
}

fn default_brightness() -> u8 {
    100
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    #[serde(default = "default_mode")]
    pub mode: String, // "dashboard" | "media"
    #[serde(default)]
    pub media_id: Option<String>, // cache entry id the profile references (None for dashboard)
    #[serde(default = "default_fit")]
    pub fit: String,
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    #[serde(default)]
    pub pan: [f32; 2],
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u32,
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    #[serde(default)]
    pub dashboard: dashboard::Dashboard,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// Brightness as a percent, 1..=100. Mapped to the device's native 0..=255
    /// byte only at the device layer (see `device::set_brightness`). Values
    /// outside 1..=100 (e.g. from a pre-Phase-2b config that stored the raw
    /// 0..=255 device byte) are clamped in `load`.
    pub brightness: u8,
    pub port: Option<String>,
    pub update_ms: u64,
    pub start_at_logon: bool,
    #[serde(default = "default_twelve_hour")]
    pub twelve_hour: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub media: MediaCfg,
    #[serde(default)]
    pub dashboard: dashboard::Dashboard,
    #[serde(default = "default_true")]
    pub show_bitrate_warning: bool,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// Whether the first-run tray nudge (showing the window instead of starting
    /// hidden in the tray) has already fired. `false` on a brand-new config so a
    /// new user notices the app launched; `main` flips this to `true` and
    /// persists it immediately on the first run so every later launch starts
    /// hidden as normal.
    #[serde(default)]
    pub shown_intro: bool,
}

fn default_twelve_hour() -> bool {
    true
}

fn default_mode() -> String {
    "dashboard".into()
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            brightness: 100,
            port: None,
            update_ms: 1000,
            start_at_logon: true,
            twelve_hour: true,
            mode: default_mode(),
            media: MediaCfg::default(),
            dashboard: dashboard::Dashboard::default(),
            show_bitrate_warning: default_true(),
            profiles: Vec::new(),
            shown_intro: false,
        }
    }
}

pub fn load(path: &Path) -> Config {
    let mut cfg: Config = match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    // Clamp brightness to the valid percent range on every load: guards against
    // pre-Phase-2b configs that stored the raw 0..=255 device byte (e.g. an old
    // default of 255) as well as any other out-of-range/corrupt value.
    cfg.brightness = cfg.brightness.clamp(1, 100);
    // Clamp bitrate into a sane operating range on every load: guards against a
    // corrupt/out-of-range value (or a future config format change) landing on
    // the encoder unchecked.
    cfg.media.bitrate_kbps = cfg.media.bitrate_kbps.clamp(300, 8000);
    // Mirror both top-level clamps onto every profile: an out-of-range profile
    // `brightness` (e.g. a pre-Phase-2b raw 0..=255 byte) or `bitrate_kbps` would
    // otherwise reach the device/encoder unclamped when that profile is applied
    // -- notably `bitrate_kbps * 1000` overflowing the encoder's `u32` downstream
    // in `encode::new_stream_encoder` for a corrupt/huge value.
    for p in &mut cfg.profiles {
        p.brightness = p.brightness.clamp(1, 100);
        p.bitrate_kbps = p.bitrate_kbps.clamp(300, 8000);
    }
    cfg
}

pub fn save(path: &Path, cfg: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_full_brightness_and_1s() {
        let c = Config::default();
        assert_eq!(c.brightness, 100); // 100% (percent unit; Phase 2b)
        assert_eq!(c.update_ms, 1000);
        assert!(c.twelve_hour);
    }

    #[test]
    fn config_missing_twelve_hour_field_defaults_true() {
        // Simulates an existing config.toml written before `twelve_hour` existed.
        let toml_str = "brightness = 200\nupdate_ms = 500\nstart_at_logon = false\n";
        let c: Config = toml::from_str(toml_str).expect("should parse without twelve_hour field");
        assert!(c.twelve_hour);
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = std::env::temp_dir().join("astroshel_cfg_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.brightness = 80; // a valid in-range percent (1..=100)
        save(&path, &c).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.brightness, 80);
    }

    #[test]
    fn missing_file_returns_default() {
        let c = load(std::path::Path::new("Z:/does/not/exist.toml"));
        assert_eq!(c.brightness, 100);
    }

    #[test]
    fn load_clamps_out_of_range_brightness() {
        // A pre-Phase-2b config storing the old raw 0..=255 device byte (e.g. the old
        // default of 255) must clamp into the new 1..=100 percent range on load.
        let dir = std::env::temp_dir().join("astroshel_cfg_clamp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old_raw_brightness.toml");
        std::fs::write(&path, "brightness = 255\nupdate_ms = 1000\nstart_at_logon = true\n").unwrap();
        let c = load(&path);
        assert_eq!(c.brightness, 100);

        let path0 = dir.join("zero_brightness.toml");
        std::fs::write(&path0, "brightness = 0\nupdate_ms = 1000\nstart_at_logon = true\n").unwrap();
        let c0 = load(&path0);
        assert_eq!(c0.brightness, 1);
    }

    #[test]
    fn media_defaults_present() {
        let c = Config::default();
        assert_eq!(c.mode, "dashboard");
        assert_eq!(c.media.fit, "cover");
        assert_eq!(c.media.zoom, 1.0);
        assert_eq!(c.media.pan, [0.0, 0.0]);
        assert!(c.media.path.is_none());
    }

    #[test]
    fn dashboard_roundtrip_preserves_customization() {
        // Task 4 carry-forward: a non-default dashboard (theme accent tweaked)
        // must survive a save/load (toml) round-trip unchanged.
        let mut c = Config::default();
        c.dashboard.theme.accent = [1, 2, 3];
        let s = toml::to_string(&c).unwrap();
        let c2: Config = toml::from_str(&s).unwrap();
        assert_eq!(c2.dashboard, c.dashboard);
    }

    #[test]
    fn default_dashboard_roundtrip() {
        let c = Config::default();
        let s = toml::to_string(&c).unwrap();
        let c2: Config = toml::from_str(&s).unwrap();
        assert_eq!(c2.dashboard, c.dashboard);
    }

    #[test]
    fn old_config_without_dashboard_field_loads_default_template() {
        // A pre-Phase-3a config file (no `[dashboard]`/`dashboard` field) must
        // load with the default dashboard template (`#[serde(default)]`).
        let toml_str = "brightness = 80\nupdate_ms = 1000\nstart_at_logon = true\n";
        let c: Config = toml::from_str(toml_str).expect("should parse without dashboard field");
        assert_eq!(c.dashboard, dashboard::Dashboard::default());
    }

    #[test]
    fn old_config_without_media_still_loads() {
        // A pre-Phase-2 config file (only Phase-1 fields) must load with media defaults.
        let dir = std::env::temp_dir().join("astroshel_cfg_p2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.toml");
        std::fs::write(&path, "brightness = 200\nupdate_ms = 1000\nstart_at_logon = true\n").unwrap();
        let c = load(&path);
        assert_eq!(c.brightness, 100);      // 200 is out of the 1..=100 percent range; clamped
        assert_eq!(c.mode, "dashboard");    // defaulted
        assert_eq!(c.media.fit, "cover");   // defaulted
    }

    #[test]
    fn old_config_without_bitrate_fields_loads_defaults() {
        // A pre-media-bitrate config (only earlier-phase fields) must load with the
        // new bitrate/warning fields defaulted rather than failing to parse.
        let toml_str = "brightness = 80\nupdate_ms = 1000\nstart_at_logon = true\n";
        let c: Config = toml::from_str(toml_str).expect("should parse without bitrate fields");
        assert_eq!(c.media.bitrate_kbps, 1500);
        assert!(c.show_bitrate_warning);
    }

    #[test]
    fn load_clamps_out_of_range_bitrate() {
        let dir = std::env::temp_dir().join("astroshel_cfg_bitrate_clamp");
        std::fs::create_dir_all(&dir).unwrap();

        let high_path = dir.join("high_bitrate.toml");
        std::fs::write(
            &high_path,
            "brightness = 100\nupdate_ms = 1000\nstart_at_logon = true\n\n[media]\nbitrate_kbps = 20000\n",
        )
        .unwrap();
        let c_high = load(&high_path);
        assert_eq!(c_high.media.bitrate_kbps, 8000);

        let low_path = dir.join("low_bitrate.toml");
        std::fs::write(
            &low_path,
            "brightness = 100\nupdate_ms = 1000\nstart_at_logon = true\n\n[media]\nbitrate_kbps = 10\n",
        )
        .unwrap();
        let c_low = load(&low_path);
        assert_eq!(c_low.media.bitrate_kbps, 300);
    }

    #[test]
    fn load_clamps_out_of_range_profile_fields() {
        // A profile with a corrupt/out-of-range `bitrate_kbps` (e.g. from a bad
        // hand-edit or future format change) must not reach the encoder
        // unclamped: `bitrate_kbps * 1000` would overflow `u32` downstream in
        // `encode::new_stream_encoder`. Likewise `brightness` must clamp into
        // the 1..=100 percent range like the top-level field does.
        let dir = std::env::temp_dir().join("astroshel_cfg_profile_clamp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad_profile.toml");
        std::fs::write(
            &path,
            "brightness = 100\nupdate_ms = 1000\nstart_at_logon = true\n\n\
             [[profiles]]\nname = \"Bad\"\nmode = \"media\"\nbitrate_kbps = 9999999\nbrightness = 250\n",
        )
        .unwrap();
        let c = load(&path);
        assert_eq!(c.profiles.len(), 1);
        assert_eq!(c.profiles[0].bitrate_kbps, 8000);
        assert_eq!(c.profiles[0].brightness, 100);
    }

    #[test]
    fn shown_intro_defaults_false_and_roundtrips() {
        let c = Config::default();
        assert!(!c.shown_intro, "a brand-new config must not have shown the intro yet");

        let dir = std::env::temp_dir().join("astroshel_cfg_shown_intro");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut c2 = Config::default();
        c2.shown_intro = true;
        save(&path, &c2).unwrap();
        let loaded = load(&path);
        assert!(loaded.shown_intro, "shown_intro=true must survive a save/load round-trip");

        // Pre-existing config files (written before `shown_intro` existed) must
        // still parse, defaulting to false rather than failing.
        let toml_str = "brightness = 80\nupdate_ms = 1000\nstart_at_logon = true\n";
        let c3: Config = toml::from_str(toml_str).expect("should parse without shown_intro field");
        assert!(!c3.shown_intro);
    }

    #[test]
    fn old_config_without_profiles_field_loads_empty_vec() {
        // A pre-Phase-3c config file (no `profiles` field) must load with an
        // empty profiles list rather than failing to parse.
        let toml_str = "brightness = 80\nupdate_ms = 1000\nstart_at_logon = true\n";
        let c: Config = toml::from_str(toml_str).expect("should parse without profiles field");
        assert!(c.profiles.is_empty());
    }

    #[test]
    fn profiles_roundtrip_preserves_len_and_fields() {
        let mut c = Config::default();
        c.profiles.push(Profile {
            name: "Dashboard Home".into(),
            mode: "dashboard".into(),
            media_id: None,
            fit: default_fit(),
            zoom: default_zoom(),
            pan: [0.0, 0.0],
            bitrate_kbps: default_bitrate(),
            brightness: default_brightness(),
            dashboard: dashboard::Dashboard::default(),
        });
        c.profiles.push(Profile {
            name: "Vacation Clip".into(),
            mode: "media".into(),
            media_id: Some("cache_entry_42".into()),
            fit: default_fit(),
            zoom: default_zoom(),
            pan: [0.0, 0.0],
            bitrate_kbps: default_bitrate(),
            brightness: default_brightness(),
            dashboard: dashboard::Dashboard::default(),
        });

        let s = toml::to_string(&c).unwrap();
        let c2: Config = toml::from_str(&s).unwrap();

        assert_eq!(c2.profiles.len(), c.profiles.len());
        assert_eq!(c2.profiles.len(), 2);
        assert_eq!(c2.profiles[0].name, "Dashboard Home");
        assert_eq!(c2.profiles[0].mode, "dashboard");
        assert!(c2.profiles[0].media_id.is_none());
        assert_eq!(c2.profiles[1].name, "Vacation Clip");
        assert_eq!(c2.profiles[1].mode, "media");
        assert_eq!(c2.profiles[1].media_id.as_deref(), Some("cache_entry_42"));
    }
}
