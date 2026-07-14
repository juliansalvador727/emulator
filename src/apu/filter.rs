// First-order IIR approximations of the analog filters on the NES audio
// output path (https://www.nesdev.org/wiki/APU_Mixer): a high-pass at
// 90 Hz, a high-pass at 440 Hz and a low-pass at 14 kHz. They run at the
// host sample rate rather than the CPU clock; all three cutoffs sit far
// below the 22 kHz Nyquist limit of a 44.1 kHz output, so the difference
// is inaudible. The high-passes also remove the mixer's DC offset.

use std::f32::consts::PI;

#[derive(Clone)]
pub struct HighPassFilter {
    alpha: f32,
    prev_input: f32,
    prev_output: f32,
}

impl HighPassFilter {
    pub fn new(cutoff_hz: f32, sample_rate: f32) -> Self {
        let rc = 1.0 / (2.0 * PI * cutoff_hz);
        let dt = 1.0 / sample_rate;
        HighPassFilter {
            alpha: rc / (rc + dt),
            prev_input: 0.0,
            prev_output: 0.0,
        }
    }

    pub fn process(&mut self, input: f32) -> f32 {
        let output = self.alpha * (self.prev_output + input - self.prev_input);
        self.prev_input = input;
        self.prev_output = output;
        output
    }
}

#[derive(Clone)]
pub struct LowPassFilter {
    alpha: f32,
    prev_output: f32,
}

impl LowPassFilter {
    pub fn new(cutoff_hz: f32, sample_rate: f32) -> Self {
        let rc = 1.0 / (2.0 * PI * cutoff_hz);
        let dt = 1.0 / sample_rate;
        LowPassFilter {
            alpha: dt / (rc + dt),
            prev_output: 0.0,
        }
    }

    pub fn process(&mut self, input: f32) -> f32 {
        self.prev_output += self.alpha * (input - self.prev_output);
        self.prev_output
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn high_pass_blocks_dc() {
        let mut hp = HighPassFilter::new(90.0, 44100.0);
        let mut out = 0.0;
        for _ in 0..44100 {
            out = hp.process(0.5);
        }
        assert!(out.abs() < 0.001);
    }

    #[test]
    fn low_pass_settles_on_dc() {
        let mut lp = LowPassFilter::new(14000.0, 44100.0);
        let mut out = 0.0;
        for _ in 0..1000 {
            out = lp.process(0.5);
        }
        assert!((out - 0.5).abs() < 0.001);
    }
}
