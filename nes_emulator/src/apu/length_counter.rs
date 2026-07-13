// https://www.nesdev.org/wiki/APU_Length_Counter
//
// Shared by pulse 1/2, triangle and noise. Counts down once per half-frame
// clock (unless halted) and silences its channel when it reaches zero.

// Lookup table for the 5-bit load value written to bits 7-3 of
// $4003/$4007/$400B/$400F.
const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, //
    12, 16, 24, 18, 48, 20, 96, 22, 192, 24, 72, 26, 16, 28, 32, 30,
];

#[derive(Clone)]
pub struct LengthCounter {
    counter: u8,
    // For pulse/noise this is the envelope loop flag; for triangle the
    // linear counter control flag.
    pub halt: bool,
    enabled: bool,
}

impl LengthCounter {
    pub fn new() -> Self {
        LengthCounter {
            counter: 0,
            halt: false,
            enabled: false,
        }
    }

    // $4015 enable bit: disabling forces the counter to 0 and it cannot be
    // reloaded until the channel is enabled again.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.counter = 0;
        }
    }

    // Reload from the 5-bit table index; ignored while the channel is
    // disabled.
    pub fn load(&mut self, index: u8) {
        if self.enabled {
            self.counter = LENGTH_TABLE[(index & 0x1f) as usize];
        }
    }

    // Half-frame clock.
    pub fn tick(&mut self) {
        if !self.halt && self.counter > 0 {
            self.counter -= 1;
        }
    }

    pub fn active(&self) -> bool {
        self.counter > 0
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn load_uses_lookup_table() {
        let mut lc = LengthCounter::new();
        lc.set_enabled(true);
        lc.load(0);
        assert!(lc.active());
        lc.load(1);
        // index 1 -> 254
        for _ in 0..254 {
            assert!(lc.active());
            lc.tick();
        }
        assert!(!lc.active());
    }

    #[test]
    fn counter_stops_at_zero() {
        let mut lc = LengthCounter::new();
        lc.set_enabled(true);
        lc.load(3); // -> 2
        lc.tick();
        lc.tick();
        lc.tick();
        assert!(!lc.active());
    }

    #[test]
    fn halt_freezes_counter() {
        let mut lc = LengthCounter::new();
        lc.set_enabled(true);
        lc.load(3); // -> 2
        lc.halt = true;
        lc.tick();
        lc.tick();
        assert!(lc.active());
    }

    #[test]
    fn disabling_zeroes_and_blocks_reload() {
        let mut lc = LengthCounter::new();
        lc.set_enabled(true);
        lc.load(1);
        lc.set_enabled(false);
        assert!(!lc.active());
        lc.load(1); // ignored while disabled
        assert!(!lc.active());
    }
}
