use super::Mapper;
use crate::cartridge::{Mirroring, Rom};

// Mapper 4 (MMC3 / TxROM). The workhorse of the mid/late NES library (SMB3,
// Kirby's Adventure, Mega Man 3-6, ...). Two things set it apart from the
// simpler mappers:
//
//   1. Fine-grained banking. PRG is switched in 8 KB windows and CHR in 1 KB
//      windows, selected through a two-step protocol: a write to $8000 (even)
//      picks which of eight bank registers R0-R7 the *next* write to $8001
//      (odd) will load, along with the PRG mode and CHR-inversion bits.
//   2. A scanline IRQ. A counter reloads from a latch and decrements once per
//      qualified PPU A12 rising edge; when it
//      hits zero with IRQs enabled it asserts the CPU IRQ line. Games use it to
//      split the screen — a fixed status bar over a scrolling playfield — which
//      is exactly the effect the whole-frame renderer could never produce.
//
// Register map (address decoded as `addr & 0xE001`):
//   $8000 even  bank select   (low 3 bits R0-R7; bit6 PRG mode; bit7 CHR inv)
//   $8001 odd   bank data      (value loaded into the selected register)
//   $A000 even  mirroring      (bit0: 0=vertical, 1=horizontal)
//   $A001 odd   PRG-RAM protect (not modelled)
//   $C000 even  IRQ latch       (reload value for the counter)
//   $C001 odd   IRQ reload      (force a reload on the next clock)
//   $E000 even  IRQ disable + acknowledge (clears the pending line)
//   $E001 odd   IRQ enable
pub struct Mmc3 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_is_ram: bool,
    prg_ram: [u8; 0x2000],

    // R0-R7. R0/R1 are 2 KB CHR banks (their low bit is ignored); R2-R5 are
    // 1 KB CHR banks; R6/R7 are 8 KB PRG banks.
    regs: [u8; 8],
    bank_select: usize, // which register the next $8001 write targets (0-7)
    prg_mode: bool,     // false: $8000 swappable; true: $C000 swappable
    chr_inversion: bool,

    mirroring: Mirroring,
    four_screen: bool, // $A000 mirroring writes are ignored on four-screen carts

    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,  // force reload from latch on the next qualified A12 edge
    irq_enabled: bool,
    irq_line: bool, // asserted; held until acknowledged by a write to $E000
    a12_high: bool,
    a12_low_since: Option<u64>,

    num_prg_banks: usize, // in 8 KB units
    num_chr_banks: usize, // in 1 KB units
}

impl Mmc3 {
    pub fn from_rom(rom: Rom) -> Self {
        let chr_is_ram = rom.chr_rom.is_empty();
        let chr = if chr_is_ram { vec![0; 0x2000] } else { rom.chr_rom };
        let num_prg_banks = (rom.prg_rom.len() / 0x2000).max(1);
        let num_chr_banks = (chr.len() / 0x400).max(1);
        let four_screen = rom.screen_mirroring == Mirroring::FourScreen;
        Mmc3 {
            prg_rom: rom.prg_rom,
            chr,
            chr_is_ram,
            prg_ram: [0; 0x2000],
            regs: [0; 8],
            bank_select: 0,
            prg_mode: false,
            chr_inversion: false,
            mirroring: rom.screen_mirroring,
            four_screen,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq_line: false,
            a12_high: false,
            a12_low_since: None,
            num_prg_banks,
            num_chr_banks,
        }
    }

    // Byte offset into prg_rom for the 8 KB window containing `addr`.
    //   $8000/$C000: one is R6, the other the fixed second-last bank, swapped
    //                by prg_mode.
    //   $A000:       always R7.
    //   $E000:       always the fixed last bank.
    fn prg_offset(&self, addr: u16) -> usize {
        let last = self.num_prg_banks - 1;
        let second_last = last.saturating_sub(1);
        let bank = match (addr - 0x8000) / 0x2000 {
            0 => if self.prg_mode { second_last } else { self.regs[6] as usize },
            1 => self.regs[7] as usize,
            2 => if self.prg_mode { self.regs[6] as usize } else { second_last },
            3 => last,
            _ => unreachable!(),
        };
        (bank % self.num_prg_banks) * 0x2000 + (addr as usize & 0x1fff)
    }

    // Byte offset into chr for the 1 KB window containing `addr` ($0000-$1FFF).
    // The eight 1 KB windows map to R0-R5 as two 2 KB banks (R0, R1) followed by
    // four 1 KB banks (R2-R5); CHR inversion swaps the two halves ($0000<->$1000).
    fn chr_offset(&self, addr: u16) -> usize {
        let mut window = (addr / 0x400) as usize; // 0-7
        if self.chr_inversion {
            window = (window + 4) % 8;
        }
        let bank = match window {
            0 => (self.regs[0] & 0xfe) as usize,
            1 => (self.regs[0] | 0x01) as usize,
            2 => (self.regs[1] & 0xfe) as usize,
            3 => (self.regs[1] | 0x01) as usize,
            4 => self.regs[2] as usize,
            5 => self.regs[3] as usize,
            6 => self.regs[4] as usize,
            7 => self.regs[5] as usize,
            _ => unreachable!(),
        };
        (bank % self.num_chr_banks) * 0x400 + (addr as usize & 0x3ff)
    }
}

