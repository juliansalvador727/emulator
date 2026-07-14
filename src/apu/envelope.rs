// https://www.nesdev.org/wiki/APU_Envelope
//
// Volume envelope shared by the pulse and noise channels. On each quarter
// frame clock the divider counts down; each time it fires, the decay level
// steps 15 -> 0, wrapping back to 15 when the loop flag is set.

#[derive(Clone)]
pub struct Envelope {
    // Register fields ($4000/$4004/$400C bits 5-0). The loop flag is the
    // same bit as the channel's length counter halt flag.
    pub loop_flag: bool,
    pub constant_volume: bool,
    pub period: u8, // V: divider period, and the volume when constant

    start: bool,
    divider: u8,
    decay: u8,
}

impl Envelope {
    pub fn new() -> Self {
        Envelope {
            loop_flag: false,
            constant_volume: false,
            period: 0,
            start: false,
            divider: 0,
            decay: 0,
        }
    }

    // Update from a $4000/$4004/$400C write (--LC VVVV).
    pub fn write(&mut self, data: u8) {
        self.loop_flag = data & 0x20 != 0;
        self.constant_volume = data & 0x10 != 0;
        self.period = data & 0x0f;
    }

    // Side effect of writing $4003/$4007/$400F: sets the start flag so the
    // next quarter-frame clock reloads the decay level.
    pub fn restart(&mut self) {
        self.start = true;
    }

    // Quarter-frame clock.
    pub fn tick(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.period;
        } else if self.divider > 0 {
            self.divider -= 1;
        } else {
            self.divider = self.period;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.loop_flag {
                self.decay = 15;
            }
        }
    }

    pub fn volume(&self) -> u8 {
        if self.constant_volume {
            self.period
        } else {
            self.decay
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn restart_reloads_decay_on_next_clock() {
        let mut env = Envelope::new();
        env.write(0x04); // period 4, not constant, no loop
        env.restart();
        env.tick();
        assert_eq!(env.volume(), 15);
    }

    #[test]
    fn decay_steps_every_period_plus_one_clocks() {
        let mut env = Envelope::new();
        env.write(0x01); // period 1 -> decay steps every 2 clocks
        env.restart();
        env.tick(); // start: decay = 15, divider = 1
        env.tick(); // divider 1 -> 0
        assert_eq!(env.volume(), 15);
        env.tick(); // divider fires: decay 14
        assert_eq!(env.volume(), 14);
    }

    #[test]
    fn decay_stops_at_zero_without_loop() {
        let mut env = Envelope::new();
        env.write(0x00); // period 0 -> decay steps every clock
        env.restart();
        env.tick(); // decay = 15
        for _ in 0..15 {
            env.tick();
        }
        assert_eq!(env.volume(), 0);
        env.tick();
        assert_eq!(env.volume(), 0);
    }

    #[test]
    fn decay_wraps_to_15_with_loop() {
        let mut env = Envelope::new();
        env.write(0x20); // loop, period 0
        env.restart();
        env.tick(); // decay = 15
        for _ in 0..15 {
            env.tick();
        }
        assert_eq!(env.volume(), 0);
        env.tick();
        assert_eq!(env.volume(), 15);
    }

    #[test]
    fn constant_volume_returns_period() {
        let mut env = Envelope::new();
        env.write(0x1a); // constant, V = 10
        assert_eq!(env.volume(), 10);
    }
}
