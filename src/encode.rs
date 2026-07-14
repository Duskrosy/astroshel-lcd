use anyhow::Result;
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile, RateControlMode, SpsPpsStrategy,
};
use openh264::formats::{YUVSlices, YUVSource};

/// Roughly how many frames elapse between periodic IDRs in the streaming encoder.
const STREAM_INTRA_PERIOD: u32 = 50;

/// Default/reference target bitrate (in kbps) for the streaming encoder's rate
/// control, used as the config default and the fallback if a caller doesn't have
/// a user-configured value handy. Bounds the size of any single encoded frame --
/// including drastic scene changes (lots of new colors / big motion) that would
/// otherwise balloon into an oversized packet and cause a decode/transfer hitch.
/// Now user-adjustable (see `config::MediaCfg::bitrate_kbps` and the GUI slider);
/// this constant remains the shipped default and a sane starting point for
/// smooth 320x320@25fps over the device link.
pub const DEFAULT_STREAM_BITRATE_KBPS: u32 = 1500;

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

/// Wraps an openh264 encoder configured for continuous streaming: Constrained
/// Baseline with self-contained SPS/PPS on every IDR (`ConstantId`), a periodic
/// IDR every [`STREAM_INTRA_PERIOD`] frames (via the encoder's own
/// `intra_frame_period` GOP setting), and plain P-frames in between. Unlike
/// [`FrameEncoder`], this does NOT force an intra frame on every call -- doing
/// so is what caused the media-wedge (all-IDR bitstream overran the device).
pub struct StreamEncoder {
    enc: Encoder,
    w: usize,
    h: usize,
    frame_count: u64,
    force_next: bool,
}

/// Creates a new streaming encoder for the given (even) dimensions, targeting
/// `bitrate_kbps` (kbps) for the rate control -- see `config::MediaCfg::bitrate_kbps`
/// for the user-facing setting this comes from.
pub fn new_stream_encoder(w: u32, h: u32, bitrate_kbps: u32) -> Result<StreamEncoder> {
    let config = EncoderConfig::new()
        .profile(Profile::Baseline)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        // Real openh264 GOP setting: periodic IDR every STREAM_INTRA_PERIOD
        // frames, with plain P-frames encoded in between automatically.
        .intra_frame_period(IntraFramePeriod::from_num_frames(STREAM_INTRA_PERIOD))
        // Bound per-frame size so a drastic scene change doesn't emit one
        // oversized packet: cap the target bitrate and put the encoder in
        // bitrate-based rate control (rather than the default quality mode).
        .bitrate(BitRate::from_bps(bitrate_kbps * 1000))
        .rate_control_mode(RateControlMode::Bitrate)
        .max_frame_rate(FrameRate::from_hz(25.0))
        // Enable frame-skip so the bitrate cap is actually ENFORCED: openh264
        // can only hold a bitrate target in Bitrate mode by skipping frames that
        // would blow the budget (it logs a warning and ignores the cap otherwise).
        // On a drastic scene change the encoder now drops that frame rather than
        // emitting one oversized packet that stalls the link -- the pre-encode
        // playback path treats an empty (skipped) packet as "hold the previous
        // frame for its delay", which reads as far smoother than a bandwidth hitch.
        .skip_frames(true);
    let api = OpenH264API::from_source();
    let enc = Encoder::with_api_config(api, config).map_err(|e| anyhow::anyhow!("openh264 init: {e}"))?;
    Ok(StreamEncoder { enc, w: w as usize, h: h as usize, frame_count: 0, force_next: true })
}

impl StreamEncoder {
    /// Encodes the next RGBA frame, returning Annex-B bytes. The very first
    /// frame (and any frame following [`Self::force_keyframe`]) is a
    /// self-contained IDR (SPS + PPS + IDR slice); the encoder's own periodic
    /// intra-period setting handles subsequent IDRs, and all other frames are
    /// encoded as P-frames referencing the previous frame.
    pub fn encode_frame(&mut self, rgba: &[u8]) -> Result<Vec<u8>> {
        let (y, u, v) = rgba_to_i420(rgba, self.w, self.h);
        let strides = (self.w, self.w / 2, self.w / 2);
        let yuv = YUVSlices::new((&y, &u, &v), (self.w, self.h), strides);

        if self.force_next {
            self.enc.force_intra_frame();
            self.force_next = false;
        }
        let bitstream = self.enc.encode(&yuv).map_err(|e| anyhow::anyhow!("openh264 encode: {e}"))?;
        self.frame_count += 1;
        Ok(bitstream.to_vec())
    }

