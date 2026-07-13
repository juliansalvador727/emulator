use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 7 (AxROM): 32 KB PRG bank switching over the whole $8000-$FFFF window,
// plus single-screen nametable mirroring picked by the same register. Bits 0-2
// of a write select the 32 KB PRG bank; bit 4 selects which single nametable is
// mirrored. CHR is always 8 KB of CHR-RAM.
pub struct Axrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_bank: usize, // switchable 32 KB bank at $8000-$FFFF
    num_banks: usize,
    mirroring: Mirroring,
}

impl Axrom {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; 0x2000] } else { rom.chr_rom };
        let num_banks = (rom.prg_rom.len() / 0x8000).max(1);
        Axrom {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_bank: 0,
            num_banks,
            // Powers up single-screen; the game sets which page on its first write.
            mirroring: Mirroring::SingleScreenLower,
        }
    }
}

impl Mapper for Axrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xffff => self.prg_rom[self.prg_bank * 0x8000 + (addr as usize - 0x8000)],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            self.prg_bank = (data as usize & 0x7) % self.num_banks;
            self.mirroring = if data & 0x10 != 0 {
                Mirroring::SingleScreenUpper
            } else {
                Mirroring::SingleScreenLower
            };
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
}

#[cfg(test)]
mod test {
    use super::*;

    fn rom(prg_banks: usize) -> Rom {
        // Fill each 32 KB bank with its own index so reads are identifiable.
        let mut prg_rom = Vec::new();
        for b in 0..prg_banks {
            prg_rom.extend(std::iter::repeat(b as u8).take(0x8000));
        }
        Rom {
            prg_rom,
            chr_rom: vec![],
            mapper: 7,
            screen_mirroring: Mirroring::Horizontal, // ignored by AxROM
        }
    }

    #[test]
    fn switches_prg_bank() {
        let mut m = Axrom::from_rom(rom(4));
        assert_eq!(m.cpu_read(0x8000), 0);
        m.cpu_write(0x8000, 2);
        assert_eq!(m.cpu_read(0x8000), 2);
        assert_eq!(m.cpu_read(0xffff), 2);
    }

    #[test]
    fn selects_single_screen_page() {
        let mut m = Axrom::from_rom(rom(2));
        m.cpu_write(0x8000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0x8000, 0x10);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }
}
