use std::cell::RefCell;
use std::rc::Rc;

use crate::cartridge::{Mirroring, Rom};

pub mod axrom;
pub mod cnrom;
pub mod gnrom;
pub mod mmc1;
pub mod mmc3;
pub mod nrom;
pub mod uxrom;

use axrom::Axrom;
use cnrom::Cnrom;
use gnrom::Gnrom;
use mmc1::Mmc1;
use mmc3::Mmc3;
use nrom::Nrom;
use uxrom::Uxrom;

// A cartridge mapper. It owns PRG ROM, CHR ROM/RAM, PRG RAM, any bank
// registers, and the mirroring state, and is visible from both sides of the
// machine at once: the CPU bus drives PRG space ($6000-$FFFF) and the PPU +
// dot pipeline drives CHR space ($0000-$1FFF) plus nametable mirroring. Writes to
// PRG ROM space are how bank switches arrive, so they land in `cpu_write`
// rather than being errors.
pub trait Mapper {
    fn cpu_read(&mut self, addr: u16) -> u8; // $6000-$FFFF: PRG-RAM + PRG-ROM
    fn cpu_write(&mut self, addr: u16, data: u8); // bank-register writes land here
    fn ppu_read(&mut self, addr: u16) -> u8; // $0000-$1FFF CHR
    fn ppu_write(&mut self, addr: u16, data: u8); // CHR-RAM
    fn mirroring(&self) -> Mirroring; // MMC1 etc. set this at runtime
    // Observe the address driven by a real PPU memory access. `ppu_cycle` is a
    // monotonically increasing PPU-dot timestamp, allowing edge-sensitive
    // mappers to distinguish a qualified A12 low period from a short pulse.
    fn on_ppu_bus_access(&mut self, _addr: u16, _ppu_cycle: u64) {}
    // Mapper IRQ line (MMC3). Level-triggered like the APU's: stays asserted
    // until the program acknowledges it (MMC3: a write to $E000). Default off
    // for mappers with no IRQ source.
    fn irq_pending(&self) -> bool {
        false
    }
}

// Shared between Bus and NesPPU so both see the same mapper state. Cloned
// (the Rc) into each; the RefCell gives interior mutability since the PPU
// accesses it throughout CPU and PPU bus activity.
pub type SharedMapper = Rc<RefCell<Box<dyn Mapper>>>;

// Builds the mapper named by the iNES header. Consumes the parsed Rom.
pub fn from_rom(rom: Rom) -> SharedMapper {
    let mapper: Box<dyn Mapper> = match rom.mapper {
        0 => Box::new(Nrom::from_rom(rom)),
        1 => Box::new(Mmc1::from_rom(rom)),
        2 => Box::new(Uxrom::from_rom(rom)),
        3 => Box::new(Cnrom::from_rom(rom)),
        4 => Box::new(Mmc3::from_rom(rom)),
        7 => Box::new(Axrom::from_rom(rom)),
        66 => Box::new(Gnrom::from_rom(rom)),
        other => panic!("Mapper {} is not supported yet", other),
    };
    Rc::new(RefCell::new(mapper))
}

// Test helper: an NROM cartridge with the given CHR (as writable CHR-RAM so
// tests may also exercise CHR writes) and mirroring, over 32 KB of zeroed PRG.
#[cfg(test)]
pub fn test_nrom(chr: Vec<u8>, mirroring: Mirroring) -> SharedMapper {
    let nrom = Nrom::new(vec![0; 0x8000], chr, true, mirroring);
    Rc::new(RefCell::new(Box::new(nrom) as Box<dyn Mapper>))
}
