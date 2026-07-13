use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 2 (UxROM): 16 KB PRG bank switching. $8000-$BFFF selects one of the
// 16 KB banks (low bits of any write to $8000-$FFFF); $C000-$FFFF is hardwired
// to the last 16 KB bank. CHR is always 8 KB of CHR-RAM. Mirroring is fixed by
// the header.
pub struct Uxrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_bank: usize,  // switchable 16 KB bank at $8000-$BFFF
    last_bank: usize, // fixed 16 KB bank at $C000-$FFFF
    num_banks: usize,
    mirroring: Mirroring,
}

impl Uxrom {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; 0x2000] } else { rom.chr_rom };
        let num_banks = (rom.prg_rom.len() / 0x4000).max(1);
        Uxrom {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_bank: 0,
            last_bank: num_banks - 1,
            num_banks,
            mirroring: rom.screen_mirroring,
        }
    }
}

impl Mapper for Uxrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xbfff => self.prg_rom[self.prg_bank * 0x4000 + (addr as usize - 0x8000)],
            0xc000..=0xffff => self.prg_rom[self.last_bank * 0x4000 + (addr as usize - 0xc000)],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        // Any write to ROM space selects the low bank (bus conflicts ignored).
        if addr >= 0x8000 {
            self.prg_bank = (data as usize) % self.num_banks;
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
        // Fill each 16 KB bank with its own index so reads are identifiable.
        let mut prg_rom = Vec::new();
        for b in 0..prg_banks {
            prg_rom.extend(std::iter::repeat(b as u8).take(0x4000));
        }
        Rom {
            prg_rom,
            chr_rom: vec![],
            mapper: 2,
            screen_mirroring: Mirroring::Vertical,
        }
    }

    #[test]
    fn selects_low_bank_and_fixes_last() {
        let mut m = Uxrom::from_rom(rom(4));
        // Default low bank is 0, last bank is 3.
        assert_eq!(m.cpu_read(0x8000), 0);
        assert_eq!(m.cpu_read(0xc000), 3);
        // Switch the low window to bank 2; the fixed window is unchanged.
        m.cpu_write(0x8000, 2);
        assert_eq!(m.cpu_read(0x8000), 2);
        assert_eq!(m.cpu_read(0xc000), 3);
    }

    #[test]
    fn bank_select_wraps_to_rom_size() {
        let mut m = Uxrom::from_rom(rom(4));
        m.cpu_write(0xffff, 6); // 6 % 4 == 2
        assert_eq!(m.cpu_read(0x8000), 2);
    }

    #[test]
    fn chr_ram_is_writable() {
        let mut m = Uxrom::from_rom(rom(2));
        m.ppu_write(0x0010, 0xab);
        assert_eq!(m.ppu_read(0x0010), 0xab);
    }
}
