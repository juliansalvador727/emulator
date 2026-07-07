// https://www.nesdev.org/wiki/APU_DMC
//
// Delta modulation channel: plays 1-bit delta-encoded samples fetched from
// CPU memory ($8000-$FFFF), moving a 7-bit output level up or down by 2 per
// bit. The memory reader can't touch the bus from in here, so it is split
// into a handshake: when the sample buffer is empty and bytes remain,
// dma_request() returns the address to fetch; the Bus performs the read
// (stalling the CPU like the real DMA unit does) and hands the byte back
// via dma_load(). See Bus::tick.

// NTSC rate table: CPU cycles between output unit clocks, indexed by the
// low nibble of $4010.
const RATE_TABLE: [u16; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];

pub struct Dmc {
    irq_enabled: bool,
    loop_flag: bool,
    timer_period: u16,
    timer: u16,

    output_level: u8, // 7-bit DAC value

    // Memory reader
    sample_address: u16, // $C000 + $4012 * 64
    sample_length: u16,  // $4013 * 16 + 1
    current_address: u16,
    pub bytes_remaining: u16,
    sample_buffer: Option<u8>,

    // Output unit
    shift: u8,
    bits_remaining: u8,
    silence: bool,

    pub irq_flag: bool,
}

impl Dmc {
    pub fn new() -> Self {
        Dmc {
            irq_enabled: false,
            loop_flag: false,
            timer_period: RATE_TABLE[0],
            timer: 0,
            output_level: 0,
            sample_address: 0xc000,
            sample_length: 1,
            current_address: 0xc000,
            bytes_remaining: 0,
            sample_buffer: None,
            shift: 0,
            bits_remaining: 8,
            silence: true,
            irq_flag: false,
        }
    }

    // $4010: IL-- RRRR. Clearing the IRQ enable bit also clears the
    // interrupt flag.
    pub fn write_control(&mut self, data: u8) {
        self.irq_enabled = data & 0x80 != 0;
        if !self.irq_enabled {
            self.irq_flag = false;
        }
        self.loop_flag = data & 0x40 != 0;
        self.timer_period = RATE_TABLE[(data & 0x0f) as usize];
    }

    // $4011: -DDD DDDD direct load of the output level.
    pub fn write_direct_load(&mut self, data: u8) {
        self.output_level = data & 0x7f;
    }

    // $4012: sample start address = $C000 + A*64
    pub fn write_sample_address(&mut self, data: u8) {
        self.sample_address = 0xc000 | ((data as u16) << 6);
    }

    // $4013: sample length = L*16 + 1 bytes
    pub fn write_sample_length(&mut self, data: u8) {
        self.sample_length = ((data as u16) << 4) | 1;
    }

