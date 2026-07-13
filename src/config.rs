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
}

fn default_fit() -> String {
    "cover".into()
}

fn default_zoom() -> f32 {
    1.0
}

impl Default for MediaCfg {
    fn default() -> Self {
        MediaCfg {
            path: None,
            fit: default_fit(),
            zoom: default_zoom(),
            pan: [0.0, 0.0],
        }
    }
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
}

fn default_twelve_hour() -> bool {
    true
}

fn default_mode() -> String {
    "dashboard".into()
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
}
