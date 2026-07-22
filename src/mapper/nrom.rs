use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 0 (NROM): no banking. PRG ROM is 16 or 32 KB mapped at $8000-$FFFF
// (a 16 KB image is mirrored into both halves); CHR is a fixed 8 KB window,
// either ROM or, when the cartridge ships none, 8 KB of CHR-RAM. Optional
// 8 KB of PRG-RAM lives at $6000-$7FFF.
#[derive(Clone)]
pub struct Nrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
}

impl Nrom {
    pub fn new(prg_rom: Vec<u8>, chr: Vec<u8>, chr_is_ram: bool, mirroring: Mirroring) -> Self {
        Nrom {
            prg_rom,
            chr,
            chr_is_ram,
            prg_ram: vec![0; 0x2000],
            mirroring,
        }
    }

    pub fn from_rom(rom: Rom) -> Self {
        // No CHR ROM in the header means the cartridge uses 8 KB of CHR-RAM.
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram {
            vec![0; rom.memory.chr_ram_size()]
        } else {
            rom.chr_rom
        };
        let mut mapper = Nrom::new(rom.prg_rom, chr, chr_is_ram, rom.screen_mirroring);
        mapper.prg_ram.resize(rom.memory.prg_ram_size(), 0);
        mapper
    }
}

impl Mapper for Nrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7fff => self.prg_ram.get((addr - 0x6000) as usize).copied().unwrap_or(0),
            0x8000..=0xffff => {
                let mut index = addr - 0x8000;
                // Mirror the 16 KB image across both banks.
                if self.prg_rom.len() == 0x4000 && index >= 0x4000 {
                    index %= 0x4000;
                }
                self.prg_rom[index as usize]
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7fff => {
                if let Some(byte) = self.prg_ram.get_mut((addr - 0x6000) as usize) { *byte = data; }
            }
            // NROM has no bank registers; writes to ROM space are inert.
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.chr[addr as usize]
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_is_ram {
            self.chr[addr as usize] = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn prg_ram(&self) -> Option<&[u8]> { (!self.prg_ram.is_empty()).then_some(&self.prg_ram) }
    fn prg_ram_mut(&mut self) -> Option<&mut [u8]> { (!self.prg_ram.is_empty()).then_some(&mut self.prg_ram) }
    fn chr_ram(&self) -> Option<&[u8]> { self.chr_is_ram.then_some(&self.chr) }
    fn chr_ram_mut(&mut self) -> Option<&mut [u8]> { self.chr_is_ram.then_some(&mut self.chr) }
}
