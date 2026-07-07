// https://www.nesdev.org/wiki/APU_Pulse
// https://www.nesdev.org/wiki/APU_Sweep
//
// Square wave channel: an 11-bit timer clocks an 8-step duty sequencer at
// the APU rate (every 2nd CPU cycle), gated by the envelope, sweep and
// length counter. Pulse 1 and pulse 2 differ only in the sweep unit's
// negate arithmetic.

use crate::apu::envelope::Envelope;
use crate::apu::length_counter::LengthCounter;

// Waveforms selected by the duty field of $4000/$4004 (bits 7-6).
const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0], // 12.5%
    [0, 1, 1, 0, 0, 0, 0, 0], // 25%
    [0, 1, 1, 1, 1, 0, 0, 0], // 50%
    [1, 0, 0, 1, 1, 1, 1, 1], // 25% negated
];

pub struct Pulse {
    // Pulse 1's sweep adds the ones' complement of the change amount,
    // pulse 2 the two's complement (see sweep_target()).
    is_pulse1: bool,

    duty: u8,
    duty_step: u8,
    timer_period: u16, // 11-bit reload value from $4002/$4003
    timer: u16,

    pub envelope: Envelope,
    pub length: LengthCounter,

    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_divider: u8,
    sweep_reload: bool,
}

impl Pulse {
    pub fn new(is_pulse1: bool) -> Self {
        Pulse {
            is_pulse1,
            duty: 0,
            duty_step: 0,
            timer_period: 0,
            timer: 0,
            envelope: Envelope::new(),
            length: LengthCounter::new(),
            sweep_enabled: false,
            sweep_period: 0,
            sweep_negate: false,
            sweep_shift: 0,
            sweep_divider: 0,
            sweep_reload: false,
        }
    }

    // $4000/$4004: DDLC VVVV
    pub fn write_control(&mut self, data: u8) {
        self.duty = data >> 6;
        self.envelope.write(data);
        self.length.halt = data & 0x20 != 0;
    }

    // $4001/$4005: EPPP NSSS
    pub fn write_sweep(&mut self, data: u8) {
        self.sweep_enabled = data & 0x80 != 0;
        self.sweep_period = (data >> 4) & 0x07;
        self.sweep_negate = data & 0x08 != 0;
        self.sweep_shift = data & 0x07;
        self.sweep_reload = true;
    }

    // $4002/$4006: timer low byte
    pub fn write_timer_lo(&mut self, data: u8) {
        self.timer_period = (self.timer_period & 0x0700) | data as u16;
    }

    // $4003/$4007: LLLL LTTT - also restarts the sequencer and envelope
    pub fn write_timer_hi(&mut self, data: u8) {
        self.timer_period = (self.timer_period & 0x00ff) | (((data & 0x07) as u16) << 8);
        self.length.load(data >> 3);
        self.duty_step = 0;
        self.envelope.restart();
    }

