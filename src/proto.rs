pub const MAGIC: [u8; 2] = [0x5A, 0xA5];
pub const T_VIDEO: u8 = 0x85;
pub const T_BRIGHT: u8 = 0x80;
pub const T_HELLO: u8 = 0x81;
pub const T_RES: u8 = 0x90;

/// Build a host→device message: 5A A5 <type> 00 | len(u32 LE) | payload
pub fn frame(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(msg_type);
    out.push(0x00);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub width: u16,
    pub height: u16,
}

/// Parse one device reply: 5A A5 00 <type> | len(u32 LE) | payload
pub fn parse_reply(buf: &[u8]) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 8 || buf[0] != MAGIC[0] || buf[1] != MAGIC[1] || buf[2] != 0x00 {
        return None;
    }
    let msg_type = buf[3];
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if buf.len() < 8 + len {
        return None;
    }
    Some((msg_type, buf[8..8 + len].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_brightness_full() {
        // 5A A5 80 00 | 01 00 00 00 | FF
        assert_eq!(frame(T_BRIGHT, &[0xFF]),
            vec![0x5A,0xA5,0x80,0x00, 0x01,0x00,0x00,0x00, 0xFF]);
    }

    #[test]
    fn frames_hello() {
        assert_eq!(frame(T_HELLO, &[0x01]),
            vec![0x5A,0xA5,0x81,0x00, 0x01,0x00,0x00,0x00, 0x01]);
    }

    #[test]
    fn frames_video_length_le() {
        let payload = vec![0xAB; 300];
        let f = frame(T_VIDEO, &payload);
        assert_eq!(&f[0..4], &[0x5A,0xA5,0x85,0x00]);
        assert_eq!(&f[4..8], &[0x2C,0x01,0x00,0x00]); // 300 = 0x12C LE
        assert_eq!(f.len(), 8 + 300);
    }

    #[test]
    fn parses_resolution_reply() {
        // 5A A5 00 90 | 04 00 00 00 | 01 40 01 40  (device reply captured on hardware)
        let buf = [0x5A,0xA5,0x00,0x90, 0x04,0x00,0x00,0x00, 0x01,0x40,0x01,0x40];
        let (t, p) = parse_reply(&buf).unwrap();
        assert_eq!(t, 0x90);
        assert_eq!(p, vec![0x01,0x40,0x01,0x40]);
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(parse_reply(&[0x00,0x00,0x00,0x90, 0,0,0,0]).is_none());
    }
}
