use crate::proto::{self, DeviceInfo, T_BRIGHT, T_HELLO, T_RES, T_VIDEO};
use anyhow::{Context, Result};
use serialport::SerialPort;
use std::io::{self, Read, Write};
use std::time::Duration;

const VID: u16 = 0x1F3A;
const PID: u16 = 0x0007;
const BAUD: u32 = 1_000_000;

pub struct Lcd {
    port: Box<dyn SerialPort>,
    info: Option<DeviceInfo>,
}

/// Auto-detect the LCD's COM port by USB VID/PID; fall back to None.
pub fn find_port() -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    for p in ports {
        if let serialport::SerialPortType::UsbPort(usb) = p.port_type {
            if usb.vid == VID && usb.pid == PID {
                return Some(p.port_name);
            }
        }
    }
    None
}

/// Maps a 1..=100 brightness percent to the device's native 0..=255 byte,
/// clamping out-of-range input (e.g. a pre-Phase-2b config carrying a raw
/// 0..=255 value) into the valid percent range first.
fn percent_to_raw(percent: u8) -> u8 {
    let pct = percent.clamp(1, 100) as u16;
    ((pct * 255 + 50) / 100) as u8
}

/// `brightness` is a percent (1..=100); mapped to the device's native byte
/// inside `set_brightness`.
pub fn open(port: &str, brightness: u8) -> Result<Lcd> {
    let mut port = serialport::new(port, BAUD)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| "open COM port")?;

    // MANDATORY: deassert DTR and RTS, or the panel stays black.
    if let Err(e) = port.write_data_terminal_ready(false) {
        log::warn!("failed to deassert DTR: {e}");
    }
    if let Err(e) = port.write_request_to_send(false) {
        log::warn!("failed to deassert RTS: {e}");
    }

    let mut lcd = Lcd { port, info: None };
    lcd.handshake().context("handshake")?;
    lcd.set_brightness(brightness)?;
    Ok(lcd)
}

impl Lcd {
    fn write_msg(&mut self, msg_type: u8, payload: &[u8]) -> Result<()> {
        self.port.write_all(&proto::frame(msg_type, payload))?;
        self.port.flush()?;
        Ok(())
    }

    fn read_reply(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut buf = Vec::with_capacity(256);
        let mut iterations = 0;
        const MAX_ITERATIONS: usize = 16;

        loop {
            if iterations >= MAX_ITERATIONS {
                return None;
            }
            iterations += 1;

            let mut chunk = [0u8; 64];
            match self.port.read(&mut chunk) {
                Ok(0) => {
                    // Read returned 0 bytes, no more data
                    return None;
                }
                Ok(n) => {
                    // Accumulated n bytes
                    buf.extend_from_slice(&chunk[..n]);
                    // Try to parse the accumulated buffer
                    if let Some(reply) = proto::parse_reply(&buf) {
                        return Some(reply);
                    }
                    // Continue loop to read more bytes
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => {
                    // Timeout or would-block, stop trying
                    return None;
                }
                Err(_) => {
                    // Other error, stop trying
                    return None;
                }
            }
        }
    }

    fn handshake(&mut self) -> Result<()> {
        self.write_msg(T_HELLO, &[0x01])?;
        let id = self.read_reply().map(|(_, p)| String::from_utf8_lossy(&p).to_string());
        self.write_msg(T_RES, &[0x01])?;
        let mut w = 320u16;
        let mut h = 320u16;
        if let Some((T_RES, p)) = self.read_reply() {
            if p.len() >= 4 {
                w = u16::from_be_bytes([p[0], p[1]]);
                h = u16::from_be_bytes([p[2], p[3]]);
            }
        }
        self.info = Some(DeviceInfo { id: id.unwrap_or_default(), width: w, height: h });
        log::info!("LCD handshake ok: {:?}", self.info);
        Ok(())
    }

    /// `percent` is 1..=100 (the app-wide brightness unit); this is the sole place
    /// that maps it to the device's native 0..=255 byte for the `T_BRIGHT` command.
    pub fn set_brightness(&mut self, percent: u8) -> Result<()> {
        let raw = percent_to_raw(percent);
        self.write_msg(T_BRIGHT, &[raw])
    }

    pub fn send_video(&mut self, h264: &[u8]) -> Result<()> {
        self.write_msg(T_VIDEO, h264)
    }

    pub fn info(&self) -> &Option<DeviceInfo> {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use super::percent_to_raw;

    #[test]
    fn percent_to_raw_endpoints() {
        assert_eq!(percent_to_raw(1), 3); // (1*255+50)/100 = 3.05 -> 3
        assert_eq!(percent_to_raw(100), 255);
    }

    #[test]
    fn percent_to_raw_midpoint() {
        assert_eq!(percent_to_raw(50), 128); // (50*255+50)/100 = 127.5+0.5 rounding -> 128
    }

    #[test]
    fn percent_to_raw_clamps_out_of_range() {
        // Guards against a pre-Phase-2b config that still carries a raw 0..=255 value.
        assert_eq!(percent_to_raw(0), percent_to_raw(1));
        assert_eq!(percent_to_raw(200), percent_to_raw(100));
        assert_eq!(percent_to_raw(255), 255);
    }
}