    // Clocked every APU cycle (every 2nd CPU cycle). The divider counts
    // P, P-1, ..., 0 and steps the sequencer when it wraps, so the sequencer
    // advances every P+1 APU cycles.
    pub fn tick_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.timer_period;
            self.duty_step = (self.duty_step + 1) & 7;
        } else {
            self.timer -= 1;
        }
    }

    // The period the sweep is aiming for. The change amount is the current
    // period shifted right by the shift count; in negate mode pulse 1 adds
    // the ones' complement (-change - 1) and pulse 2 the two's complement
    // (-change). A negative target is treated as 0.
    fn sweep_target(&self) -> u16 {
        let change = (self.timer_period >> self.sweep_shift) as i32;
        let target = if self.sweep_negate {
            let carry = if self.is_pulse1 { 1 } else { 0 };
            self.timer_period as i32 - change - carry
        } else {
            self.timer_period as i32 + change
        };
        target.max(0) as u16
    }

    // Muting applies continuously, regardless of the sweep enable flag or
    // divider state: the channel is silenced whenever the current period is
    // below 8 or the target period overflows 11 bits.
    fn sweep_muting(&self) -> bool {
        self.timer_period < 8 || self.sweep_target() > 0x07ff
    }

    // Half-frame clock, per the wiki's sweep update logic: the period is
    // only adjusted when the divider fires while the sweep is enabled with a
    // nonzero shift count and the channel is not muted.
    pub fn tick_sweep(&mut self) {
        if self.sweep_divider == 0
            && self.sweep_enabled
            && self.sweep_shift > 0
            && !self.sweep_muting()
        {
            self.timer_period = self.sweep_target();
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }

    pub fn output(&self) -> u8 {
        if DUTY_TABLE[self.duty as usize][self.duty_step as usize] == 0
            || self.sweep_muting()
            || !self.length.active()
        {
            0
        } else {
            self.envelope.volume()
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // A pulse with length loaded, constant volume 15, 50% duty, period 8.
    fn audible_pulse() -> Pulse {
        let mut p = Pulse::new(true);
        p.length.set_enabled(true);
        p.write_control(0b1001_1111); // duty 2, constant volume 15
        p.write_timer_lo(0x08);
        p.write_timer_hi(0x00); // loads length index 0 -> 10
        p
    }

    // Step the timer until the sequencer advances once.
    fn clock_sequencer(p: &mut Pulse) {
        for _ in 0..(p.timer_period + 1) {
            p.tick_timer();
        }
    }

    #[test]
    fn duty_sequence_produces_square_wave() {
        let mut p = audible_pulse();
        // duty 2 -> 0,1,1,1,1,0,0,0; sequencer starts at step 0
        let mut outputs = vec![p.output()];
        for _ in 0..7 {
            clock_sequencer(&mut p);
            outputs.push(p.output());
        }
        assert_eq!(outputs, vec![0, 15, 15, 15, 15, 0, 0, 0]);
    }

    #[test]
    fn write_timer_hi_resets_duty_phase() {
        let mut p = audible_pulse();
        clock_sequencer(&mut p);
        assert_eq!(p.output(), 15); // step 1
        p.write_timer_hi(0x00);
        assert_eq!(p.output(), 0); // back to step 0
    }

    #[test]
    fn silent_when_length_counter_empty() {
        let mut p = audible_pulse();
        clock_sequencer(&mut p); // step 1: high
        p.length.set_enabled(false);
        assert_eq!(p.output(), 0);
    }

    #[test]
    fn muted_when_period_below_8() {
        let mut p = audible_pulse();
        clock_sequencer(&mut p); // step 1: high
        p.write_timer_lo(0x07);
        assert_eq!(p.output(), 0);
    }

    #[test]
    fn muted_when_sweep_target_overflows() {
        let mut p = audible_pulse();
        clock_sequencer(&mut p); // step 1: high
        assert_eq!(p.output(), 15);
        // period $400, shift 0, add mode -> target $800 > $7FF
        p.write_timer_lo(0x00);
        p.write_timer_hi(0x04);
        clock_sequencer(&mut p);
        assert_eq!(p.output(), 0);
    }

    #[test]
    fn sweep_negate_pulse1_uses_ones_complement() {
        let mut p = Pulse::new(true);
        p.timer_period = 0x200;
        p.write_sweep(0b1000_1001); // enabled, period 0, negate, shift 1
        assert_eq!(p.sweep_target(), 0x200 - 0x100 - 1);
    }

    #[test]
    fn sweep_negate_pulse2_uses_twos_complement() {
        let mut p = Pulse::new(false);
        p.timer_period = 0x200;
        p.write_sweep(0b1000_1001);
        assert_eq!(p.sweep_target(), 0x200 - 0x100);
    }

    #[test]
    fn sweep_updates_period_when_divider_fires() {
        let mut p = audible_pulse();
        p.write_timer_lo(0x00);
        p.write_timer_hi(0x01); // period 0x100
        p.write_sweep(0b1000_0001); // enabled, divider period 0, add, shift 1
        // Divider period 0 fires on every half-frame clock.
        p.tick_sweep(); // period += period >> 1
        assert_eq!(p.timer_period, 0x180);
        p.tick_sweep();
        assert_eq!(p.timer_period, 0x240);
    }

    #[test]
    fn sweep_divider_period_delays_updates() {
        let mut p = audible_pulse();
        p.write_timer_lo(0x00);
        p.write_timer_hi(0x01); // period 0x100
        p.write_sweep(0b1001_0001); // enabled, divider period 1, add, shift 1
        p.tick_sweep(); // divider 0: updates, divider reloaded to 1
        assert_eq!(p.timer_period, 0x180);
        p.tick_sweep(); // divider 1 -> 0, no update
        assert_eq!(p.timer_period, 0x180);
        p.tick_sweep(); // divider 0: updates again
        assert_eq!(p.timer_period, 0x240);
    }

    #[test]
    fn sweep_disabled_does_not_change_period() {
        let mut p = audible_pulse();
        p.write_timer_lo(0x00);
        p.write_timer_hi(0x01);
        p.write_sweep(0b0000_0001); // disabled, shift 1
        for _ in 0..4 {
            p.tick_sweep();
        }
        assert_eq!(p.timer_period, 0x100);
    }
}
