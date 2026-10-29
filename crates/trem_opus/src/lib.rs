//! The libopus calls of tremulous-legacy's VoIP (client/cl_cgame.c
//! `CL_FirstSnapshot`'s `opus_encoder_create` / `opus_decoder_create`,
//! cl_main.c `CL_CaptureVoip`'s `opus_encode`, cl_parse.c `CL_ParseVoip`'s
//! `opus_decode` with packet-loss concealment, `OPUS_RESET_STATE`) over the
//! libopus 1.6.1 (third_party/opus) that build.rs compiles (the reference
//! tree bundles 1.1; the bitstream is the same RFC 6716 Opus, so voice still
//! interoperates). Owned handles free themselves
//! (`opus_encoder_destroy` / `opus_decoder_destroy` of `CL_Disconnect`).

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uchar};

#[repr(C)]
struct OpusEncoderRaw {
    _p: [u8; 0],
}
#[repr(C)]
struct OpusDecoderRaw {
    _p: [u8; 0],
}

extern "C" {
    fn opus_encoder_create(fs: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut OpusEncoderRaw;
    fn opus_encode(st: *mut OpusEncoderRaw, pcm: *const i16, frame_size: c_int, data: *mut c_uchar, max_data_bytes: i32) -> i32;
    fn opus_encoder_destroy(st: *mut OpusEncoderRaw);
    fn opus_encoder_ctl(st: *mut OpusEncoderRaw, request: c_int, ...) -> c_int;
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoderRaw;
    fn opus_decode(st: *mut OpusDecoderRaw, data: *const c_uchar, len: i32, pcm: *mut i16, frame_size: c_int, decode_fec: c_int) -> c_int;
    fn opus_decoder_destroy(st: *mut OpusDecoderRaw);
    fn opus_decoder_ctl(st: *mut OpusDecoderRaw, request: c_int, ...) -> c_int;
    fn opus_get_version_string() -> *const c_char;
}

/// `opus_get_version_string()`: "libopus 1.6.1".
pub fn version() -> String {
    unsafe { CStr::from_ptr(opus_get_version_string()) }.to_string_lossy().into_owned()
}

/// `OPUS_APPLICATION_VOIP`.
pub const OPUS_APPLICATION_VOIP: i32 = 2048;
/// `OPUS_RESET_STATE`.
pub const OPUS_RESET_STATE: i32 = 4028;
/// `OPUS_OK`.
pub const OPUS_OK: i32 = 0;

/// An `OpusEncoder *`.
pub struct Encoder {
    st: *mut OpusEncoderRaw,
}

// the handle is plain heap memory owned by this value
unsafe impl Send for Encoder {}

impl Encoder {
    /// `opus_encoder_create( fs, channels, application, &error )`.
    pub fn new(fs: i32, channels: i32, application: i32) -> Result<Encoder, i32> {
        let mut error: c_int = 0;
        let st = unsafe { opus_encoder_create(fs, channels, application, &mut error) };
        if error != OPUS_OK || st.is_null() {
            return Err(error);
        }
        Ok(Encoder { st })
    }

    /// `opus_encode`: `pcm.len()` samples (one channel) into `out`; the
    /// byte count, or the (negative) opus error.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> i32 {
        unsafe { opus_encode(self.st, pcm.as_ptr(), pcm.len() as c_int, out.as_mut_ptr(), out.len() as i32) }
    }

    /// `opus_encoder_ctl( st, OPUS_RESET_STATE )`.
    pub fn reset(&mut self) {
        unsafe {
            opus_encoder_ctl(self.st, OPUS_RESET_STATE);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.st) }
    }
}

/// An `OpusDecoder *`.
pub struct Decoder {
    st: *mut OpusDecoderRaw,
}

unsafe impl Send for Decoder {}

impl Decoder {
    /// `opus_decoder_create( fs, channels, &error )`.
    pub fn new(fs: i32, channels: i32) -> Result<Decoder, i32> {
        let mut error: c_int = 0;
        let st = unsafe { opus_decoder_create(fs, channels, &mut error) };
        if error != OPUS_OK || st.is_null() {
            return Err(error);
        }
        Ok(Decoder { st })
    }

    /// `opus_decode( st, data, len, pcm, frame_size, 0 )`; `None` data is a
    /// lost packet (concealment). The sample count, or the opus error.
    pub fn decode(&mut self, data: Option<&[u8]>, pcm: &mut [i16]) -> i32 {
        let (p, n) = match data {
            Some(d) => (d.as_ptr(), d.len() as i32),
            None => (std::ptr::null(), 0),
        };
        unsafe { opus_decode(self.st, p, n, pcm.as_mut_ptr(), pcm.len() as c_int, 0) }
    }

    /// `opus_decoder_ctl( st, OPUS_RESET_STATE )`.
    pub fn reset(&mut self) {
        unsafe {
            opus_decoder_ctl(self.st, OPUS_RESET_STATE);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.st) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 440 Hz tone at 48 kHz survives encode -> decode (after the codec's
    /// look-ahead delay) with a usable signal-to-noise ratio.
    #[test]
    fn tone_round_trip() {
        let mut enc = Encoder::new(48000, 1, OPUS_APPLICATION_VOIP).unwrap();
        let mut dec = Decoder::new(48000, 1).unwrap();
        let n = 2880;
        let total = n * 20;
        let src: Vec<i16> = (0..total).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 8000.0) as i16).collect();
        let mut out = Vec::new();
        for c in src.chunks(n) {
            let mut buf = [0u8; 1024];
            let b = enc.encode(c, &mut buf);
            assert!(b > 0, "encode {b}");
            let mut pcm = vec![0i16; n * 4];
            let s = dec.decode(Some(&buf[..b as usize]), &mut pcm);
            assert_eq!(s as usize, n);
            out.extend_from_slice(&pcm[..s as usize]);
        }
        // best alignment over the codec delay
        let best = (0..1000)
            .map(|d| {
                let (mut sig, mut err) = (0f64, 0f64);
                for i in 5000..total - 1000 {
                    let a = src[i] as f64;
                    let b = out[i + d] as f64;
                    sig += a * a;
                    err += (a - b) * (a - b);
                }
                10.0 * (sig / err.max(1.0)).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 10.0, "SNR {best} dB");
        // a lost packet is concealed
        let mut pcm = vec![0i16; n];
        assert_eq!(dec.decode(None, &mut pcm), n as i32);
        dec.reset();
        enc.reset();
    }

    /// The linked library is the vendored release, not an older copy.
    #[test]
    fn version_is_vendored_release() {
        let v = version();
        let want = include_str!("../../../third_party/opus/package_version");
        let want = want.trim().trim_start_matches("PACKAGE_VERSION=").trim_matches('"');
        assert_eq!(v, format!("libopus {want}"));
    }

    /// Hostile packets (random bytes, every TOC byte, truncations, oversize
    /// frame counts) come back as a sample count or an opus error, never a
    /// crash, and never more samples than the buffer holds.
    #[test]
    fn garbage_packets_are_rejected_or_decoded() {
        let mut dec = Decoder::new(48000, 1).unwrap();
        let mut pcm = vec![0i16; 2880];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..20_000 {
            let len = (rnd() % 1300) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            if !p.is_empty() {
                p[0] = (i % 256) as u8;
            }
            let r = dec.decode(Some(&p), &mut pcm);
            assert!(r <= pcm.len() as i32, "{r}");
            if i % 97 == 0 {
                dec.reset();
            }
        }
    }
}
