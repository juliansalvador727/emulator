use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 1 (MMC1 / SxROM). The distinguishing features versus the simple
// mappers: registers are loaded one bit at a time through a 5-bit serial shift
// register, mirroring is picked at runtime, and both PRG and CHR have switchable
// bank *modes*.
//
// A write to $8000-$FFFF with bit 7 set clears the shift register (and forces
// PRG mode 3). Otherwise bit 0 of the value is shifted in; on the fifth write
// the accumulated 5 bits are copied into one of four internal registers, chosen
// by address bits 14-13:
//   $8000-$9FFF Control   (mirroring, PRG mode, CHR mode)
//   $A000-$BFFF CHR bank 0
//   $C000-$DFFF CHR bank 1
//   $E000-$FFFF PRG bank
//
// This implements the common SxROM behaviour; the 512 KB SUROM high-PRG-bit
// trick is not modelled (none of the usual MMC1 games need it). Optional 8 KB
// PRG-RAM lives at $6000-$7FFF. CHR is 4 KB-banked ROM, or a flat 8 KB of
// CHR-RAM when the cartridge ships no CHR (the bank registers then don't matter
// since the RAM is only 8 KB).
pub struct Mmc1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_ram: [u8; 0x2000],

    shift: u8,     // serial load register; sentinel bit marks the 5th write
    control: u8,   // mirroring (0-1), PRG mode (2-3), CHR mode (4)
    chr_bank0: u8, // 4 KB CHR bank at $0000 (low bit ignored in 8 KB mode)
    chr_bank1: u8, // 4 KB CHR bank at $1000 (used only in 4 KB mode)
    prg_bank: u8,  // 16 KB PRG bank

    num_prg_banks: usize, // in 16 KB units
    num_chr_banks: usize, // in 4 KB units
}

// The shift register starts with a 1 in bit 4; after four right-shifts that 1
// reaches bit 0, so seeing it there means the incoming write is the fifth.
const SHIFT_RESET: u8 = 0x10;

impl Mmc1 {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; 0x2000] } else { rom.chr_rom };
        let num_prg_banks = (rom.prg_rom.len() / 0x4000).max(1);
        let num_chr_banks = (chr.len() / 0x1000).max(1);
        Mmc1 {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_ram: [0; 0x2000],
            shift: SHIFT_RESET,
            // Power-on state: PRG mode 3 (fixed last bank at $C000, the mode the
            // reset vector relies on). Other bits default to 0.
            control: 0x0c,
            chr_bank0: 0,
            chr_bank1: 0,
            prg_bank: 0,
            num_prg_banks,
            num_chr_banks,
        }
    }

    fn prg_mode(&self) -> u8 {
        (self.control >> 2) & 3
    }

    fn chr_mode(&self) -> u8 {
        (self.control >> 4) & 1
    }

    // Byte offset into prg_rom for the $8000 half (upper=false) or $C000 half
    // (upper=true) of the window, per the current PRG bank mode.
    fn prg_base(&self, upper: bool) -> usize {
        let last = self.num_prg_banks - 1;
        let bank = match self.prg_mode() {
            // 0/1: switch a full 32 KB bank at $8000 (low bit of the reg ignored).
            0 | 1 => {
                let base = (self.prg_bank & !1) as usize;
                if upper { base + 1 } else { base }
            }
            // 2: fix the first bank at $8000, switch 16 KB at $C000.
            2 => {
                if upper { self.prg_bank as usize } else { 0 }
            }
            // 3: switch 16 KB at $8000, fix the last bank at $C000.
            3 => {
                if upper { last } else { self.prg_bank as usize }
            }
            _ => unreachable!(),
        };
        (bank % self.num_prg_banks) * 0x4000
    }

    // Byte offset into chr for the $0000 half (upper=false) or $1000 half
    // (upper=true), per the current CHR bank mode.
    fn chr_base(&self, upper: bool) -> usize {
        let bank = if self.chr_mode() == 0 {
            // 8 KB mode: one bank ignoring its low bit.
            let base = (self.chr_bank0 & !1) as usize;
            if upper { base + 1 } else { base }
        } else {
            // 4 KB mode: two independent banks.
            (if upper { self.chr_bank1 } else { self.chr_bank0 }) as usize
        };
        (bank % self.num_chr_banks) * 0x1000
    }

    // Copy a completed 5-bit value into the register selected by address.
    fn write_register(&mut self, addr: u16, value: u8) {
        match (addr >> 13) & 3 {
            0 => self.control = value & 0x1f,
            1 => self.chr_bank0 = value & 0x1f,
            2 => self.chr_bank1 = value & 0x1f,
            // Bit 4 is PRG-RAM enable, which we don't model; keep the bank bits.
            3 => self.prg_bank = value & 0x0f,
            _ => unreachable!(),
        }
    }
}