    // $4015 bit 4. Setting it restarts the sample only if it has finished;
    // clearing it stops the fetches but lets the buffered bits play out.
    // Any write to $4015 clears the DMC interrupt flag.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.irq_flag = false;
        if !enabled {
            self.bytes_remaining = 0;
        } else if self.bytes_remaining == 0 {
            self.restart_sample();
        }
    }

    fn restart_sample(&mut self) {
        self.current_address = self.sample_address;
        self.bytes_remaining = self.sample_length;
    }

    // The memory reader wants a byte from this address. On hardware the
    // fetch happens the moment the sample buffer empties; we service it
    // once per Bus::tick (i.e. once per CPU instruction), which is at most
    // a few cycles late.
    pub fn dma_request(&self) -> Option<u16> {
        if self.sample_buffer.is_none() && self.bytes_remaining > 0 {
            Some(self.current_address)
        } else {
            None
        }
    }

    // Completion of the DMA fetch: fill the buffer, advance the reader, and
    // handle end-of-sample looping / IRQ.
    pub fn dma_load(&mut self, value: u8) {
        self.sample_buffer = Some(value);
        // The address wraps from $FFFF back to $8000.
        self.current_address = if self.current_address == 0xffff {
            0x8000
        } else {
            self.current_address + 1
        };
        self.bytes_remaining -= 1;
        if self.bytes_remaining == 0 {
            if self.loop_flag {
                self.restart_sample();
            } else if self.irq_enabled {
                self.irq_flag = true;
            }
        }
    }

    // Clocked every CPU cycle; the rate table is in CPU cycles.
    pub fn tick_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.timer_period - 1;
            self.clock_output_unit();
        } else {
            self.timer -= 1;
        }
    }

    // One output cycle step: apply the delta for the current bit, then
    // refill the shift register from the sample buffer every 8 bits.
    fn clock_output_unit(&mut self) {
        if !self.silence {
            if self.shift & 1 == 1 {
                if self.output_level <= 125 {
                    self.output_level += 2;
                }
            } else if self.output_level >= 2 {
                self.output_level -= 2;
            }
        }
        self.shift >>= 1;
        self.bits_remaining -= 1;
        if self.bits_remaining == 0 {
            self.bits_remaining = 8;
            match self.sample_buffer.take() {
                Some(byte) => {
                    self.shift = byte;
                    self.silence = false;
                }
                None => self.silence = true,
            }
        }
    }

    pub fn output(&self) -> u8 {
        self.output_level
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn enable_restarts_finished_sample() {
        let mut d = Dmc::new();
        d.write_sample_address(0x02); // $C080
        d.write_sample_length(0x01); // 17 bytes
        assert_eq!(d.dma_request(), None);
        d.set_enabled(true);
        assert_eq!(d.bytes_remaining, 17);
        assert_eq!(d.dma_request(), Some(0xc080));
    }

    #[test]
    fn enable_does_not_restart_running_sample() {
        let mut d = Dmc::new();
        d.write_sample_length(0x01);
        d.set_enabled(true);
        d.dma_load(0xff);
        assert_eq!(d.bytes_remaining, 16);
        d.set_enabled(true);
        assert_eq!(d.bytes_remaining, 16); // unchanged
    }

    #[test]
    fn disable_stops_fetches() {
        let mut d = Dmc::new();
        d.write_sample_length(0x01);
        d.set_enabled(true);
        d.set_enabled(false);
        assert_eq!(d.bytes_remaining, 0);
        assert_eq!(d.dma_request(), None);
    }

    #[test]
    fn address_wraps_to_8000() {
        let mut d = Dmc::new();
        d.write_sample_address(0xff); // $FFC0
        d.write_sample_length(0x04); // 65 bytes
        d.set_enabled(true);
        for _ in 0..64 {
            assert!(d.dma_request().is_some());
            d.dma_load(0x00);
            d.sample_buffer = None; // consume it, as the output unit would
        }
        assert_eq!(d.dma_request(), Some(0x8000));
    }

    #[test]
    fn irq_set_when_sample_ends_and_irq_enabled() {
        let mut d = Dmc::new();
        d.write_control(0x80); // IRQ enabled, no loop
        d.write_sample_length(0x00); // 1 byte
        d.set_enabled(true);
        d.dma_load(0x00);
        assert!(d.irq_flag);
        assert_eq!(d.bytes_remaining, 0);
        // A $4015 write clears the flag.
        d.set_enabled(false);
        assert!(!d.irq_flag);
    }

    #[test]
    fn loop_restarts_instead_of_irq() {
        let mut d = Dmc::new();
        d.write_control(0xc0); // IRQ enabled + loop
        d.write_sample_address(0x00); // $C000
        d.write_sample_length(0x00); // 1 byte
        d.set_enabled(true);
        d.dma_load(0x00);
        assert!(!d.irq_flag);
        assert_eq!(d.bytes_remaining, 1);
        d.sample_buffer = None;
        assert_eq!(d.dma_request(), Some(0xc000));
    }

    #[test]
    fn clearing_irq_enable_clears_flag() {
        let mut d = Dmc::new();
        d.write_control(0x80);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0x00);
        assert!(d.irq_flag);
        d.write_control(0x00);
        assert!(!d.irq_flag);
    }

    #[test]
    fn output_deltas_track_sample_bits() {
        let mut d = Dmc::new();
        d.write_direct_load(64);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0b0000_1111); // four +2 steps then four -2 steps
        // The output unit powers up mid-cycle with 8 silent bits; the 8th
        // clock loads the sample byte into the shift register.
        for _ in 0..8 {
            d.clock_output_unit();
        }
        assert_eq!(d.output(), 64);
        let mut levels = vec![];
        for _ in 0..8 {
            d.clock_output_unit();
            levels.push(d.output());
        }
        assert_eq!(levels, vec![66, 68, 70, 72, 70, 68, 66, 64]);
    }

    #[test]
    fn timer_period_spaces_output_clocks() {
        let mut d = Dmc::new();
        d.write_control(0x0f); // fastest rate: 54 CPU cycles per clock
        d.write_direct_load(64);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0xff); // all +2
        // The timer starts at 0, so clock k lands on cycle 54*(k-1) + 1.
        // Burn the 8 silent power-up bits (clock 8 loads the sample byte).
        for _ in 0..8 * 54 {
            d.tick_timer();
        }
        assert_eq!(d.output(), 64);
        d.tick_timer(); // clock 9: first sample bit
        assert_eq!(d.output(), 66);
        // The next delta arrives exactly 54 cycles later.
        for _ in 0..53 {
            d.tick_timer();
        }
        assert_eq!(d.output(), 66);
        d.tick_timer();
        assert_eq!(d.output(), 68);
    }

    #[test]
    fn output_clamps_at_bounds() {
        let mut d = Dmc::new();
        d.write_direct_load(126);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0xff); // all +2
        for _ in 0..9 {
            d.clock_output_unit();
        }
        assert_eq!(d.output(), 126); // 126 -> stuck (adding would exceed 127)

        let mut d = Dmc::new();
        d.write_direct_load(1);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0x00); // all -2
        for _ in 0..9 {
            d.clock_output_unit();
        }
        assert_eq!(d.output(), 1); // 1 -> stuck (subtracting would go below 0)
    }

    #[test]
    fn silence_resumes_when_buffer_runs_dry() {
        let mut d = Dmc::new();
        d.write_direct_load(64);
        d.write_sample_length(0x00);
        d.set_enabled(true);
        d.dma_load(0xff);
        // 8 clocks to burn the initial silent byte's bits? No: buffer is
        // consumed at the first 8-bit boundary, then 8 more clocks play it.
        for _ in 0..16 {
            d.clock_output_unit();
        }
        let level = d.output();
        // Buffer is now empty -> silence flag set; output level holds.
        for _ in 0..8 {
            d.clock_output_unit();
        }
        assert_eq!(d.output(), level);
    }
}
