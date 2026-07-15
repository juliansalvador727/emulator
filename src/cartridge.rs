const NES_TAG: [u8; 4] = [0x4E, 0x45, 0x53, 0x1A];
const PRG_ROM_PAGE_SIZE: usize = 16384;
const CHR_ROM_PAGE_SIZE: usize = 8192;

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Mirroring {
    Vertical,
    Horizontal,
    // Single-screen mirroring (AxROM and friends): all four nametables show the
    // same physical 1 KB page — the lower (first) or upper (second) VRAM bank.
    SingleScreenLower,
    SingleScreenUpper,
    FourScreen,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MemoryKind {
    Absent,
    Rom,
    VolatileRam,
    NonVolatileRam,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct MemoryRegion {
    pub kind: MemoryKind,
    pub size: usize,
}

impl MemoryRegion {
    pub const fn new(kind: MemoryKind, size: usize) -> Self {
        Self { kind, size }
    }

    pub fn is_ram(self) -> bool {
        matches!(self.kind, MemoryKind::VolatileRam | MemoryKind::NonVolatileRam)
    }

    pub fn is_nonvolatile(self) -> bool {
        self.kind == MemoryKind::NonVolatileRam
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct CartridgeMemory {
    pub prg_rom: MemoryRegion,
    pub prg_ram: MemoryRegion,
    pub chr: MemoryRegion,
}

impl CartridgeMemory {
    pub fn test_defaults(prg_rom_size: usize, chr_rom_size: usize) -> Self {
        Self {
            prg_rom: MemoryRegion::new(MemoryKind::Rom, prg_rom_size),
            prg_ram: MemoryRegion::new(MemoryKind::VolatileRam, 0x2000),
            chr: if chr_rom_size == 0 {
                MemoryRegion::new(MemoryKind::VolatileRam, 0x2000)
            } else {
                MemoryRegion::new(MemoryKind::Rom, chr_rom_size)
            },
        }
    }
}

#[derive(Clone)]
pub struct Rom {
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub mapper: u8,
    pub screen_mirroring: Mirroring,
    pub memory: CartridgeMemory,
    pub save_path: Option<PathBuf>,
}

impl Rom {
    pub fn new(raw: &Vec<u8>) -> Result<Rom, String> {
        if raw.len() < 16 {
            return Err("iNES image is shorter than its 16-byte header".to_string());
        }
        if &raw[0..4] != NES_TAG {
            return Err("bruh, not ines file format".to_string());
        }

        let mapper = raw[7] & 0b1111_0000 | raw[6] >> 4;

        let ines_ver = (raw[7] >> 2) & 0b11;
        if ines_ver != 0 {
            return Err("NES2.0 format is not supported".to_string());
        }
        let four_screen = raw[6] & 0b1000 != 0;
        let vertical_mirroring = raw[6] & 0b1 != 0;
        let screen_mirroring = match (four_screen, vertical_mirroring) {
            (true, _) => Mirroring::FourScreen,
            (false, true) => Mirroring::Vertical,
            (false, false) => Mirroring::Horizontal,
        };
        let prg_rom_size = raw[4] as usize * PRG_ROM_PAGE_SIZE;
        let chr_rom_size = raw[5] as usize * CHR_ROM_PAGE_SIZE;

        let skip_trainer = raw[6] & 0b100 != 0;

        let prg_rom_start = 16 + if skip_trainer { 512 } else { 0 };
        let chr_rom_start = prg_rom_start + prg_rom_size;

        if raw.len() < chr_rom_start + chr_rom_size {
            return Err("iNES image is shorter than the sizes declared in its header".to_string());
        }

        // iNES byte 8 is in 8 KiB units. Zero conventionally infers 8 KiB;
        // unofficial byte 10 bit 4 is the only iNES 1.0 way to say that PRG
        // RAM is absent. NES 2.0 can describe volatile and nonvolatile regions
        // independently, but is rejected above until the rest of its header is
        // supported.
        let prg_ram_absent = raw[8] == 0 && raw[10] & 0x10 != 0;
        let prg_ram_size = if prg_ram_absent {
            0
        } else if raw[8] == 0 {
            0x2000
        } else {
            raw[8] as usize * 0x2000
        };
        let battery = raw[6] & 0x02 != 0;
        let prg_ram_kind = if prg_ram_size == 0 {
            MemoryKind::Absent
        } else if battery {
            MemoryKind::NonVolatileRam
        } else {
            MemoryKind::VolatileRam
        };
        let chr_kind = if chr_rom_size == 0 {
            MemoryKind::VolatileRam
        } else {
            MemoryKind::Rom
        };

        Ok(Rom {
            prg_rom: raw[prg_rom_start..(prg_rom_start + prg_rom_size)].to_vec(),
            chr_rom: raw[chr_rom_start..(chr_rom_start + chr_rom_size)].to_vec(),
            mapper: mapper,
            screen_mirroring: screen_mirroring,
            memory: CartridgeMemory {
                prg_rom: MemoryRegion::new(MemoryKind::Rom, prg_rom_size),
                prg_ram: MemoryRegion::new(prg_ram_kind, prg_ram_size),
                chr: MemoryRegion::new(chr_kind, if chr_rom_size == 0 { 0x2000 } else { chr_rom_size }),
            },
            save_path: None,
        })
    }

    /// Parse a ROM from disk and attach its battery-save destination. Raw byte
    /// parsing deliberately has no filesystem side effects, which keeps probes
    /// and embedded test ROMs hermetic.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Rom, String> {
        let path = path.as_ref();
        let raw = std::fs::read(path)
            .map_err(|err| format!("failed to read ROM {}: {err}", path.display()))?;
        let mut rom = Rom::new(&raw)?;
        if rom.memory.prg_ram.is_nonvolatile() || rom.memory.chr.is_nonvolatile() {
            rom.save_path = Some(match std::env::var_os("NES_SAVE_DIR") {
                Some(dir) => {
                    let name = path
                        .file_stem()
                        .unwrap_or_else(|| std::ffi::OsStr::new("cartridge"));
                    PathBuf::from(dir).join(name).with_extension("sav")
                }
                None => path.with_extension("sav"),
            });
        }
        Ok(rom)
    }
}

pub mod test {

    use super::*;

    struct TestRom {
        header: Vec<u8>,
        trainer: Option<Vec<u8>>,
        pgp_rom: Vec<u8>,
        chr_rom: Vec<u8>,
    }

    fn create_rom(rom: TestRom) -> Vec<u8> {
        let mut result = Vec::with_capacity(
            rom.header.len()
                + rom.trainer.as_ref().map_or(0, |t| t.len())
                + rom.pgp_rom.len()
                + rom.chr_rom.len(),
        );

        result.extend(&rom.header);
        if let Some(t) = rom.trainer {
            result.extend(t);
        }
        result.extend(&rom.pgp_rom);
        result.extend(&rom.chr_rom);

        result
    }

    pub fn test_rom(program: Vec<u8>) -> Rom {
        let mut pgp_rom_contents = program;
        pgp_rom_contents.resize(2 * PRG_ROM_PAGE_SIZE, 0);

        // Byte 6 low nibble 0 => mapper 0 (NROM), the only mapper the bus/ppu
        // tests need. Bit 0 set keeps vertical mirroring. (The standalone
        // cartridge-parsing tests below use their own mapper-3 headers.)
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4E, 0x45, 0x53, 0x1A, 0x02, 0x01, 0x01, 00, 00, 00, 00, 00, 00, 00, 00, 00,
            ],
            trainer: None,
            pgp_rom: pgp_rom_contents,
            chr_rom: vec![2; 1 * CHR_ROM_PAGE_SIZE],
        });

        Rom::new(&test_rom).unwrap()
    }

    #[test]
    fn test() {
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4E, 0x45, 0x53, 0x1A, 0x02, 0x01, 0x31, 00, 00, 00, 00, 00, 00, 00, 00, 00,
            ],
            trainer: None,
            pgp_rom: vec![1; 2 * PRG_ROM_PAGE_SIZE],
            chr_rom: vec![2; 1 * CHR_ROM_PAGE_SIZE],
        });

        let rom: Rom = Rom::new(&test_rom).unwrap();

        assert_eq!(rom.chr_rom, vec!(2; 1 * CHR_ROM_PAGE_SIZE));
        assert_eq!(rom.prg_rom, vec!(1; 2 * PRG_ROM_PAGE_SIZE));
        assert_eq!(rom.mapper, 3);
        assert_eq!(rom.screen_mirroring, Mirroring::Vertical);
    }

    #[test]
    fn test_with_trainer() {
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4E,
                0x45,
                0x53,
                0x1A,
                0x02,
                0x01,
                0x31 | 0b100,
                00,
                00,
                00,
                00,
                00,
                00,
                00,
                00,
                00,
            ],
            trainer: Some(vec![0; 512]),
            pgp_rom: vec![1; 2 * PRG_ROM_PAGE_SIZE],
            chr_rom: vec![2; 1 * CHR_ROM_PAGE_SIZE],
        });

        let rom: Rom = Rom::new(&test_rom).unwrap();

        assert_eq!(rom.chr_rom, vec!(2; 1 * CHR_ROM_PAGE_SIZE));
        assert_eq!(rom.prg_rom, vec!(1; 2 * PRG_ROM_PAGE_SIZE));
        assert_eq!(rom.mapper, 3);
        assert_eq!(rom.screen_mirroring, Mirroring::Vertical);
    }

    #[test]
    fn test_nes2_is_not_supported() {
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4E, 0x45, 0x53, 0x1A, 0x01, 0x01, 0x31, 0x8, 00, 00, 00, 00, 00, 00, 00, 00,
            ],
            trainer: None,
            pgp_rom: vec![1; 1 * PRG_ROM_PAGE_SIZE],
            chr_rom: vec![2; 1 * CHR_ROM_PAGE_SIZE],
        });
        let rom = Rom::new(&test_rom);
        match rom {
            Result::Ok(_) => assert!(false, "should not load rom"),
            Result::Err(str) => assert_eq!(str, "NES2.0 format is not supported"),
        }
    }

    #[test]
    fn parses_rom_volatile_and_battery_memory_types() {
        let mut raw = create_rom(TestRom {
            header: vec![
                0x4e, 0x45, 0x53, 0x1a, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            trainer: None,
            pgp_rom: vec![0; PRG_ROM_PAGE_SIZE],
            chr_rom: vec![],
        });
        let volatile = Rom::new(&raw).unwrap();
        assert_eq!(volatile.memory.prg_rom, MemoryRegion::new(MemoryKind::Rom, 0x4000));
        assert_eq!(volatile.memory.prg_ram, MemoryRegion::new(MemoryKind::VolatileRam, 0x2000));
        assert_eq!(volatile.memory.chr, MemoryRegion::new(MemoryKind::VolatileRam, 0x2000));

        raw[6] |= 0x02;
        raw[8] = 4;
        let battery = Rom::new(&raw).unwrap();
        assert_eq!(battery.memory.prg_ram, MemoryRegion::new(MemoryKind::NonVolatileRam, 0x8000));
    }

    #[test]
    fn unofficial_ines_absent_prg_ram_flag_is_not_an_implicit_8k() {
        let raw = create_rom(TestRom {
            header: vec![
                0x4e, 0x45, 0x53, 0x1a, 0x01, 0x01, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0,
            ],
            trainer: None,
            pgp_rom: vec![0; PRG_ROM_PAGE_SIZE],
            chr_rom: vec![0; CHR_ROM_PAGE_SIZE],
        });
        let rom = Rom::new(&raw).unwrap();
        assert_eq!(rom.memory.prg_ram, MemoryRegion::new(MemoryKind::Absent, 0));
        assert_eq!(rom.memory.chr, MemoryRegion::new(MemoryKind::Rom, 0x2000));
    }
}