    /// Makes the NEXT call to [`Self::encode_frame`] emit a self-contained IDR,
    /// regardless of where it falls in the periodic GOP. Used on device
    /// reconnect / stream start.
    pub fn force_keyframe(&mut self) {
        self.force_next = true;
    }
}

/// Wraps an openh264 decoder for in-process preview decode of OUR OWN cached
/// `.lcdv` packets (see `crate::cache`/`crate::video::import_to_cache`) -- no
/// ffmpeg involved. Used by the GUI to preview a cached video without shelling
/// out to a process.
pub struct VideoDecoder {
    dec: Decoder,
}

/// Creates a new openh264 decoder for previewing cached (self-encoded) H.264
/// Annex-B packets.
pub fn new_video_decoder() -> Result<VideoDecoder> {
    let api = OpenH264API::from_source();
    let dec = Decoder::with_api_config(api, DecoderConfig::new())
        .map_err(|e| anyhow::anyhow!("openh264 decoder init: {e}"))?;
    Ok(VideoDecoder { dec })
}

impl VideoDecoder {
    /// Decodes one Annex-B packet (as produced by `StreamEncoder::encode_frame`),
    /// returning the decoded frame as an owned `RgbaImage` once the decoder has
    /// enough data to produce one (`Ok(None)` for a packet that only carries
    /// SPS/PPS with no picture yet, or an empty/skipped packet).
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<image::RgbaImage>> {
        match self.dec.decode(annexb) {
            Ok(Some(yuv)) => {
                let mut buf = vec![0u8; yuv.rgba8_len()];
                yuv.write_rgba8(&mut buf);
                let (w, h) = yuv.dimensions();
                Ok(image::RgbaImage::from_raw(w as u32, h as u32, buf))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("openh264 decode: {e}")),
        }
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
mod stream_tests {
    use super::*;
    fn has(nal: &[u8], t: u8) -> bool { nal.windows(5).any(|w| w == [0,0,0,1,t]) || nal.windows(4).any(|w| w==[0,0,1,t]) }
    #[test]
    fn first_frame_is_idr_then_p_frames() {
        let mut e = new_stream_encoder(320,320,1500).unwrap();
        let a = vec![20u8; 320*320*4];
        let mut b = vec![20u8; 320*320*4];
        // vary frame 2 slightly so it's not identical (encoder may skip)
        for i in (0..b.len()).step_by(97) { b[i] = 200; }
        let f1 = e.encode_frame(&a).unwrap();
        assert!(has(&f1, 0x67), "first frame must carry SPS (IDR)");
        assert!(has(&f1, 0x65), "first frame must be IDR");
        // subsequent frames should be able to be P-frames (NAL type 1), not all-IDR
        let mut saw_p = false;
        for _ in 0..5 {
            let f = e.encode_frame(&b).unwrap();
            if has(&f, 0x61) || (!has(&f,0x65) && !f.is_empty()) { saw_p = true; }
        }
        assert!(saw_p, "streaming encoder should emit P-frames after the first IDR");
    }
    #[test]
    fn force_keyframe_makes_next_idr() {
        let mut e = new_stream_encoder(320,320,1500).unwrap();
        let a = vec![50u8; 320*320*4];
        let _ = e.encode_frame(&a).unwrap();
        e.force_keyframe();
        let f = e.encode_frame(&a).unwrap();
        assert!(has(&f, 0x65) && has(&f, 0x67), "forced frame must be a self-contained IDR");
    }
}

#[cfg(test)]
mod decoder_tests {
    use super::*;

    #[test]
    fn decodes_a_self_encoded_idr_frame() {
        let mut enc = new_stream_encoder(320, 320, 1500).unwrap();
        let rgba = vec![80u8; 320 * 320 * 4];
        let pkt = enc.encode_frame(&rgba).unwrap();

        let mut dec = new_video_decoder().expect("decoder init");
        let out = dec.decode(&pkt).expect("decode should not error");
        let img = out.expect("first (IDR) packet should decode to a frame");
        assert_eq!(img.dimensions(), (320, 320));
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
