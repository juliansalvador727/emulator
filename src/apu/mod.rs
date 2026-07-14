// NES APU (the audio half of the 2A03) - https://www.nesdev.org/wiki/APU
//
// The Bus ticks the APU once per CPU cycle alongside the PPU. Every
// CPU_HZ / sample_rate cycles the five channel outputs are combined by the
// non-linear mixer into one f32 sample and buffered; the game loop drains
// the buffer into the host audio device once per frame.

use crate::apu::dmc::Dmc;
use crate::apu::filter::{HighPassFilter, LowPassFilter};
use crate::apu::frame_counter::{FrameCounter, FrameEvent};
use crate::apu::noise::Noise;
use crate::apu::pulse::Pulse;
use crate::apu::triangle::Triangle;

pub mod dmc;
pub mod envelope;
pub mod filter;
pub mod frame_counter;
pub mod length_counter;
pub mod noise;
pub mod pulse;
pub mod triangle;

// NTSC 2A03 CPU clock rate, from
// https://www.nesdev.org/wiki/Cycle_reference_chart
const CPU_HZ: f64 = 1_789_773.0;

const DEFAULT_SAMPLE_RATE: u32 = 48000;

#[derive(Clone)]
pub struct NesAPU {
    pub pulse1: Pulse,
    pub pulse2: Pulse,
    pub triangle: Triangle,
    pub noise: Noise,
    pub dmc: Dmc,
    frame_counter: FrameCounter,

    // Total CPU cycles ticked. The pulse timers and the $4017 write delay
    // depend on APU-cycle (CPU/2) parity.
    cycles: usize,

    // Downsampling: emit one mixed sample every cycles_per_sample CPU cycles,
    // tracking the fractional remainder so the long-run rate is exact.
    cycles_per_sample: f64,
    sample_timer: f64,
    samples: Vec<f32>,
    // If nothing drains the buffer (e.g. the nestest path), stop pushing
    // after ~1 second rather than growing without bound.
    max_buffered_samples: usize,

    hp90: HighPassFilter,
    hp440: HighPassFilter,
    lp14k: LowPassFilter,

    // Debug aid (used by the `probe` subcommand): log every register write
    // to stderr as `APUW addr=value`.
    pub trace_writes: bool,
}

impl NesAPU {
    pub fn new() -> Self {
        let mut apu = NesAPU {
            pulse1: Pulse::new(true),
            pulse2: Pulse::new(false),
            triangle: Triangle::new(),
            noise: Noise::new(),
            dmc: Dmc::new(),
            frame_counter: FrameCounter::new(),
            cycles: 0,
            cycles_per_sample: 0.0,
            sample_timer: 0.0,
            samples: Vec::new(),
            max_buffered_samples: 0,
            hp90: HighPassFilter::new(90.0, DEFAULT_SAMPLE_RATE as f32),
            hp440: HighPassFilter::new(440.0, DEFAULT_SAMPLE_RATE as f32),
            lp14k: LowPassFilter::new(14000.0, DEFAULT_SAMPLE_RATE as f32),
            trace_writes: false,
        };
        apu.set_sample_rate(DEFAULT_SAMPLE_RATE);
        apu
    }

