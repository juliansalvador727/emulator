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
    gameloop_callback: Box<dyn FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call>,
    joypad1: Joypad,
}

impl<'a> Bus<'a> {
    pub fn new<'call, F>(rom: Rom, gameloop_callback: F) -> Bus<'call>
    where
        F: FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call,
    {
        let mapper = mapper::from_rom(rom);
        let ppu = NesPPU::new(Rc::clone(&mapper));
        Bus {
            cpu_vram: [0; 2048],
            mapper: mapper,
            ppu: ppu,
            apu: NesAPU::new(),
            cycles: 0,
            gameloop_callback: Box::from(gameloop_callback),
            joypad1: Joypad::new(),
        }
    }
    pub fn tick(&mut self, cycles: u8) {
        self.cycles += cycles as usize;
        self.apu.tick(cycles);
        let mut frame_complete = self.ppu.tick(cycles * 3);

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
            frame_complete |= self.ppu.tick(12);
        }

        // Presentation and audio draining are video-frame events, independent
        // of whether a game enables NMI. Tying this callback to an NMI edge
        // silently skipped frames (and accumulated audio) during NMI-disabled
        // transitions, making profiling and host playback drift.
        if frame_complete {
            (self.gameloop_callback)(&self.ppu, &mut self.apu, &mut self.joypad1);
        }
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
}

impl Mem for Bus<'_> {
    fn mem_read(&mut self, addr: u16) -> u8 {
        match addr {
            RAM..=RAM_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00000111_11111111;
                self.cpu_vram[mirror_down_addr as usize]
            }
            0x2000 | 0x2001 | 0x2003 | 0x2005 | 0x2006 | 0x4014 => {
                panic!("Attempt to read from write-only PPU address {:x}", addr);
            }
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

            0x2002 => panic!("attempt to write to PPU status register"),

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
    fn frame_callback_does_not_depend_on_nmi_enable() {
        let callbacks = std::rc::Rc::new(std::cell::Cell::new(0));
        let callback_count = callbacks.clone();
        let mut bus = Bus::new(test_rom(vec![]), move |_, _, _| {
            callback_count.set(callback_count.get() + 1);
        });

        // One NTSC PPU frame is 262 * 341 dots, advanced three dots per CPU
        // cycle. PPUCTRL remains at its reset value, with NMI disabled.
        for _ in 0..29_781 {
            bus.tick(1);
        }
        assert_eq!(callbacks.get(), 1);
    }
}