impl Mapper for Mmc3 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7fff => self.prg_ram[(addr - 0x6000) as usize],
            0x8000..=0xffff => self.prg_rom[self.prg_offset(addr)],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7fff => self.prg_ram[(addr - 0x6000) as usize] = data,
            0x8000..=0xffff => match addr & 0xe001 {
                0x8000 => {
                    self.bank_select = (data & 0x07) as usize;
                    self.prg_mode = data & 0x40 != 0;
                    self.chr_inversion = data & 0x80 != 0;
                }
                0x8001 => self.regs[self.bank_select] = data,
                0xa000 => {
                    if !self.four_screen {
                        self.mirroring = if data & 1 != 0 {
                            Mirroring::Horizontal
                        } else {
                            Mirroring::Vertical
                        };
                    }
                }
                0xa001 => {} // PRG-RAM protect: not modelled
                0xc000 => self.irq_latch = data,
                0xc001 => {
                    self.irq_counter = 0;
                    self.irq_reload = true;
                }
                0xe000 => {
                    // Disable and acknowledge: drop the pending line.
                    self.irq_enabled = false;
                    self.irq_line = false;
                }
                0xe001 => self.irq_enabled = true,
                _ => unreachable!(),
            },
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if self.chr_is_ram {
            return self.chr[addr as usize];
        }
        self.chr[self.chr_offset(addr)]
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_is_ram {
            self.chr[addr as usize] = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_ppu_bus_access(&mut self, addr: u16, ppu_cycle: u64) {
        let high = addr & 0x1000 != 0;
        if !high {
            if self.a12_high || self.a12_low_since.is_none() {
                self.a12_low_since = Some(ppu_cycle);
            }
            self.a12_high = false;
            return;
        }

        if !self.a12_high
            && self
                .a12_low_since
                .is_some_and(|start| ppu_cycle.saturating_sub(start) >= 8)
        {
            self.clock_irq_counter();
        }
        self.a12_high = true;
    }

    fn irq_pending(&self) -> bool {
        self.irq_line
    }
}

impl Mmc3 {
    // Clocked on a qualified PPU A12 edge. The counter reloads from the latch
    // on a forced reload or when it has run down to zero, otherwise decrements.
    fn clock_irq_counter(&mut self) {
        if self.irq_reload || self.irq_counter == 0 {
            self.irq_counter = self.irq_latch;
            self.irq_reload = false;
        } else {
            self.irq_counter -= 1;
        }

        if self.irq_counter == 0 && self.irq_enabled {
            self.irq_line = true;
        }
    }

}

#[cfg(test)]
mod test {
    use super::*;

    // A ROM whose every 8 KB PRG bank is filled with its own index, and every
    // 1 KB CHR bank likewise, so a read identifies the mapped bank.
    fn rom(prg_8k_banks: usize, chr_1k_banks: usize) -> Rom {
        let mut prg_rom = Vec::new();
        for b in 0..prg_8k_banks {
            prg_rom.extend(std::iter::repeat(b as u8).take(0x2000));
        }
        let mut chr_rom = Vec::new();
        for b in 0..chr_1k_banks {
            chr_rom.extend(std::iter::repeat(b as u8).take(0x400));
        }
        Rom {
            prg_rom,
            chr_rom,
            mapper: 4,
            screen_mirroring: Mirroring::Vertical,
        }
    }

    // Select register `reg` then load `value` into it (the $8000/$8001 pair).
    fn set_bank(m: &mut Mmc3, reg: u8, value: u8) {
        m.cpu_write(0x8000, reg); // prg_mode/chr_inv clear (reg < 8)
        m.cpu_write(0x8001, value);
    }

    #[test]
    fn prg_mode0_fixes_second_last_at_c000_last_at_e000() {
        let mut m = Mmc3::from_rom(rom(8, 8)); // banks 0..7, last=7
        set_bank(&mut m, 6, 3); // R6 -> $8000
        set_bank(&mut m, 7, 5); // R7 -> $A000
        assert_eq!(m.cpu_read(0x8000), 3); // R6
        assert_eq!(m.cpu_read(0xa000), 5); // R7
        assert_eq!(m.cpu_read(0xc000), 6); // second-last fixed
        assert_eq!(m.cpu_read(0xe000), 7); // last fixed
    }