impl Mapper for Mmc1 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7fff => self.prg_ram[(addr - 0x6000) as usize],
            0x8000..=0xffff => {
                let upper = addr >= 0xc000;
                self.prg_rom[self.prg_base(upper) + (addr as usize & 0x3fff)]
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7fff => self.prg_ram[(addr - 0x6000) as usize] = data,
            0x8000..=0xffff => {
                if data & 0x80 != 0 {
                    // Reset: clear the shift register and force PRG mode 3.
                    self.shift = SHIFT_RESET;
                    self.control |= 0x0c;
                } else {
                    let complete = self.shift & 1 == 1;
                    self.shift = (self.shift >> 1) | ((data & 1) << 4);
                    if complete {
                        let value = self.shift & 0x1f;
                        self.write_register(addr, value);
                        self.shift = SHIFT_RESET;
                    }
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if self.chr_is_ram {
            return self.chr[addr as usize];
        }
        let upper = addr >= 0x1000;
        self.chr[self.chr_base(upper) + (addr as usize & 0x0fff)]
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_is_ram {
            self.chr[addr as usize] = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        match self.control & 3 {
            0 => Mirroring::SingleScreenLower,
            1 => Mirroring::SingleScreenUpper,
            2 => Mirroring::Vertical,
            3 => Mirroring::Horizontal,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // A ROM whose every 16 KB PRG bank is filled with its own index, and every
    // 4 KB CHR bank likewise, so a read identifies which bank is mapped.
    fn rom(prg_banks: usize, chr_4k_banks: usize) -> Rom {
        let mut prg_rom = Vec::new();
        for b in 0..prg_banks {
            prg_rom.extend(std::iter::repeat(b as u8).take(0x4000));
        }
        let mut chr_rom = Vec::new();
        for b in 0..chr_4k_banks {
            chr_rom.extend(std::iter::repeat(b as u8).take(0x1000));
        }
        Rom {
            prg_rom,
            chr_rom,
            mapper: 1,
            screen_mirroring: Mirroring::Horizontal,
        }
    }

    // Serially load a 5-bit value into the register selected by `addr`.
    fn load(m: &mut Mmc1, addr: u16, value: u8) {
        for i in 0..5 {
            m.cpu_write(addr, (value >> i) & 1);
        }
    }

    #[test]
    fn register_needs_five_writes() {
        let mut m = Mmc1::from_rom(rom(8, 2));
        // Four writes must not commit anything: control stays at its power-on
        // value (PRG mode 3), so the last bank is fixed at $C000.
        for _ in 0..4 {
            m.cpu_write(0xe000, 1);
        }
        assert_eq!(m.prg_bank, 0);
        // The fifth write commits the accumulated value.
        m.cpu_write(0xe000, 1);
        assert_eq!(m.prg_bank, 0b11111 & 0x0f);
    }

    #[test]
    fn prg_mode3_switches_low_fixes_last() {
        let mut m = Mmc1::from_rom(rom(8, 2)); // power-on PRG mode 3
        assert_eq!(m.cpu_read(0x8000), 0); // switchable low bank = 0
        assert_eq!(m.cpu_read(0xc000), 7); // fixed last bank
        load(&mut m, 0xe000, 3);
        assert_eq!(m.cpu_read(0x8000), 3);
        assert_eq!(m.cpu_read(0xffff), 7); // last bank unchanged
    }

    #[test]
    fn prg_mode2_fixes_first_switches_high() {
        let mut m = Mmc1::from_rom(rom(8, 2));
        load(&mut m, 0x8000, 0b0_1000); // control: mirroring 0, PRG mode 2
        load(&mut m, 0xe000, 5);
        assert_eq!(m.cpu_read(0x8000), 0); // fixed first bank
        assert_eq!(m.cpu_read(0xc000), 5); // switchable high bank
    }

    #[test]
    fn prg_mode0_switches_full_32k() {
        let mut m = Mmc1::from_rom(rom(8, 2));
        load(&mut m, 0x8000, 0b0_0000); // PRG mode 0 (32 KB)
        load(&mut m, 0xe000, 5); // low bit ignored -> banks 4 and 5
        assert_eq!(m.cpu_read(0x8000), 4);
        assert_eq!(m.cpu_read(0xc000), 5);
    }

    #[test]
    fn chr_4k_mode_two_banks() {
        let mut m = Mmc1::from_rom(rom(8, 4));
        load(&mut m, 0x8000, 0b1_0000); // CHR mode 1 (4 KB), mirroring/PRG 0
        load(&mut m, 0xa000, 2); // CHR bank 0 = 2
        load(&mut m, 0xc000, 3); // CHR bank 1 = 3
        assert_eq!(m.ppu_read(0x0000), 2);
        assert_eq!(m.ppu_read(0x1000), 3);
    }

    #[test]
    fn chr_8k_mode_ignores_low_bit() {
        let mut m = Mmc1::from_rom(rom(8, 4));
        // Power-on CHR mode is 0 (8 KB). Selecting bank 3 rounds down to the
        // 2/3 pair mapped consecutively.
        load(&mut m, 0xa000, 3);
        assert_eq!(m.ppu_read(0x0000), 2);
        assert_eq!(m.ppu_read(0x1000), 3);
    }

    #[test]
    fn mirroring_from_control() {
        let mut m = Mmc1::from_rom(rom(8, 2));
        load(&mut m, 0x8000, 2);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        load(&mut m, 0x8000, 3);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        load(&mut m, 0x8000, 0);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn reset_bit_restores_prg_mode3() {
        let mut m = Mmc1::from_rom(rom(8, 2));
        load(&mut m, 0x8000, 0b0_0000); // drop to PRG mode 0
        assert_eq!(m.prg_mode(), 0);
        m.cpu_write(0x8000, 0x80); // reset bit
        assert_eq!(m.prg_mode(), 3);
    }

    #[test]
    fn chr_ram_is_flat_and_writable() {
        let mut m = Mmc1::from_rom(rom(8, 0)); // no CHR -> 8 KB CHR-RAM
        assert!(m.chr_is_ram);
        m.ppu_write(0x0123, 0xab);
        assert_eq!(m.ppu_read(0x0123), 0xab);
    }
}
