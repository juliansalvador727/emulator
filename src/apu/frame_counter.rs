// https://www.nesdev.org/wiki/APU_Frame_Counter
//
// The frame counter divides the CPU clock down to ~240 Hz "quarter frame"
// and ~120 Hz "half frame" clocks that drive the envelopes, the triangle's
// linear counter, the length counters and the sweep units.
//
// The wiki documents the sequence in APU cycles (1 APU cycle = 2 CPU cycles)
// with half-cycle resolution; the constants below are the wiki values
// doubled so we can count whole CPU cycles.

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum FrameEvent {
    None,
    Quarter,
    // A half-frame clock. Every half-frame clock coincides with a quarter
    // frame clock, so `Half` means "clock both quarter and half units".
    Half,
}

// Mode 0 (4-step) sequence, in CPU cycles:       wiki (APU cycles):
const STEP1: u32 = 7457; // quarter                3728.5
const STEP2: u32 = 14913; // quarter + half        7456.5
const STEP3: u32 = 22371; // quarter              11185.5
const IRQ_SET: u32 = 29828; // frame IRQ          14914
const STEP4: u32 = 29829; // quarter + half + IRQ 14914.5
const FOUR_STEP_LENGTH: u32 = 29830; // IRQ, wrap 14915
// Mode 1 (5-step): steps 1-3 as above, nothing at 29829, then:
const STEP5: u32 = 37281; // quarter + half       18640.5
const FIVE_STEP_LENGTH: u32 = 37282; //           18641

#[derive(Clone)]
pub struct FrameCounter {
    cycle: u32, // CPU cycles into the current sequence
    five_step: bool,
    irq_inhibit: bool,
    pub frame_irq: bool,

    // A $4017 write only takes effect 3-4 CPU cycles later; see write().
    reset_delay: u8,
    pending_five_step: bool,
}

impl FrameCounter {
    pub fn new() -> Self {
        FrameCounter {
            cycle: 0,
            five_step: false,
            irq_inhibit: false,
            frame_irq: false,
            reset_delay: 0,
            pending_five_step: false,
        }
    }

    // $4017 write (MI-- ----). The sequencer restarts "3 or 4 CPU clock
    // cycles after the write cycle" depending on whether the write landed on
    // an APU cycle. We take the parity of the APU's global cycle counter;
    // the CPU ticks the bus in whole-instruction batches, so which parity a
    // write lands on is approximate - the same order of jitter real
    // hardware exhibits.
    pub fn write(&mut self, data: u8, on_apu_cycle: bool) {
        self.pending_five_step = data & 0x80 != 0;
        self.irq_inhibit = data & 0x40 != 0;
        // Setting the inhibit flag clears any pending frame interrupt.
        if self.irq_inhibit {
            self.frame_irq = false;
        }
        self.reset_delay = if on_apu_cycle { 3 } else { 4 };
    }

    // Advance one CPU cycle.
    pub fn tick(&mut self) -> FrameEvent {
        // Delayed $4017 write: restart the sequence. The wiki says a write
        // with bit 7 set "immediately" clocks the quarter+half units;
        // blargg's frame counter tests show that clock coincides with the
        // delayed sequencer reset, so we generate it here.
        if self.reset_delay > 0 {
            self.reset_delay -= 1;
            if self.reset_delay == 0 {
                self.cycle = 0;
                self.five_step = self.pending_five_step;
                if self.five_step {
                    return FrameEvent::Half;
                }
                return FrameEvent::None;
            }
        }

        self.cycle += 1;
        if self.five_step {
            match self.cycle {
                STEP1 | STEP3 => FrameEvent::Quarter,
                STEP2 | STEP5 => FrameEvent::Half,
                FIVE_STEP_LENGTH => {
                    self.cycle = 0;
                    FrameEvent::None
                }
                _ => FrameEvent::None,
            }
        } else {
            match self.cycle {
                STEP1 | STEP3 => FrameEvent::Quarter,
                STEP2 => FrameEvent::Half,
                // The frame interrupt flag is held asserted for three
                // consecutive CPU cycles around the end of the 4-step
                // sequence (wiki APU cycles 14914, 14914.5 and 14915).
                IRQ_SET => {
                    self.set_irq();
                    FrameEvent::None
                }
                STEP4 => {
                    self.set_irq();
                    FrameEvent::Half
                }
                FOUR_STEP_LENGTH => {
                    self.set_irq();
                    self.cycle = 0;
                    FrameEvent::None
                }
                _ => FrameEvent::None,
            }
        }
    }