    #[test]
    fn prg_mode1_swaps_8000_and_c000() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0x8000, 0x40 | 6); // PRG mode 1, select R6
        m.cpu_write(0x8001, 3); // R6 = 3
        assert_eq!(m.cpu_read(0x8000), 6); // second-last now fixed at $8000
        assert_eq!(m.cpu_read(0xc000), 3); // R6 now swappable at $C000
        assert_eq!(m.cpu_read(0xe000), 7); // last still fixed
    }

    #[test]
    fn chr_2k_and_1k_banks_no_inversion() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        set_bank(&mut m, 0, 0); // R0 (2 KB) at $0000-$07FF -> banks 0,1
        set_bank(&mut m, 1, 2); // R1 (2 KB) at $0800-$0FFF -> banks 2,3
        set_bank(&mut m, 2, 4); // R2 (1 KB) at $1000
        set_bank(&mut m, 3, 5); // R3 (1 KB) at $1400
        set_bank(&mut m, 4, 6); // R4 (1 KB) at $1800
        set_bank(&mut m, 5, 7); // R5 (1 KB) at $1C00
        assert_eq!(m.ppu_read(0x0000), 0);
        assert_eq!(m.ppu_read(0x0400), 1); // R0 high 1 KB
        assert_eq!(m.ppu_read(0x0800), 2);
        assert_eq!(m.ppu_read(0x0c00), 3);
        assert_eq!(m.ppu_read(0x1000), 4);
        assert_eq!(m.ppu_read(0x1400), 5);
        assert_eq!(m.ppu_read(0x1800), 6);
        assert_eq!(m.ppu_read(0x1c00), 7);
    }

    #[test]
    fn chr_inversion_swaps_halves() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        set_bank(&mut m, 0, 0); // R0 pair
        set_bank(&mut m, 2, 4); // R2
        // Turn on CHR inversion (bit 7); the 1 KB banks move to $0000.
        m.cpu_write(0x8000, 0x80);
        assert_eq!(m.ppu_read(0x0000), 4); // R2 now at $0000
        assert_eq!(m.ppu_read(0x1000), 0); // R0 pair now at $1000
        assert_eq!(m.ppu_read(0x1400), 1);
    }

    #[test]
    fn mirroring_selected_by_a000() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0xa000, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xa000, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn irq_fires_after_latch_plus_one_scanlines() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0xc000, 2); // latch = 2
        m.cpu_write(0xc001, 0); // force reload
        m.cpu_write(0xe001, 0); // enable IRQ
        // First clock reloads to 2 (counter 0 or reload set); then decrements.
        m.clock_irq_counter(); // reload -> 2
        assert!(!m.irq_pending());
        m.clock_irq_counter(); // 2 -> 1
        assert!(!m.irq_pending());
        m.clock_irq_counter(); // 1 -> 0 -> assert
        assert!(m.irq_pending());
    }

    #[test]
    fn irq_line_cleared_by_e000_write() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0xc000, 0); // latch 0: fires as soon as it reloads
        m.cpu_write(0xc001, 0);
        m.cpu_write(0xe001, 0);
        m.clock_irq_counter(); // reload to 0 -> counter 0 & enabled -> assert
        assert!(m.irq_pending());
        m.cpu_write(0xe000, 0); // disable + acknowledge
        assert!(!m.irq_pending());
    }

    #[test]
    fn irq_does_not_fire_while_disabled() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0xc000, 0);
        m.cpu_write(0xc001, 0);
        // IRQ never enabled.
        for _ in 0..4 {
            m.clock_irq_counter();
        }
        assert!(!m.irq_pending());
    }

    #[test]
    fn a12_rise_requires_eight_ppu_cycles_low() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0xc000, 0);
        m.cpu_write(0xc001, 0);
        m.cpu_write(0xe001, 0);

        m.on_ppu_bus_access(0x0000, 10);
        m.on_ppu_bus_access(0x1000, 17);
        assert!(!m.irq_pending());
        m.on_ppu_bus_access(0x0000, 20);
        m.on_ppu_bus_access(0x1000, 28);
        assert!(m.irq_pending());
    }

    #[test]
    fn prg_ram_read_write() {
        let mut m = Mmc3::from_rom(rom(8, 8));
        m.cpu_write(0x6001, 0xab);
        assert_eq!(m.cpu_read(0x6001), 0xab);
    }

    #[test]
    fn chr_ram_is_flat_and_writable() {
        let mut m = Mmc3::from_rom(rom(8, 0)); // no CHR -> 8 KB CHR-RAM
        assert!(m.chr_is_ram);
        m.ppu_write(0x0123, 0xcd);
        assert_eq!(m.ppu_read(0x0123), 0xcd);
    }

    #[test]
    fn bank_numbers_wrap_to_available_banks() {
        // Only 4 PRG banks: selecting a high R6 must modulo into range.
        let mut m = Mmc3::from_rom(rom(4, 8));
        set_bank(&mut m, 6, 10); // 10 % 4 == 2
        assert_eq!(m.cpu_read(0x8000), 2);
    }
}
