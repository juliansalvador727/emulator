// https://www.nesdev.org/wiki/APU_Triangle
//
// The triangle steps through a fixed 32-entry 15..0,0..15 sequence. Unlike
// the pulses its timer runs at the CPU clock (so the wave is one octave
// lower for the same period), and it has no volume control: when silenced
// by the length or linear counter the sequencer simply stops stepping and
// the channel holds its last output value.

use crate::apu::length_counter::LengthCounter;

const SEQUENCE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, //
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

pub struct Triangle {
    // $4008 bit 7: halts the length counter and controls whether the linear
    // counter reload flag is cleared after a quarter-frame clock.
    control: bool,
    linear_reload_value: u8,
    linear_counter: u8,
    linear_reload: bool,

    timer_period: u16, // 11-bit reload value from $400A/$400B
    timer: u16,
    step: u8,

    pub length: LengthCounter,
}

impl Triangle {
    pub fn new() -> Self {
        Triangle {
            control: false,
            linear_reload_value: 0,
            linear_counter: 0,
            linear_reload: false,
            timer_period: 0,
            timer: 0,
            step: 0,
            length: LengthCounter::new(),
        }
    }

    // $4008: CRRR RRRR
    pub fn write_linear(&mut self, data: u8) {
        self.control = data & 0x80 != 0;
        self.linear_reload_value = data & 0x7f;
        self.length.halt = self.control;
    }

    // $400A: timer low byte
    pub fn write_timer_lo(&mut self, data: u8) {
        self.timer_period = (self.timer_period & 0x0700) | data as u16;
    }

    // $400B: LLLL LTTT - also sets the linear counter reload flag. The
    // sequencer phase is NOT reset (unlike the pulses).
    pub fn write_timer_hi(&mut self, data: u8) {
        self.timer_period = (self.timer_period & 0x00ff) | (((data & 0x07) as u16) << 8);
        self.length.load(data >> 3);
        self.linear_reload = true;
    }

    // Clocked every CPU cycle. The sequencer only advances while both the
    // linear counter and length counter are nonzero.
    pub fn tick_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.timer_period;
            if self.linear_counter > 0 && self.length.active() {
                self.step = (self.step + 1) & 31;
            }
        } else {
            self.timer -= 1;
        }
    }

    // Quarter-frame clock, per the wiki: reload or decrement the linear
    // counter, then clear the reload flag only if the control bit is clear.
    pub fn tick_linear(&mut self) {
        if self.linear_reload {
            self.linear_counter = self.linear_reload_value;
        } else if self.linear_counter > 0 {
            self.linear_counter -= 1;
        }
        if !self.control {
            self.linear_reload = false;
        }
    }

    pub fn output(&self) -> u8 {
        SEQUENCE[self.step as usize]
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Triangle with length + linear counters loaded, period 8.
    fn running_triangle() -> Triangle {
        let mut t = Triangle::new();
        t.length.set_enabled(true);
        t.write_linear(0x7f); // control clear, linear reload value 127
        t.write_timer_lo(0x08);
        t.write_timer_hi(0x00); // length index 0 -> 10, reload flag set
        t.tick_linear(); // load the linear counter
        t
    }

    // Step the timer until the sequencer would advance once.
    fn clock_sequencer(t: &mut Triangle) {
        for _ in 0..(t.timer_period + 1) {
            t.tick_timer();
        }
    }

    #[test]
    fn sequence_descends_then_ascends() {
        let mut t = running_triangle();
        assert_eq!(t.output(), 15);
        let mut outputs = vec![];
        for _ in 0..32 {
            clock_sequencer(&mut t);
            outputs.push(t.output());
        }
        assert_eq!(outputs[..16], [14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0]);
        assert_eq!(outputs[16..31], [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(outputs[31], 15); // wrapped back to step 0
    }

    #[test]
    fn sequencer_freezes_when_linear_counter_empty() {
        let mut t = Triangle::new();
        t.length.set_enabled(true);
        t.write_linear(0x00); // linear reload value 0
        t.write_timer_lo(0x08);
        t.write_timer_hi(0x00);
        t.tick_linear(); // linear counter = 0
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 15); // held at step 0
    }

    #[test]
    fn sequencer_freezes_when_length_counter_empty() {
        let mut t = running_triangle();
        t.length.set_enabled(false);
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 15);
    }

    #[test]
    fn output_holds_last_value_when_silenced() {
        let mut t = running_triangle();
        clock_sequencer(&mut t);
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 13);
        t.length.set_enabled(false);
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 13); // no volume gate; value is held
    }

    #[test]
    fn linear_counter_counts_down_when_control_clear() {
        let mut t = Triangle::new();
        t.length.set_enabled(true);
        t.write_linear(0x02); // control clear, reload value 2
        t.write_timer_hi(0x00); // sets reload flag
        t.tick_linear(); // reload -> 2, reload flag cleared (control clear)
        t.tick_linear(); // 2 -> 1
        t.tick_linear(); // 1 -> 0
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 15); // sequencer frozen at 0 steps
    }

    #[test]
    fn control_set_keeps_reloading_linear_counter() {
        let mut t = Triangle::new();
        t.length.set_enabled(true);
        t.write_linear(0x82); // control set, reload value 2
        t.write_timer_hi(0x00);
        t.tick_linear(); // reload -> 2; reload flag stays set
        t.tick_linear(); // reloads again instead of decrementing
        t.tick_linear();
        clock_sequencer(&mut t);
        assert_eq!(t.output(), 14); // still running
    }
}
