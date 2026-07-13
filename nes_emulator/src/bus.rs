use std::rc::Rc;

use crate::apu::NesAPU;
use crate::cartridge::Rom;
use crate::cpu::Mem;
use crate::joypad::Joypad;
use crate::mapper::{self, SharedMapper};
use crate::ppu::NesPPU;

const RAM: u16 = 0x0000;
const RAM_MIRRORS_END: u16 = 0x1FFF;
// const PPU_REGISTERS: u16 = 0x2000; unused
const PPU_REGISTERS_MIRRORS_END: u16 = 0x3FFF;

pub struct Bus<'call> {
    cpu_vram: [u8; 2048],
    mapper: SharedMapper,
    ppu: NesPPU,
    pub apu: NesAPU,

    cycles: usize,
    oam_dma_pending: bool,
    gameloop_callback: Box<dyn FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call>,
    audio_chunk_samples: Option<usize>,
    audio_callback: Box<dyn FnMut(Vec<f32>) + 'call>,
    audio_delivery_enabled: bool,
    host_frame_ready: bool,
    joypad1: Joypad,
}

pub struct BusSnapshot {
    cpu_vram: [u8; 2048],
    mapper: SharedMapper,
    ppu: NesPPU,
    apu: NesAPU,
    cycles: usize,
    oam_dma_pending: bool,
    audio_delivery_enabled: bool,
    host_frame_ready: bool,
    joypad1: Joypad,
}

impl<'a> Bus<'a> {
    pub fn new<'call, F>(rom: Rom, gameloop_callback: F) -> Bus<'call>
    where
        F: FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call,
    {
        Self::new_with_audio(rom, gameloop_callback, usize::MAX, |_| {})
    }

