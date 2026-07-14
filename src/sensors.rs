use chrono::Local;
use nvml_wrapper::Nvml;
use sysinfo::System;

#[derive(Clone)]
pub struct Snapshot {
    pub gpu_temp_c: Option<u32>,
    pub gpu_usage_pct: Option<u32>,
    pub cpu_usage_pct: u32,
    pub ram_pct: Option<u32>,
    pub time: chrono::DateTime<Local>,
}

impl Snapshot {
    /// Returns the metric value for a widget kind (temp °C, usage %, ram %)
    /// as `f32`. `None` for non-metric kinds (Clock/Date/Text) or a missing
    /// sensor reading.
    pub fn value_for(&self, kind: crate::dashboard::WidgetKind) -> Option<f32> {
        use crate::dashboard::WidgetKind;
        match kind {
            WidgetKind::GpuTemp => self.gpu_temp_c.map(|v| v as f32),
            WidgetKind::GpuUsage => self.gpu_usage_pct.map(|v| v as f32),
            WidgetKind::CpuUsage => Some(self.cpu_usage_pct as f32),
            WidgetKind::RamUsage => self.ram_pct.map(|v| v as f32),
            WidgetKind::Clock | WidgetKind::Date | WidgetKind::Text => None,
        }
    }

    /// Formats a widget's primary text and optional caption for display.
    /// Missing metric values render primary as `"--"`.
    pub fn display_for(&self, w: &crate::dashboard::Widget) -> (String, Option<String>) {
        use crate::dashboard::WidgetKind;
        match w.kind {
            WidgetKind::GpuTemp => {
                let primary = match self.gpu_temp_c {
                    Some(t) => format!("{t}°"),
                    None => "--".to_string(),
                };
                (primary, Some("GPU TEMP".to_string()))
            }
            WidgetKind::GpuUsage => {
                let primary = match self.gpu_usage_pct {
                    Some(v) => format!("{v}%"),
                    None => "--".to_string(),
                };
                (primary, Some("GPU".to_string()))
            }
            WidgetKind::CpuUsage => (format!("{}%", self.cpu_usage_pct), Some("CPU".to_string())),
            WidgetKind::RamUsage => {
                let primary = match self.ram_pct {
                    Some(v) => format!("{v}%"),
                    None => "--".to_string(),
                };
                (primary, Some("RAM".to_string()))
            }
            WidgetKind::Clock => {
                let mut fmt = if w.clock_24h { "%H:%M".to_string() } else { "%-I:%M %p".to_string() };
                if w.show_seconds {
                    fmt = if w.clock_24h { "%H:%M:%S".to_string() } else { "%-I:%M:%S %p".to_string() };
                }
                (self.time.format(&fmt).to_string(), None)
            }
            WidgetKind::Date => (self.time.format(&w.date_fmt).to_string(), None),
            WidgetKind::Text => (w.text.clone(), None),
        }
    }
}

pub struct Sensors {
    nvml: Option<Nvml>,
    sys: System,
}

pub fn new() -> Sensors {
    let nvml = Nvml::init().map_err(|e| log::warn!("NVML unavailable: {e}")).ok();
    let mut sys = System::new();
    sys.refresh_cpu_usage();
    Sensors { nvml, sys }
}

