use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 9 (MMC2 / PxROM), used by Mike Tyson's Punch-Out!!. PRG exposes one
// switchable 8 KiB bank followed by the final three fixed banks. Each 4 KiB
// CHR window has an FD/FE pair selected by a latch that changes only after the
// PPU reads a trigger address, so the trigger tile itself uses the old bank.
#[derive(Clone)]
pub struct Mmc2 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_ram: Vec<u8>,

    prg_bank: usize,
    chr_fd_0000: usize,
    chr_fe_0000: usize,
    chr_fd_1000: usize,
    chr_fe_1000: usize,
    latch_0000: Latch,
    latch_1000: Latch,
    mirroring: Mirroring,

    num_prg_banks: usize,
    num_chr_banks: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Latch {
    Fd,
    Fe,
}

impl Mmc2 {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram {
            vec![0; rom.memory.chr.size.max(0x2000)]
        } else {
            rom.chr_rom
        };
        let num_prg_banks = (rom.prg_rom.len() / 0x2000).max(1);
        let num_chr_banks = (chr.len() / 0x1000).max(1);
        Self {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_ram: vec![0; rom.memory.prg_ram.size],
            prg_bank: 0,
            chr_fd_0000: 0,
            chr_fe_0000: 0,
            chr_fd_1000: 0,
            chr_fe_1000: 0,
            // MMC2 has no guaranteed power-on register state. FE is the
            // conventional deterministic emulator state and Punch-Out!!
            // initializes all bank registers before relying on the latches.
            latch_0000: Latch::Fe,
            latch_1000: Latch::Fe,
            mirroring: rom.screen_mirroring,
            num_prg_banks,
            num_chr_banks,
        }
    }

    fn prg_offset(&self, addr: u16) -> usize {
        let window = ((addr - 0x8000) / 0x2000) as usize;
        let bank = match window {
            0 => self.prg_bank,
            1..=3 => self.num_prg_banks.saturating_sub(4 - window),
            _ => unreachable!(),
        } % self.num_prg_banks;
        bank * 0x2000 + (addr as usize & 0x1fff)
    }

    fn chr_bank(&self, addr: u16) -> usize {
        let bank = if addr < 0x1000 {
            match self.latch_0000 {
                Latch::Fd => self.chr_fd_0000,
                Latch::Fe => self.chr_fe_0000,
            }
        } else {
            match self.latch_1000 {
                Latch::Fd => self.chr_fd_1000,
                Latch::Fe => self.chr_fe_1000,
            }
        };
        bank % self.num_chr_banks
    }

    fn update_latch_after_read(&mut self, addr: u16) {
        match addr {
            0x0fd8 => self.latch_0000 = Latch::Fd,
            0x0fe8 => self.latch_0000 = Latch::Fe,
            0x1fd8..=0x1fdf => self.latch_1000 = Latch::Fd,
            0x1fe8..=0x1fef => self.latch_1000 = Latch::Fe,
            _ => {}
        }
    }
}