    fn set_irq(&mut self) {
        if !self.irq_inhibit {
            self.frame_irq = true;
        }
    }

    // Side effect of reading $4015.
    pub fn clear_frame_irq(&mut self) {
        self.frame_irq = false;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // Tick `n` cycles, returning (cycle offset, event) for each non-None event.
    fn run(fc: &mut FrameCounter, n: u32) -> Vec<(u32, FrameEvent)> {
        let mut events = vec![];
        for i in 1..=n {
            let e = fc.tick();
            if e != FrameEvent::None {
                events.push((i, e));
            }
        }
        events
    }

    #[test]
    fn four_step_event_timing() {
        let mut fc = FrameCounter::new();
        let events = run(&mut fc, FOUR_STEP_LENGTH);
        assert_eq!(
            events,
            vec![
                (STEP1, FrameEvent::Quarter),
                (STEP2, FrameEvent::Half),
                (STEP3, FrameEvent::Quarter),
                (STEP4, FrameEvent::Half),
            ]
        );
    }

    #[test]
    fn four_step_wraps_around() {
        let mut fc = FrameCounter::new();
        run(&mut fc, FOUR_STEP_LENGTH);
        // Second pass produces the same sequence.
        let events = run(&mut fc, FOUR_STEP_LENGTH);
        assert_eq!(events[0], (STEP1, FrameEvent::Quarter));
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn four_step_sets_frame_irq_at_sequence_end() {
        let mut fc = FrameCounter::new();
        run(&mut fc, IRQ_SET - 1);
        assert!(!fc.frame_irq);
        fc.tick();
        assert!(fc.frame_irq);
    }

    #[test]
    fn irq_flag_persists_until_cleared() {
        let mut fc = FrameCounter::new();
        run(&mut fc, FOUR_STEP_LENGTH + 100);
        assert!(fc.frame_irq);
        fc.clear_frame_irq();
        assert!(!fc.frame_irq);
    }

    #[test]
    fn irq_inhibit_blocks_and_clears_irq() {
        let mut fc = FrameCounter::new();
        run(&mut fc, FOUR_STEP_LENGTH);
        assert!(fc.frame_irq);
        fc.write(0x40, true); // inhibit clears the pending flag...
        assert!(!fc.frame_irq);
        run(&mut fc, 2 * FOUR_STEP_LENGTH); // ...and blocks future ones
        assert!(!fc.frame_irq);
    }

    #[test]
    fn five_step_write_clocks_quarter_and_half_after_delay() {
        let mut fc = FrameCounter::new();
        fc.write(0x80, true); // 3-cycle delay
        assert_eq!(fc.tick(), FrameEvent::None);
        assert_eq!(fc.tick(), FrameEvent::None);
        assert_eq!(fc.tick(), FrameEvent::Half);
    }

    #[test]
    fn four_step_write_does_not_clock_units() {
        let mut fc = FrameCounter::new();
        fc.write(0x00, false); // 4-cycle delay
        let events = run(&mut fc, 4);
        assert!(events.is_empty());
    }

    #[test]
    fn five_step_event_timing_and_no_irq() {
        let mut fc = FrameCounter::new();
        fc.write(0x80, true);
        run(&mut fc, 3); // apply the reset (clocks quarter+half once)
        let events = run(&mut fc, FIVE_STEP_LENGTH);
        assert_eq!(
            events,
            vec![
                (STEP1, FrameEvent::Quarter),
                (STEP2, FrameEvent::Half),
                (STEP3, FrameEvent::Quarter),
                (STEP5, FrameEvent::Half),
            ]
        );
        assert!(!fc.frame_irq);
    }

    #[test]
    fn write_resets_sequence_position() {
        let mut fc = FrameCounter::new();
        run(&mut fc, 5000);
        fc.write(0x00, true);
        run(&mut fc, 3); // apply the reset
        // The next quarter clock arrives a full STEP1 later, not at 7457-5000.
        let events = run(&mut fc, STEP1);
        assert_eq!(events, vec![(STEP1, FrameEvent::Quarter)]);
    }
}