    // Call before running if the host audio device didn't open at 48 kHz.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.cycles_per_sample = CPU_HZ / sample_rate as f64;
        self.max_buffered_samples = sample_rate as usize;
        self.hp90 = HighPassFilter::new(90.0, sample_rate as f32);
        self.hp440 = HighPassFilter::new(440.0, sample_rate as f32);
        self.lp14k = LowPassFilter::new(14000.0, sample_rate as f32);
    }

    pub fn write_register(&mut self, addr: u16, data: u8) {
        if self.trace_writes {
            eprintln!("APUW {:04x}={:02x}", addr, data);
        }
        match addr {
            0x4000 => self.pulse1.write_control(data),
            0x4001 => self.pulse1.write_sweep(data),
            0x4002 => self.pulse1.write_timer_lo(data),
            0x4003 => self.pulse1.write_timer_hi(data),
            0x4004 => self.pulse2.write_control(data),
            0x4005 => self.pulse2.write_sweep(data),
            0x4006 => self.pulse2.write_timer_lo(data),
            0x4007 => self.pulse2.write_timer_hi(data),
            0x4008 => self.triangle.write_linear(data),
            0x400a => self.triangle.write_timer_lo(data),
            0x400b => self.triangle.write_timer_hi(data),
            0x400c => self.noise.write_control(data),
            0x400e => self.noise.write_mode_period(data),
            0x400f => self.noise.write_length(data),
            0x4010 => self.dmc.write_control(data),
            0x4011 => self.dmc.write_direct_load(data),
            0x4012 => self.dmc.write_sample_address(data),
            0x4013 => self.dmc.write_sample_length(data),
            0x4015 => self.write_status(data),
            0x4017 => self.frame_counter.write(data, self.cycles % 2 == 0),
            0x4009 | 0x400d => { /* unused registers */ }
            _ => panic!("invalid APU register write: {:04x}", addr),
        }
    }

    // $4015 write (---D NT21): channel enables. Clearing a bit zeroes that
    // channel's length counter; the DMC bit restarts or stops the sample
    // fetches and any write clears the DMC interrupt flag.
    fn write_status(&mut self, data: u8) {
        self.pulse1.length.set_enabled(data & 0x01 != 0);
        self.pulse2.length.set_enabled(data & 0x02 != 0);
        self.triangle.length.set_enabled(data & 0x04 != 0);
        self.noise.length.set_enabled(data & 0x08 != 0);
        self.dmc.set_enabled(data & 0x10 != 0);
    }

    // $4015 read (IF-D NT21; bit 5 is open bus, returned as 0). Reading
    // clears the frame interrupt flag but not the DMC interrupt flag.
    pub fn read_status(&mut self) -> u8 {
        let mut result = 0u8;
        if self.pulse1.length.active() {
            result |= 0x01;
        }
        if self.pulse2.length.active() {
            result |= 0x02;
        }
        if self.triangle.length.active() {
            result |= 0x04;
        }
        if self.noise.length.active() {
            result |= 0x08;
        }
        if self.dmc.bytes_remaining > 0 {
            result |= 0x10;
        }
        if self.frame_counter.frame_irq {
            result |= 0x40;
        }
        if self.dmc.irq_flag {
            result |= 0x80;
        }
        self.frame_counter.clear_frame_irq();
        result
    }

    // Level-triggered IRQ line to the CPU: asserted while either interrupt
    // flag is set. The handler acknowledges by reading $4015 (frame) or
    // writing $4015/$4010 (DMC).
    pub fn irq_pending(&self) -> bool {
        self.frame_counter.frame_irq || self.dmc.irq_flag
    }

    // DMC memory reader handshake; see Bus::tick.
    pub fn dmc_dma_request(&self) -> Option<u16> {
        self.dmc.dma_request()
    }

    pub fn dmc_dma_load(&mut self, value: u8) {
        self.dmc.dma_load(value);
    }

    #[cfg(test)]
    pub(crate) fn cycle_count(&self) -> usize {
        self.cycles
    }

    pub fn tick(&mut self, cycles: u8) {
        for _ in 0..cycles {
            self.tick_cycle();
        }
    }

    fn tick_cycle(&mut self) {
        self.cycles += 1;

        match self.frame_counter.tick() {
            FrameEvent::Quarter => self.clock_quarter_frame(),
            FrameEvent::Half => {
                self.clock_quarter_frame();
                self.clock_half_frame();
            }
            FrameEvent::None => {}
        }

        // Triangle, noise and DMC timers run at the CPU clock; the pulse
        // timers count APU cycles (every 2nd CPU cycle).
        self.triangle.tick_timer();
        self.noise.tick_timer();
        self.dmc.tick_timer();
        if self.cycles % 2 == 0 {
            self.pulse1.tick_timer();
            self.pulse2.tick_timer();
        }

        self.sample_timer += 1.0;
        if self.sample_timer >= self.cycles_per_sample {
            self.sample_timer -= self.cycles_per_sample;
            let sample = self.mix();
            if self.samples.len() < self.max_buffered_samples {
                self.samples.push(sample);
            }
        }
    }

    // Envelopes and the triangle's linear counter.
    fn clock_quarter_frame(&mut self) {
        self.pulse1.envelope.tick();
        self.pulse2.envelope.tick();
        self.noise.envelope.tick();
        self.triangle.tick_linear();
    }

    // Length counters and sweep units.
    fn clock_half_frame(&mut self) {
        self.pulse1.length.tick();
        self.pulse2.length.tick();
        self.triangle.length.tick();
        self.noise.length.tick();
        self.pulse1.tick_sweep();
        self.pulse2.tick_sweep();
    }

    // Non-linear mixer from https://www.nesdev.org/wiki/APU_Mixer
    // ("exact" formulas), followed by the output filters.
    fn mix(&mut self) -> f32 {
        let pulse = (self.pulse1.output() + self.pulse2.output()) as f64;
        let pulse_out = if pulse == 0.0 {
            0.0
        } else {
            95.88 / (8128.0 / pulse + 100.0)
        };

        let tnd = self.triangle.output() as f64 / 8227.0
            + self.noise.output() as f64 / 12241.0
            + self.dmc.output() as f64 / 22638.0;
        let tnd_out = if tnd == 0.0 {
            0.0
        } else {
            159.79 / (1.0 / tnd + 100.0)
        };

        let mixed = (pulse_out + tnd_out) as f32;
        self.lp14k
            .process(self.hp440.process(self.hp90.process(mixed)))
    }

    // Hand the buffered samples to the caller (the per-frame game loop
    // callback queues them on the SDL audio device).
    pub fn drain_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }

    /// Number of mixed host samples waiting for delivery. Frontends use this
    /// to forward small chunks during a video frame instead of making audio
    /// wait for the next vblank callback.
    pub fn buffered_samples(&self) -> usize {
        self.samples.len()
    }

    /// Remove the oldest `count` samples while retaining any newer remainder.
    /// Callers must not request more samples than `buffered_samples()` reports.
    pub fn drain_sample_chunk(&mut self, count: usize) -> Vec<f32> {
        assert!(count <= self.samples.len());
        self.samples.drain(..count).collect()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn status_reflects_length_counters() {
        let mut apu = NesAPU::new();
        assert_eq!(apu.read_status() & 0x1f, 0);
        apu.write_register(0x4015, 0x0f); // enable pulse1/2, triangle, noise
        apu.write_register(0x4003, 0x08); // load pulse1 length
        apu.write_register(0x4007, 0x08); // load pulse2 length
        apu.write_register(0x400b, 0x08); // load triangle length
        apu.write_register(0x400f, 0x08); // load noise length
        assert_eq!(apu.read_status() & 0x1f, 0x0f);
    }

    #[test]
    fn disabling_channel_clears_length_counter() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4015, 0x01);
        apu.write_register(0x4003, 0x08);
        assert_eq!(apu.read_status() & 0x01, 0x01);
        apu.write_register(0x4015, 0x00);
        assert_eq!(apu.read_status() & 0x01, 0);
    }

    #[test]
    fn writes_to_disabled_channel_length_are_ignored() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4003, 0x08); // pulse1 not enabled
        assert_eq!(apu.read_status() & 0x01, 0);
    }

    #[test]
    fn frame_irq_raised_and_cleared_by_status_read() {
        let mut apu = NesAPU::new();
        // 4-step mode is the power-up default; run one full sequence.
        for _ in 0..29830 {
            apu.tick(1);
        }
        assert!(apu.irq_pending());
        let status = apu.read_status();
        assert_eq!(status & 0x40, 0x40);
        assert!(!apu.irq_pending());
        assert_eq!(apu.read_status() & 0x40, 0);
    }

    #[test]
    fn frame_irq_inhibited_by_4017_bit6() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4017, 0x40);
        for _ in 0..30000 {
            apu.tick(1);
        }
        assert!(!apu.irq_pending());
    }

    #[test]
    fn five_step_mode_produces_no_irq() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4017, 0x80);
        for _ in 0..40000 {
            apu.tick(1);
        }
        assert!(!apu.irq_pending());
    }

    #[test]
    fn half_frame_clocks_length_counters() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4015, 0x01);
        apu.write_register(0x4003, 0x18); // length index 3 -> 2
        // The half-frame clocks at CPU cycles 14913 and 29829 empty it.
        for _ in 0..15000 {
            apu.tick(1);
        }
        assert_eq!(apu.read_status() & 0x01, 0x01);
        for _ in 0..15000 {
            apu.tick(1);
        }
        assert_eq!(apu.read_status() & 0x01, 0);
    }

    #[test]
    fn dmc_enable_reflected_in_status() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4013, 0x01); // 17 bytes
        apu.write_register(0x4015, 0x10);
        assert_eq!(apu.read_status() & 0x10, 0x10);
        apu.write_register(0x4015, 0x00);
        assert_eq!(apu.read_status() & 0x10, 0);
    }

    #[test]
    fn sampling_produces_roughly_sample_rate_per_second() {
        let mut apu = NesAPU::new();
        // One NTSC frame is 29780.5 CPU cycles; tick 60 frames' worth.
        for _ in 0..1_786_830usize / 255 {
            apu.tick(255);
        }
        let n = apu.drain_samples().len();
        // ~1 second of emulated time -> ~48000 samples (+-1%).
        assert!((47000..49000).contains(&n), "got {} samples", n);
    }

    #[test]
    fn enabled_pulse_produces_audible_samples() {
        let mut apu = NesAPU::new();
        apu.write_register(0x4015, 0x01); // enable pulse 1
        apu.write_register(0x4000, 0xbf); // duty 2, halt, constant volume 15
        apu.write_register(0x4002, 0xfd); // timer low
        apu.write_register(0x4003, 0x09); // timer hi 1 -> period 0x1fd (~219 Hz)
        for _ in 0..29830 {
            apu.tick(1);
        }
        let samples = apu.drain_samples();
        assert!(!samples.is_empty());
        assert!(samples.iter().any(|s| s.abs() > 0.01));
    }

    #[test]
    fn dc_output_decays_to_silence_through_filters() {
        // At power-up the triangle holds sequence value 15: a pure DC
        // level, since its sequencer is frozen by the zeroed linear
        // counter. The high-pass filters remove DC, so after the initial
        // transient the output settles back to silence.
        let mut apu = NesAPU::new();
        for _ in 0..894_886usize / 255 {
            apu.tick(255); // ~0.5 s of emulated time
        }
        let samples = apu.drain_samples();
        let last = *samples.last().unwrap();
        assert!(last.abs() < 0.001, "expected silence, got {}", last);
    }
}
