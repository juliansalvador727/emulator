use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 3 (CNROM): fixed PRG (16 KB mirrored or 32 KB, like NROM), with 8 KB
// CHR-ROM bank switching. Any write to $8000-$FFFF selects the 8 KB CHR bank.
// Mirroring is fixed by the header.
#[derive(Clone)]
pub struct Cnrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_ram: Vec<u8>,
    chr_bank: usize,
    num_chr_banks: usize,
    mirroring: Mirroring,
}

impl Cnrom {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; rom.memory.chr.size] } else { rom.chr_rom };
        let num_chr_banks = (chr.len() / 0x2000).max(1);
        Cnrom {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_ram: vec![0; rom.memory.prg_ram.size],
            chr_bank: 0,
            num_chr_banks,
            mirroring: rom.screen_mirroring,
        }
    }
}

impl Mapper for Cnrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7fff => self.prg_ram.get((addr - 0x6000) as usize).copied().unwrap_or(0),
            0x8000..=0xffff => {
                let mut index = addr - 0x8000;
                // Mirror a 16 KB image across both banks.
                if self.prg_rom.len() == 0x4000 && index >= 0x4000 {
                    index %= 0x4000;
                }
                self.prg_rom[index as usize]
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if let 0x6000..=0x7fff = addr {
            if let Some(byte) = self.prg_ram.get_mut((addr - 0x6000) as usize) { *byte = data; }
            return;
        }
        // The written value selects the 8 KB CHR bank (bus conflicts ignored).
        if addr >= 0x8000 {
            self.chr_bank = (data as usize) % self.num_chr_banks;
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
    fn reset(&mut self) { self.chr_bank = 0; }
    fn prg_ram(&self) -> Option<&[u8]> { (!self.prg_ram.is_empty()).then_some(&self.prg_ram) }
    fn prg_ram_mut(&mut self) -> Option<&mut [u8]> { (!self.prg_ram.is_empty()).then_some(&mut self.prg_ram) }
    fn chr_ram(&self) -> Option<&[u8]> { self.chr_is_ram.then_some(&self.chr) }
    fn chr_ram_mut(&mut self) -> Option<&mut [u8]> { self.chr_is_ram.then_some(&mut self.chr) }
}

#[cfg(test)]
mod test {
    use super::*;

    fn rom(chr_banks: usize) -> Rom {
        // Fill each 8 KB CHR bank with its own index so reads are identifiable.
        let mut chr_rom = Vec::new();
        for b in 0..chr_banks {
            chr_rom.extend(std::iter::repeat(b as u8).take(0x2000));
        }
        Rom {
            memory: crate::cartridge::CartridgeMemory::test_defaults(0x8000, chr_rom.len()),
            save_path: None,
            prg_rom: vec![0; 0x8000],
            chr_rom,
            mapper: 3,
            screen_mirroring: Mirroring::Horizontal,
        }
    }

    #[test]
    fn switches_chr_bank() {
        let mut m = Cnrom::from_rom(rom(4));
        assert_eq!(m.ppu_read(0x0000), 0);
        m.cpu_write(0x8000, 2);
        assert_eq!(m.ppu_read(0x0000), 2);
        assert_eq!(m.ppu_read(0x1fff), 2);
    }

    #[test]
    fn bank_select_wraps() {
        let mut m = Cnrom::from_rom(rom(4));
        m.cpu_write(0x9abc, 5); // 5 % 4 == 1
        assert_eq!(m.ppu_read(0x0000), 1);
    }

    #[test]
    fn chr_rom_is_read_only() {
        let mut m = Cnrom::from_rom(rom(2));
        m.ppu_write(0x0000, 0xff);
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn reset_restores_chr_bank_zero() {
        let mut m = Cnrom::from_rom(rom(2));
        m.cpu_write(0x8000, 1);
        m.reset();
        assert_eq!(m.ppu_read(0), 0);
    }
}