impl Sensors {
    pub fn read(&mut self) -> Snapshot {
        self.sys.refresh_cpu_usage();
        let cpu_usage_pct = self.sys.global_cpu_usage().round() as u32;

        self.sys.refresh_memory();
        let total_mem = self.sys.total_memory();
        let ram_pct = if total_mem > 0 {
            Some((self.sys.used_memory() * 100 / total_mem) as u32)
        } else {
            None
        };

        let (mut gpu_temp_c, mut gpu_usage_pct) = (None, None);
        if let Some(nvml) = &self.nvml {
            if let Ok(dev) = nvml.device_by_index(0) {
                gpu_temp_c = dev
                    .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                    .ok();
                gpu_usage_pct = dev.utilization_rates().ok().map(|u| u.gpu);
            }
        }
        Snapshot {
            gpu_temp_c,
            gpu_usage_pct,
            cpu_usage_pct,
            ram_pct,
            time: Local::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{Widget, WidgetKind};
    use chrono::{Local, TimeZone};

    fn snap(gpu_temp_c: Option<u32>, gpu_usage_pct: Option<u32>, cpu_usage_pct: u32, ram_pct: Option<u32>) -> Snapshot {
        Snapshot {
            gpu_temp_c,
            gpu_usage_pct,
            cpu_usage_pct,
            ram_pct,
            time: Local::now(),
        }
    }

    #[test]
    fn value_for_maps_metric_kinds() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        assert_eq!(s.value_for(WidgetKind::GpuTemp), Some(54.0));
        assert_eq!(s.value_for(WidgetKind::GpuUsage), Some(37.0));
        assert_eq!(s.value_for(WidgetKind::CpuUsage), Some(18.0));
        assert_eq!(s.value_for(WidgetKind::RamUsage), Some(62.0));
    }

    #[test]
    fn value_for_missing_metric_is_none() {
        let s = snap(None, None, 18, None);
        assert_eq!(s.value_for(WidgetKind::GpuTemp), None);
        assert_eq!(s.value_for(WidgetKind::GpuUsage), None);
        assert_eq!(s.value_for(WidgetKind::RamUsage), None);
    }

    #[test]
    fn value_for_non_metric_kinds_is_none() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        assert_eq!(s.value_for(WidgetKind::Clock), None);
        assert_eq!(s.value_for(WidgetKind::Date), None);
        assert_eq!(s.value_for(WidgetKind::Text), None);
    }

    #[test]
    fn display_for_gpu_temp_has_caption_and_degree() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        let w = Widget::new(WidgetKind::GpuTemp);
        let (primary, caption) = s.display_for(&w);
        assert_eq!(primary, "54°");
        assert_eq!(caption, Some("GPU TEMP".to_string()));
    }

    #[test]
    fn display_for_gpu_temp_missing_is_dashes() {
        let s = snap(None, Some(37), 18, Some(62));
        let w = Widget::new(WidgetKind::GpuTemp);
        let (primary, _caption) = s.display_for(&w);
        assert_eq!(primary, "--");
    }

    #[test]
    fn display_for_ram_usage() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        let w = Widget::new(WidgetKind::RamUsage);
        let (primary, caption) = s.display_for(&w);
        assert_eq!(primary, "62%");
        assert_eq!(caption, Some("RAM".to_string()));
    }

    #[test]
    fn display_for_clock_12h_has_am_or_pm() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        let mut w = Widget::new(WidgetKind::Clock);
        w.clock_24h = false;
        let (primary, caption) = s.display_for(&w);
        assert!(primary.contains("AM") || primary.contains("PM"), "expected AM/PM in {primary:?}");
        assert_eq!(caption, None);
    }

    #[test]
    fn display_for_clock_24h_no_am_pm() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        let mut w = Widget::new(WidgetKind::Clock);
        w.clock_24h = true;
        w.show_seconds = true;
        let (primary, _caption) = s.display_for(&w);
        assert!(!primary.contains("AM") && !primary.contains("PM"));
        // HH:MM:SS
        assert_eq!(primary.matches(':').count(), 2);
    }

    #[test]
    fn display_for_date_uses_date_fmt() {
        let time = Local.with_ymd_and_hms(2026, 7, 13, 10, 30, 0).unwrap();
        let s = Snapshot {
            gpu_temp_c: Some(54),
            gpu_usage_pct: Some(37),
            cpu_usage_pct: 18,
            ram_pct: Some(62),
            time,
        };
        let mut w = Widget::new(WidgetKind::Date);
        w.date_fmt = "%Y-%m-%d".to_string();
        let (primary, caption) = s.display_for(&w);
        assert_eq!(primary, "2026-07-13");
        assert_eq!(caption, None);
    }

    #[test]
    fn display_for_text_uses_widget_text() {
        let s = snap(Some(54), Some(37), 18, Some(62));
        let mut w = Widget::new(WidgetKind::Text);
        w.text = "Hello".to_string();
        let (primary, caption) = s.display_for(&w);
        assert_eq!(primary, "Hello");
        assert_eq!(caption, None);
    }
}
