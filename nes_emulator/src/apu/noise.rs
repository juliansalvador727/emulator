// https://www.nesdev.org/wiki/APU_Noise
//
// Pseudo-random channel: a 15-bit linear feedback shift register clocked by
// a timer, gated by the envelope and length counter. Mode 1 taps bit 6
// instead of bit 1, giving a short 93-step "metallic" loop.

use crate::apu::envelope::Envelope;
use crate::apu::length_counter::LengthCounter;

// NTSC timer periods, in CPU cycles, indexed by the low nibble of $400E.
const PERIOD_TABLE: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];

pub struct Noise {
    pub envelope: Envelope,
    pub length: LengthCounter,

    mode: bool, // $400E bit 7
    timer_period: u16,
    timer: u16,
    shift: u16, // 15-bit LFSR, loaded with 1 on power-up
}

impl Noise {
    pub fn new() -> Self {
        Noise {
            envelope: Envelope::new(),
            length: LengthCounter::new(),
            mode: false,
            timer_period: PERIOD_TABLE[0],
            timer: 0,
            shift: 1,
        }
    }

    // $400C: --LC VVVV
    pub fn write_control(&mut self, data: u8) {
        self.envelope.write(data);
        self.length.halt = data & 0x20 != 0;
    }

    // $400E: M--- PPPP
    pub fn write_mode_period(&mut self, data: u8) {
        self.mode = data & 0x80 != 0;
        self.timer_period = PERIOD_TABLE[(data & 0x0f) as usize];
    }

    // $400F: LLLL L--- - also restarts the envelope
    pub fn write_length(&mut self, data: u8) {
        self.length.load(data >> 3);
        self.envelope.restart();
    }

    // Clocked every CPU cycle; the table above is already in CPU cycles, so
    // reload with period-1 to clock the LFSR once every `timer_period`
    // cycles.
    pub fn tick_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.timer_period - 1;
            self.clock_lfsr();
        } else {
            self.timer -= 1;
        }
    }

    fn clock_lfsr(&mut self) {
        let tap = if self.mode {
            (self.shift >> 6) & 1
        } else {
            (self.shift >> 1) & 1
        };
        let feedback = (self.shift & 1) ^ tap;
        self.shift >>= 1;
        self.shift |= feedback << 14;
    }

    pub fn output(&self) -> u8 {
        if self.shift & 1 == 1 || !self.length.active() {
            0
        } else {
            self.envelope.volume()
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn lfsr_mode0_feedback_from_bit1() {
        let mut n = Noise::new();
        // shift = 1: feedback = bit0 ^ bit1 = 1 ^ 0 = 1
        n.clock_lfsr();
        assert_eq!(n.shift, 0x4000);
        // shift = 0x4000: feedback = 0 ^ 0 = 0
        n.clock_lfsr();
        assert_eq!(n.shift, 0x2000);
    }

    #[test]
    fn lfsr_mode1_feedback_from_bit6() {
        let mut n = Noise::new();
        n.write_mode_period(0x80);
        n.shift = 0x0040; // bit 6 set
        // feedback = bit0 ^ bit6 = 0 ^ 1 = 1
        n.clock_lfsr();
        assert_eq!(n.shift, 0x4020);
    }

    #[test]
    fn lfsr_mode0_has_maximal_period() {
        let mut n = Noise::new();
        // A 15-bit maximal LFSR repeats after 2^15 - 1 = 32767 clocks and
        // never earlier.
        for i in 1..=32767u32 {
            n.clock_lfsr();
            if n.shift == 1 {
                assert_eq!(i, 32767);
                return;
            }
        }
        panic!("LFSR never returned to its seed");
    }

    #[test]
    fn timer_clocks_lfsr_every_period_cycles() {
        let mut n = Noise::new();
        n.write_mode_period(0x01); // period 8
        for _ in 0..8 {
            n.tick_timer();
        }
        assert_eq!(n.shift, 0x4000); // exactly one LFSR clock
        for _ in 0..8 {
            n.tick_timer();
        }
        assert_eq!(n.shift, 0x2000);
    }

    #[test]
    fn silent_when_bit0_set_or_length_empty() {
        let mut n = Noise::new();
        n.length.set_enabled(true);
        n.write_control(0x1f); // constant volume 15
        n.write_length(0x00); // length index 0 -> 10
        assert_eq!(n.output(), 0); // shift = 1, bit 0 set
        n.clock_lfsr(); // shift = 0x4000
        assert_eq!(n.output(), 15);
        n.length.set_enabled(false);
        assert_eq!(n.output(), 0);
    }
}