impl Mapper for Mmc2 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7fff => self
                .prg_ram
                .get((addr - 0x6000) as usize)
                .copied()
                .unwrap_or(0),
            0x8000..=0xffff => self.prg_rom[self.prg_offset(addr)],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7fff => {
                if let Some(byte) = self.prg_ram.get_mut((addr - 0x6000) as usize) {
                    *byte = data;
                }
            }
            0xa000..=0xafff => self.prg_bank = (data as usize & 0x0f) % self.num_prg_banks,
            0xb000..=0xbfff => self.chr_fd_0000 = data as usize & 0x1f,
            0xc000..=0xcfff => self.chr_fe_0000 = data as usize & 0x1f,
            0xd000..=0xdfff => self.chr_fd_1000 = data as usize & 0x1f,
            0xe000..=0xefff => self.chr_fe_1000 = data as usize & 0x1f,
            0xf000..=0xffff => {
                self.mirroring = if data & 1 == 0 {
                    Mirroring::Vertical
                } else {
                    Mirroring::Horizontal
                };
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        let bank = self.chr_bank(addr);
        let value = self.chr[bank * 0x1000 + (addr as usize & 0x0fff)];
        self.update_latch_after_read(addr);
        value
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_is_ram {
            let bank = self.chr_bank(addr);
            self.chr[bank * 0x1000 + (addr as usize & 0x0fff)] = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    // The front-panel reset line is not connected to MMC2. Preserve its
    // mapper registers and latches while the CPU/PPU reset in place.
    fn reset(&mut self) {}

    fn prg_ram(&self) -> Option<&[u8]> {
        (!self.prg_ram.is_empty()).then_some(&self.prg_ram)
    }

    fn prg_ram_mut(&mut self) -> Option<&mut [u8]> {
        (!self.prg_ram.is_empty()).then_some(&mut self.prg_ram)
    }

    fn chr_ram(&self) -> Option<&[u8]> {
        self.chr_is_ram.then_some(&self.chr)
    }

    fn chr_ram_mut(&mut self) -> Option<&mut [u8]> {
        self.chr_is_ram.then_some(&mut self.chr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom() -> Rom {
        let mut prg_rom = Vec::new();
        for bank in 0..16 {
            prg_rom.extend(std::iter::repeat(bank).take(0x2000));
        }
        let mut chr_rom = Vec::new();
        for bank in 0..32 {
            chr_rom.extend(std::iter::repeat(0x80 | bank).take(0x1000));
        }
        Rom {
            memory: crate::cartridge::CartridgeMemory::test_defaults(prg_rom.len(), chr_rom.len()),
            save_path: None,
            prg_rom,
            chr_rom,
            mapper: 9,
            metadata: crate::cartridge::CartridgeMetadata::test_defaults(),
            screen_mirroring: Mirroring::Horizontal,
        }
    }

    #[test]
    fn switches_first_prg_bank_and_fixes_last_three() {
        let mut mapper = Mmc2::from_rom(rom());
        mapper.cpu_write(0xa000, 5);
        assert_eq!(mapper.cpu_read(0x8000), 5);
        assert_eq!(mapper.cpu_read(0xa000), 13);
        assert_eq!(mapper.cpu_read(0xc000), 14);
        assert_eq!(mapper.cpu_read(0xe000), 15);
    }

    #[test]
    fn chr_trigger_byte_uses_old_bank_then_changes_latch() {
        let mut mapper = Mmc2::from_rom(rom());
        mapper.cpu_write(0xb000, 3);
        mapper.cpu_write(0xc000, 4);
        assert_eq!(mapper.ppu_read(0x0000), 0x84); // FE power-on latch
        assert_eq!(mapper.ppu_read(0x0fd8), 0x84); // trigger still old bank
        assert_eq!(mapper.ppu_read(0x0000), 0x83);
        assert_eq!(mapper.ppu_read(0x0fe8), 0x83);
        assert_eq!(mapper.ppu_read(0x0000), 0x84);
    }

    #[test]
    fn upper_latch_responds_to_full_eight_byte_ranges_only() {
        let mut mapper = Mmc2::from_rom(rom());
        mapper.cpu_write(0xd000, 7);
        mapper.cpu_write(0xe000, 9);
        assert_eq!(mapper.ppu_read(0x1fd7), 0x89);
        assert_eq!(mapper.ppu_read(0x1000), 0x89);
        assert_eq!(mapper.ppu_read(0x1fdf), 0x89);
        assert_eq!(mapper.ppu_read(0x1000), 0x87);
        assert_eq!(mapper.ppu_read(0x1fe8), 0x87);
        assert_eq!(mapper.ppu_read(0x1000), 0x89);
    }

    #[test]
    fn mirroring_register_selects_vertical_and_horizontal() {
        let mut mapper = Mmc2::from_rom(rom());
        mapper.cpu_write(0xf000, 0);
        assert_eq!(mapper.mirroring(), Mirroring::Vertical);
        mapper.cpu_write(0xffff, 1);
        assert_eq!(mapper.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn front_panel_reset_preserves_mmc2_state() {
        let mut mapper = Mmc2::from_rom(rom());
        mapper.cpu_write(0xa000, 5);
        mapper.cpu_write(0xb000, 3);
        mapper.ppu_read(0x0fd8);
        mapper.cpu_write(0xf000, 0);
        mapper.reset();
        assert_eq!(mapper.cpu_read(0x8000), 5);
        assert_eq!(mapper.ppu_read(0), 0x83);
        assert_eq!(mapper.mirroring(), Mirroring::Vertical);
    }
}
