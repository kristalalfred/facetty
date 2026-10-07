/// Streaming linear-interpolation resampler for mono audio. Good enough for
/// voice between the usual 44.1/48 kHz device rates.
pub struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Resampler {
            step: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            last: 0.0,
        }
    }

    pub fn is_passthrough(&self) -> bool {
        self.step == 1.0
    }

    /// Appends the resampled `input` to `out`. State carries across calls, so
    /// chunk boundaries do not click.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        if input.is_empty() {
            return;
        }
        let n = input.len() as f64;
        let at = |i: isize| if i < 0 { self.last } else { input[i as usize] };
        while self.pos < n - 1.0 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = at(i as isize);
            let b = at(i as isize + 1);
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        self.last = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, to: u32, seconds: usize, chunk: usize) -> Vec<f32> {
        let input: Vec<f32> = (0..from as usize * seconds)
            .map(|i| (2.0 * std::f32::consts::PI * 300.0 * i as f32 / from as f32).sin())
            .collect();
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            r.process(c, &mut out);
        }
        out
    }

    #[test]
    fn output_length_tracks_rate_ratio() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (48_000, 48_000),
        ] {
            let out = run(from, to, 2, 441);
            let expected = 2 * to as usize;
            assert!(
                out.len().abs_diff(expected) <= 2,
                "{from}->{to}: {}",
                out.len()
            );
        }
    }

    #[test]
    fn preserves_a_low_tone() {
        let out = run(44_100, 48_000, 1, 512);
        let expected: Vec<f32> = (0..out.len())
            .map(|i| (2.0 * std::f32::consts::PI * 300.0 * i as f32 / 48_000.0).sin())
            .collect();
        let max_err = out
            .iter()
            .zip(&expected)
            .skip(1)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 0.02, "max error {max_err}");
    }
}
