use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 66 (GxROM/GNROM): a single register (any write to $8000-$FFFF) selects
// both a 32 KB PRG bank (bits 4-5) and an 8 KB CHR bank (bits 0-1). Mirroring is
// fixed by the header.
#[derive(Clone)]
pub struct Gnrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_bank: usize, // 32 KB bank at $8000-$FFFF
    chr_bank: usize, // 8 KB CHR bank
    num_prg_banks: usize,
    num_chr_banks: usize,
    mirroring: Mirroring,
}

impl Gnrom {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; 0x2000] } else { rom.chr_rom };
        let num_prg_banks = (rom.prg_rom.len() / 0x8000).max(1);
        let num_chr_banks = (chr.len() / 0x2000).max(1);
        Gnrom {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_bank: 0,
            chr_bank: 0,
            num_prg_banks,
            num_chr_banks,
            mirroring: rom.screen_mirroring,
        }
    }
}

impl Mapper for Gnrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xffff => self.prg_rom[self.prg_bank * 0x8000 + (addr as usize - 0x8000)],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            self.prg_bank = ((data as usize >> 4) & 0x3) % self.num_prg_banks;
            self.chr_bank = (data as usize & 0x3) % self.num_chr_banks;
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.chr[self.chr_bank * 0x2000 + addr as usize]
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_is_ram {
            self.chr[self.chr_bank * 0x2000 + addr as usize] = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn rom(prg_banks: usize, chr_banks: usize) -> Rom {
        let mut prg_rom = Vec::new();
        for b in 0..prg_banks {
            prg_rom.extend(std::iter::repeat(b as u8).take(0x8000));
        }
        let mut chr_rom = Vec::new();
        for b in 0..chr_banks {
            chr_rom.extend(std::iter::repeat(0x80 | b as u8).take(0x2000));
        }
        Rom {
            prg_rom,
            chr_rom,
            mapper: 66,
            screen_mirroring: Mirroring::Vertical,
        }
    }

    #[test]
    fn one_register_switches_both_banks() {
        let mut m = Gnrom::from_rom(rom(4, 4));
        assert_eq!(m.cpu_read(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 0x80);
        // PRG bank = bits 4-5 (=2), CHR bank = bits 0-1 (=3).
        m.cpu_write(0x8000, (2 << 4) | 3);
        assert_eq!(m.cpu_read(0x8000), 2);
        assert_eq!(m.ppu_read(0x0000), 0x83);
    }
}