    /// Construct a bus that forwards completed audio chunks independently of
    /// video frames. `audio_chunk_samples` is a host-delivery quantum, not an
    /// emulation timing parameter; the APU still runs at the CPU clock.
    pub fn new_with_audio<'call, F, A>(
        rom: Rom,
        gameloop_callback: F,
        audio_chunk_samples: usize,
        audio_callback: A,
    ) -> Bus<'call>
    where
        F: FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call,
        A: FnMut(Vec<f32>) + 'call,
    {
        assert!(audio_chunk_samples > 0);
        let mapper = mapper::from_rom(rom);
        let ppu = NesPPU::new(Rc::clone(&mapper));
        Bus {
            cpu_vram: [0; 2048],
            mapper: mapper,
            ppu: ppu,
            apu: NesAPU::new(),
            cycles: 0,
            oam_dma_pending: false,
            gameloop_callback: Box::from(gameloop_callback),
            audio_chunk_samples: (audio_chunk_samples != usize::MAX)
                .then_some(audio_chunk_samples),
            audio_callback: Box::from(audio_callback),
            audio_delivery_enabled: true,
            host_frame_ready: false,
            joypad1: Joypad::new(),
        }
    }

    #[inline]
    fn deliver_audio_chunks(&mut self) {
        let Some(chunk_samples) = self.audio_chunk_samples else {
            return;
        };
        if !self.audio_delivery_enabled {
            return;
        }
        while self.apu.buffered_samples() >= chunk_samples {
            let samples = self.apu.drain_sample_chunk(chunk_samples);
            (self.audio_callback)(samples);
        }
    }

    pub fn tick(&mut self, cycles: u8) {
        self.cycles += cycles as usize;
        self.apu.tick(cycles);
        self.ppu.tick(cycles * 3);
        let mut frame_ready = self.ppu.take_frame_ready();

        // OAM DMA halts the CPU for 513 cycles, plus one alignment cycle when
        // it starts on an odd CPU cycle. The PPU and APU continue to run.
        // Defer this until the instruction containing the $4014 write has
        // completed so the halt begins at the following CPU-cycle boundary.
        if self.oam_dma_pending {
            self.oam_dma_pending = false;
            let stall = 513 + (self.cycles & 1);
            self.cycles += stall;
            for _ in 0..stall {
                self.apu.tick(1);
                self.ppu.tick(3);
                frame_ready |= self.ppu.take_frame_ready();
            }
        }

        // DMC sample fetch: when the APU's memory reader wants a byte, the
        // bus reads it and stalls the CPU. Hardware stalls 1-4 cycles
        // depending on alignment (https://www.nesdev.org/wiki/DMA); we use
        // the common case of 4 and keep the PPU and APU running through it.
        // Servicing once per CPU instruction (rather than the moment the
        // sample buffer empties) is at most a few cycles late.
        while let Some(addr) = self.apu.dmc_dma_request() {
            let value = self.mem_read(addr);
            self.apu.dmc_dma_load(value);
            self.cycles += 4;
            self.apu.tick(4);
            self.ppu.tick(12);
            frame_ready |= self.ppu.take_frame_ready();
        }

        self.deliver_audio_chunks();

        // Presentation, input sampling, and audio delivery happen at the start
        // of vblank, before the CPU can service the NMI and poll the controller.
        // The event remains independent of NMI enable so transitions with NMI
        // disabled cannot skip host frames or accumulate audio.
        if frame_ready {
            self.host_frame_ready = true;
            (self.gameloop_callback)(&self.ppu, &mut self.apu, &mut self.joypad1);
        }
    }

    pub fn take_host_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.host_frame_ready)
    }

    pub fn ppu(&self) -> &NesPPU {
        &self.ppu
    }

    pub fn joypad_mut(&mut self) -> &mut Joypad {
        &mut self.joypad1
    }

    pub fn joypad(&self) -> &Joypad {
        &self.joypad1
    }

    pub fn set_audio_delivery_enabled(&mut self, enabled: bool) {
        self.audio_delivery_enabled = enabled;
    }

    pub fn snapshot(&self) -> BusSnapshot {
        let cloned_mapper = self.mapper.borrow().as_ref().clone_box();
        let mapper = Rc::new(std::cell::RefCell::new(cloned_mapper));
        let ppu = self.ppu.clone_with_mapper(Rc::clone(&mapper));
        BusSnapshot {
            cpu_vram: self.cpu_vram,
            mapper,
            ppu,
            apu: self.apu.clone(),
            cycles: self.cycles,
            oam_dma_pending: self.oam_dma_pending,
            audio_delivery_enabled: self.audio_delivery_enabled,
            host_frame_ready: self.host_frame_ready,
            joypad1: self.joypad1.clone(),
        }
    }

    pub fn restore(&mut self, snapshot: BusSnapshot) {
        self.cpu_vram = snapshot.cpu_vram;
        self.mapper = snapshot.mapper;
        self.ppu = snapshot.ppu;
        self.apu = snapshot.apu;
        self.cycles = snapshot.cycles;
        self.oam_dma_pending = snapshot.oam_dma_pending;
        self.audio_delivery_enabled = snapshot.audio_delivery_enabled;
        self.host_frame_ready = snapshot.host_frame_ready;
        self.joypad1 = snapshot.joypad1;
    }

    pub fn poll_nmi_status(&mut self) -> Option<u8> {
        self.ppu.poll_nmi_interrupt()
    }

    // The IRQ line seen by the CPU. Both sources are level-triggered and stay
    // asserted until acknowledged: the APU (frame counter or DMC) and the
    // mapper (MMC3 scanline counter, acked by a write to $E000).
    pub fn poll_irq_status(&self) -> bool {
        self.apu.irq_pending() || self.mapper.borrow().irq_pending()
    }

    #[cfg(test)]
    pub(crate) fn cpu_cycles(&self) -> usize {
        self.cycles
    }
}

