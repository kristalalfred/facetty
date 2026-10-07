use anyhow::{Result, bail};
use unsafe_libopus::varargs::{VarArg, VarArgs};
use unsafe_libopus::{
    OPUS_APPLICATION_VOIP, OPUS_SET_BITRATE_REQUEST, OPUS_SET_INBAND_FEC_REQUEST,
    OPUS_SET_PACKET_LOSS_PERC_REQUEST, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE, OpusDecoder,
    OpusEncoder, opus_decode_float, opus_decoder_create, opus_decoder_destroy, opus_encode_float,
    opus_encoder_create, opus_encoder_ctl_impl, opus_encoder_destroy,
};

use crate::{FRAME_SAMPLES, SAMPLE_RATE};

const MAX_PACKET: usize = 1275;
const BITRATE: i32 = 24_000;
const EXPECTED_LOSS_PERCENT: i32 = 10;

pub struct Encoder(*mut OpusEncoder);

// SAFETY: the encoder state is a heap allocation owned exclusively by this
// handle; libopus keeps no thread-local or shared state.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        let st =
            unsafe { opus_encoder_create(SAMPLE_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut err) };
        if st.is_null() || err != 0 {
            bail!("opus_encoder_create failed: {err}");
        }
        let enc = Encoder(st);
        enc.ctl(OPUS_SET_BITRATE_REQUEST, BITRATE)?;
        enc.ctl(OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE)?;
        enc.ctl(OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        enc.ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST, EXPECTED_LOSS_PERCENT)?;
        Ok(enc)
    }

    fn ctl(&self, request: i32, value: i32) -> Result<()> {
        let ret = unsafe {
            opus_encoder_ctl_impl(self.0, request, VarArgs::new(vec![VarArg::I32(value)]))
        };
        if ret != 0 {
            bail!("opus encoder ctl {request} = {value} failed: {ret}");
        }
        Ok(())
    }

    pub fn encode(&mut self, pcm: &[f32; FRAME_SAMPLES]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; MAX_PACKET];
        let n = unsafe {
            opus_encode_float(
                self.0,
                pcm.as_ptr(),
                FRAME_SAMPLES as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 {
            bail!("opus_encode_float failed: {n}");
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) }
    }
}

pub struct Decoder(*mut OpusDecoder);

// SAFETY: see `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut err = 0;
        let st = unsafe { opus_decoder_create(SAMPLE_RATE as i32, 1, &mut err) };
        if st.is_null() || err != 0 {
            bail!("opus_decoder_create failed: {err}");
        }
        Ok(Decoder(st))
    }

    pub fn decode(&mut self, packet: &[u8], out: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.run(packet, out, false)
    }

    /// Recovers the frame *before* `next_packet` from its in-band FEC data.
    pub fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.run(next_packet, out, true)
    }

    pub fn conceal(&mut self, out: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.run(&[], out, false)
    }

    fn run(&mut self, packet: &[u8], out: &mut [f32; FRAME_SAMPLES], fec: bool) -> Result<()> {
        let data = if packet.is_empty() {
            std::ptr::null()
        } else {
            packet.as_ptr()
        };
        let n = unsafe {
            opus_decode_float(
                self.0,
                data,
                packet.len() as i32,
                out.as_mut_ptr(),
                FRAME_SAMPLES as i32,
                fec as i32,
            )
        };
        if n < 0 {
            bail!("opus_decode_float failed: {n}");
        }
        out[n as usize..].fill(0.0);
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frame: usize) -> [f32; FRAME_SAMPLES] {
        std::array::from_fn(|i| {
            let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
        })
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32
    }

    #[test]
    fn sine_survives_roundtrip() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = [0f32; FRAME_SAMPLES];
        let mut sizes = Vec::new();
        for f in 0..25 {
            let packet = enc.encode(&sine(f)).unwrap();
            sizes.push(packet.len());
            dec.decode(&packet, &mut out).unwrap();
        }
        let e_in = energy(&sine(24));
        let e_out = energy(&out);
        assert!((e_out / e_in - 1.0).abs() < 0.3, "in {e_in} out {e_out}");
        let avg = sizes.iter().sum::<usize>() / sizes.len();
        assert!(
            avg < 120,
            "average packet {avg} bytes, expected ~60 at 24 kbps"
        );
    }

    #[test]
    fn fec_and_plc_produce_audio() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let packets: Vec<_> = (0..20).map(|f| enc.encode(&sine(f)).unwrap()).collect();
        let mut out = [0f32; FRAME_SAMPLES];
        for p in &packets[..10] {
            dec.decode(p, &mut out).unwrap();
        }
        dec.decode_fec(&packets[11], &mut out).unwrap();
        assert!(energy(&out) > 0.01);
        dec.decode(&packets[11], &mut out).unwrap();
        dec.conceal(&mut out).unwrap();
        assert!(energy(&out) > 0.001);
    }
}
