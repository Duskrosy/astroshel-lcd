use chrono::Local;
use nvml_wrapper::Nvml;
use sysinfo::System;

pub struct Snapshot {
    pub gpu_temp_c: Option<u32>,
    pub gpu_usage_pct: Option<u32>,
    pub cpu_usage_pct: u32,
    pub time: chrono::DateTime<Local>,
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
            time: Local::now(),
        }
    }
}