impl Mem for Bus<'_> {
    fn mem_read(&mut self, addr: u16) -> u8 {
        match addr {
            RAM..=RAM_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00000111_11111111;
                self.cpu_vram[mirror_down_addr as usize]
            }
            // Reads from nominally write-only PPU ports expose the PPU's
            // internal I/O latch without refreshing it.
            0x2000 | 0x2001 | 0x2003 | 0x2005 | 0x2006 => self.ppu.read_io_data_bus(),
            0x2002 => self.ppu.read_status(),
            0x2004 => self.ppu.read_oam_data(),
            0x2007 => self.ppu.read_data(),

            0x2008..=PPU_REGISTERS_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00100000_00000111;
                self.mem_read(mirror_down_addr)
            }
            0x4015 => self.apu.read_status(),
            0x4016 => self.joypad1.read(),
            // $4017 reads controller 2 on real hardware. This emulator only
            // exposes joypad 1, so return an idle value instead of spamming
            // stdout for games that poll both controller ports.
            0x4017 => 0,
            // $6000-$7FFF is PRG-RAM; $8000-$FFFF is PRG-ROM. Both go through
            // the mapper.
            0x6000..=0xFFFF => self.mapper.borrow_mut().cpu_read(addr),

            _ => {
                println!("Ignoring mem access at {}", addr);
                0
            }
        }
    }

    fn mem_write(&mut self, addr: u16, data: u8) {
        match addr {
            RAM..=RAM_MIRRORS_END => {
                let mirror_down_addr = addr & 0b11111111111;
                self.cpu_vram[mirror_down_addr as usize] = data;
            }
            0x2000 => {
                self.ppu.write_to_ctrl(data);
            }
            0x2001 => {
                self.ppu.write_to_mask(data);
            }

            // PPUSTATUS is read-only, but writes still drive the PPU I/O bus.
            0x2002 => self.ppu.write_to_status(data),

            0x2003 => {
                self.ppu.write_to_oam_addr(data);
            }
            0x2004 => {
                self.ppu.write_to_oam_data(data);
            }
            0x2005 => {
                self.ppu.write_to_scroll(data);
            }
            0x2006 => {
                self.ppu.write_to_ppu_addr(data);
            }
            0x2007 => {
                self.ppu.write_to_data(data);
            }

            0x2008..=PPU_REGISTERS_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00100000_00000111;
                self.mem_write(mirror_down_addr, data);
            }

            // OAM DMA ($4014): copy CPU page $XX00-$XXFF into PPU OAM.
            // The book wires this in ch7; doing it now keeps write_oam_dma live
            // and games that DMA sprites during boot behave correctly.
            0x4014 => {
                let hi: u16 = (data as u16) << 8;
                let mut buffer: [u8; 256] = [0; 256];
                for i in 0..256u16 {
                    buffer[i as usize] = self.mem_read(hi + i);
                }
                self.ppu.write_oam_dma(&buffer);
                self.oam_dma_pending = true;
            }

            // APU registers ($4014 is OAM DMA above, $4016 the joypad below;
            // reads of $4017 would be joypad 2, but writes go to the APU
            // frame counter).
            0x4000..=0x4013 | 0x4015 | 0x4017 => {
                self.apu.write_register(addr, data);
            }

            0x4016 => {
                self.joypad1.write(data);
            }

            // Writes to $6000-$7FFF hit PRG-RAM; writes to $8000-$FFFF are the
            // mapper's bank-switch control registers, not errors.
            0x6000..=0xFFFF => self.mapper.borrow_mut().cpu_write(addr, data),

            _ => {
                println!("Ignoring mem write-access at {}", addr);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::test::test_rom;

    fn test_bus<'a>() -> Bus<'a> {
        Bus::new(test_rom(vec![]), |_, _, _| {})
    }

    #[test]
    fn apu_registers_reachable_through_bus() {
        let mut bus = test_bus();
        bus.mem_write(0x4015, 0x01); // enable pulse 1
        bus.mem_write(0x4003, 0x08); // load its length counter
        assert_eq!(bus.mem_read(0x4015) & 0x01, 0x01);
    }

    #[test]
    fn joypad2_reads_do_not_fall_through_to_unmapped_io() {
        let mut bus = test_bus();
        assert_eq!(bus.mem_read(0x4017), 0);
    }

    #[test]
    fn frame_irq_reaches_bus_irq_line_and_status_read_clears_it() {
        let mut bus = test_bus();
        // 4-step mode with IRQ enabled is the power-up default; one full
        // sequence is 29830 CPU cycles.
        for _ in 0..(29830 / 10) {
            bus.tick(10);
        }
        assert!(bus.poll_irq_status());
        assert_eq!(bus.mem_read(0x4015) & 0x40, 0x40);
        assert!(!bus.poll_irq_status());
    }

    #[test]
    fn dmc_dma_fetches_sample_bytes_from_prg_rom() {
        let mut bus = test_bus();
        // test_rom PRG is all zeroes; what matters here is that enabling
        // the DMC drains bytes via DMA as the bus ticks.
        bus.mem_write(0x4012, 0x00); // sample address $C000
        bus.mem_write(0x4013, 0x00); // length 1 byte
        bus.mem_write(0x4015, 0x10); // enable DMC
        bus.tick(1); // services the fetch immediately
        assert_eq!(bus.mem_read(0x4015) & 0x10, 0); // 0 bytes remaining
    }

    #[test]
    fn frame_callback_fires_at_vblank_without_nmi_enabled() {
        let callbacks = std::rc::Rc::new(std::cell::Cell::new(0));
        let callback_count = callbacks.clone();
        let mut bus = Bus::new(test_rom(vec![]), move |_, _, _| {
            callback_count.set(callback_count.get() + 1);
        });

        // The first vblank begins while the raster is still well short of the
        // end-of-frame wrap. PPUCTRL remains at reset with NMI disabled.
        for _ in 0..27_393 {
            bus.tick(1);
        }
        assert_eq!(callbacks.get(), 0);
        bus.tick(1);
        assert_eq!(callbacks.get(), 1);
    }

    #[test]
    fn vblank_callback_input_is_visible_to_the_next_controller_poll() {
        let mut bus = Bus::new(test_rom(vec![]), |_, _, joypad| {
            joypad.set_button_pressed_status(crate::joypad::JoypadButton::BUTTON_A, true);
        });

        for _ in 0..27_394 {
            bus.tick(1);
        }
        bus.mem_write(0x4016, 1);
        bus.mem_write(0x4016, 0);
        assert_eq!(bus.mem_read(0x4016), 1);
    }

    #[test]
    fn audio_chunks_are_delivered_before_a_video_frame_completes() {
        let delivered = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let callback_delivered = delivered.clone();
        let mut bus = Bus::new_with_audio(
            test_rom(vec![]),
            |_, _, _| {},
            256,
            move |samples| {
                assert_eq!(samples.len(), 256);
                callback_delivered.set(callback_delivered.get() + samples.len());
            },
        );
        bus.apu.set_sample_rate(crate::audio::SAMPLE_RATE);

        // 11,000 CPU cycles produce about 271 samples, well before the first
        // vblank at ~27,394 cycles.
        for _ in 0..220 {
            bus.tick(50);
        }
        assert_eq!(delivered.get(), 256);
        assert!(bus.apu.buffered_samples() < 256);
    }

    #[test]
    fn oam_dma_stalls_cpu_while_ppu_and_apu_keep_running() {
        let mut bus = test_bus();
        bus.mem_write(0x4014, 0x00);
        bus.tick(4);

        assert_eq!(bus.cycles, 4 + 513);

        let mut odd_bus = test_bus();
        odd_bus.tick(1);
        odd_bus.mem_write(0x4014, 0x00);
        odd_bus.tick(4);

        assert_eq!(odd_bus.cycles, 1 + 4 + 514);
    }

    #[test]
    fn writes_to_read_only_ppustatus_drive_the_ppu_io_bus() {
        let mut bus = test_bus();
        bus.mem_write(0x2002, 0xa5);

        assert_eq!(bus.mem_read(0x2000), 0xa5);
        assert_eq!(bus.mem_read(0x3ff9), 0xa5);
    }
}
