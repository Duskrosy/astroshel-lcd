use anyhow::Result;
use openh264::OpenH264API;
use openh264::encoder::{Encoder, EncoderConfig, Profile, SpsPpsStrategy};
use openh264::formats::YUVSlices;

/// Wraps an openh264 encoder configured to emit a fully self-contained
/// Constrained-Baseline IDR (SPS + PPS + IDR slice) on every call, since the
/// target device receives one keyframe per second with nothing in between.
pub struct FrameEncoder {
    enc: Encoder,
    w: usize,
    h: usize,
}

/// Creates a new keyframe-only encoder for the given (even) dimensions.
pub fn new_encoder(w: u32, h: u32) -> Result<FrameEncoder> {
    let config = EncoderConfig::new()
        .profile(Profile::Baseline)
        // Keep the SPS/PPS id constant across frames -- combined with forcing
        // an intra frame before every encode() call below, this makes openh264
        // re-emit SPS+PPS ahead of each IDR rather than only once per stream.
        .sps_pps_strategy(SpsPpsStrategy::ConstantId);
    let api = OpenH264API::from_source();
    let enc = Encoder::with_api_config(api, config).map_err(|e| anyhow::anyhow!("openh264 init: {e}"))?;
    Ok(FrameEncoder { enc, w: w as usize, h: h as usize })
}

impl FrameEncoder {
    /// Encode one RGBA frame as a self-contained IDR keyframe, returning Annex-B bytes
    /// (SPS NAL 0x67 + PPS NAL 0x68 + IDR NAL 0x65).
    pub fn encode_keyframe(&mut self, rgba: &[u8]) -> Result<Vec<u8>> {
        let (y, u, v) = rgba_to_i420(rgba, self.w, self.h);
        let strides = (self.w, self.w / 2, self.w / 2);
        let yuv = YUVSlices::new((&y, &u, &v), (self.w, self.h), strides);

        // Force every frame -- including the very first -- to be an IDR with
        // fresh parameter sets, since each call must stand alone.
        self.enc.force_intra_frame();
        let bitstream = self.enc.encode(&yuv).map_err(|e| anyhow::anyhow!("openh264 encode: {e}"))?;
        Ok(bitstream.to_vec())
    }
}

/// Convert packed RGBA to I420 (BT.601 limited range). U/V are 2x2 box-averaged.
pub fn rgba_to_i420(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let cw = w / 2;
    let ch = h / 2;
    let mut yp = vec![0u8; w * h];
    let mut up = vec![0u8; cw * ch];
    let mut vp = vec![0u8; cw * ch];

    let clamp = |x: f32| x.round().clamp(0.0, 255.0) as u8;

    for j in 0..h {
        for i in 0..w {
            let p = (j * w + i) * 4;
            let r = rgba[p] as f32;
            let g = rgba[p + 1] as f32;
            let b = rgba[p + 2] as f32;
            let y = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
            yp[j * w + i] = clamp(y);
        }
    }
    // 2x2 averaged chroma
    for cj in 0..ch {
        for ci in 0..cw {
            let mut rs = 0.0;
            let mut gs = 0.0;
            let mut bs = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((cj * 2 + dy) * w + (ci * 2 + dx)) * 4;
                    rs += rgba[p] as f32;
                    gs += rgba[p + 1] as f32;
                    bs += rgba[p + 2] as f32;
                }
            }
            let (r, g, b) = (rs / 4.0, gs / 4.0, bs / 4.0);
            let u = -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
            let v = 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
            up[cj * cw + ci] = clamp(u);
            vp[cj * cw + ci] = clamp(v);
        }
    }
    (yp, up, vp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i420_plane_sizes_320() {
        let rgba = vec![0u8; 320 * 320 * 4];
        let (y, u, v) = rgba_to_i420(&rgba, 320, 320);
        assert_eq!(y.len(), 320 * 320);
        assert_eq!(u.len(), 160 * 160);
        assert_eq!(v.len(), 160 * 160);
    }

    #[test]
    fn pure_white_maps_high_luma() {
        let rgba = vec![255u8; 4 * 4 * 4]; // 4x4 white
        let (y, u, v) = rgba_to_i420(&rgba, 4, 4);
        assert!(y[0] >= 234, "white luma should be ~235-255, got {}", y[0]);
        // white chroma is neutral (~128)
        assert!((u[0] as i32 - 128).abs() <= 2);
        assert!((v[0] as i32 - 128).abs() <= 2);
    }

    #[test]
    fn pure_red_chroma() {
        let rgba = vec![255,0,0,255, 255,0,0,255, 255,0,0,255, 255,0,0,255]; // 2x2 red
        let (_y, _u, v) = rgba_to_i420(&rgba, 2, 2);
        assert!(v[0] > 200, "red should have high V (Cr), got {}", v[0]);
    }
}

#[cfg(test)]
mod enc_tests {
    use super::*;

    fn has_start_code(bytes: &[u8], nal_type_byte: u8) -> bool {
        bytes.windows(5).any(|w| w == [0, 0, 0, 1, nal_type_byte])
    }

    #[test]
    fn encodes_320_keyframe_annexb() {
        let mut enc = new_encoder(320, 320).expect("encoder");
        let rgba = vec![40u8; 320 * 320 * 4];
        let bytes = enc.encode_keyframe(&rgba).expect("encode");
        assert!(bytes.len() > 20, "expected non-trivial bitstream");
        // Annex-B start code + SPS (NAL type 7) should appear near the front
        assert!(has_start_code(&bytes, 0x67), "expected SPS NAL (00 00 00 01 67) in keyframe");
        assert!(has_start_code(&bytes, 0x68), "expected PPS NAL (00 00 00 01 68) in keyframe");
        assert!(has_start_code(&bytes, 0x65), "expected IDR NAL (00 00 00 01 65) in keyframe");
    }

    #[test]
    fn second_keyframe_is_also_self_contained() {
        // Device behavior requirement: it receives ONE keyframe per second with
        // nothing in between, so SPS/PPS must accompany every IDR, not just the first.
        let mut enc = new_encoder(320, 320).expect("encoder");
        let rgba = vec![40u8; 320 * 320 * 4];

        let first = enc.encode_keyframe(&rgba).expect("encode 1");
        assert!(has_start_code(&first, 0x67) && has_start_code(&first, 0x68) && has_start_code(&first, 0x65));

        let second = enc.encode_keyframe(&rgba).expect("encode 2");
        assert!(has_start_code(&second, 0x67), "2nd call missing SPS (00 00 00 01 67)");
        assert!(has_start_code(&second, 0x68), "2nd call missing PPS (00 00 00 01 68)");
        assert!(has_start_code(&second, 0x65), "2nd call missing IDR (00 00 00 01 65)");
    }
}
