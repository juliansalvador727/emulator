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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RomFormat {
    INes,
    Nes2,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct CartridgeMetadata {
    pub format: RomFormat,
    pub submapper: u8,
    pub prg_ram_size: usize,
    pub prg_nvram_size: usize,
    pub chr_ram_size: usize,
    pub chr_nvram_size: usize,
}

impl CartridgeMetadata {
    pub const fn test_defaults() -> Self {
        Self {
            format: RomFormat::INes,
            submapper: 0,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            chr_ram_size: 0,
            chr_nvram_size: 0,
        }
    }
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
        matches!(
            self.kind,
            MemoryKind::VolatileRam | MemoryKind::NonVolatileRam
        )
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

#[derive(Debug, Clone)]
pub struct Rom {
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub mapper: u16,
    pub metadata: CartridgeMetadata,
    pub screen_mirroring: Mirroring,
    pub memory: CartridgeMemory,
    pub save_path: Option<PathBuf>,
}

impl Rom {
    pub fn new(raw: &[u8]) -> Result<Rom, String> {
        if raw.len() < 16 {
            return Err("iNES image is shorter than its 16-byte header".to_string());
        }
        if &raw[0..4] != NES_TAG {
            return Err("image does not have an iNES/NES 2.0 header".to_string());
        }

        let format = match raw[7] & 0x0c {
            0x00 => RomFormat::INes,
            0x08 => RomFormat::Nes2,
            bits => return Err(format!("unsupported iNES format marker ${bits:02X}")),
        };
        let mapper = u16::from(raw[7] & 0xf0)
            | u16::from(raw[6] >> 4)
            | if format == RomFormat::Nes2 {
                u16::from(raw[8] & 0x0f) << 8
            } else {
                0
            };
        let submapper = if format == RomFormat::Nes2 {
            raw[8] >> 4
        } else {
            0
        };
        let four_screen = raw[6] & 0b1000 != 0;
        let vertical_mirroring = raw[6] & 0b1 != 0;
        let screen_mirroring = match (four_screen, vertical_mirroring) {
            (true, _) => Mirroring::FourScreen,
            (false, true) => Mirroring::Vertical,
            (false, false) => Mirroring::Horizontal,
        };
        let (prg_rom_size, chr_rom_size) = if format == RomFormat::Nes2 {
            (
                decode_nes2_rom_size(raw[4], raw[9] & 0x0f, PRG_ROM_PAGE_SIZE, "PRG ROM")?,
                decode_nes2_rom_size(raw[5], raw[9] >> 4, CHR_ROM_PAGE_SIZE, "CHR ROM")?,
            )
        } else {
            (
                raw[4] as usize * PRG_ROM_PAGE_SIZE,
                raw[5] as usize * CHR_ROM_PAGE_SIZE,
            )
        };

        let skip_trainer = raw[6] & 0b100 != 0;

        let prg_rom_start = 16usize
            .checked_add(if skip_trainer { 512 } else { 0 })
            .ok_or_else(|| "cartridge offset overflow".to_string())?;
        let chr_rom_start = prg_rom_start
            .checked_add(prg_rom_size)
            .ok_or_else(|| "PRG ROM size overflows the host address space".to_string())?;
        let image_end = chr_rom_start
            .checked_add(chr_rom_size)
            .ok_or_else(|| "CHR ROM size overflows the host address space".to_string())?;

        if raw.len() < image_end {
            return Err(format!(
                "cartridge image has {} bytes but its header requires at least {image_end}",
                raw.len()
            ));
        }

        // iNES byte 8 is in 8 KiB units. Zero conventionally infers 8 KiB;
        // unofficial byte 10 bit 4 is the only iNES 1.0 way to say that PRG
        // RAM is absent. NES 2.0 instead describes volatile and nonvolatile
        // regions independently in bytes 10 and 11.
        let battery = raw[6] & 0x02 != 0;
        let (prg_ram_size, prg_nvram_size, chr_ram_size, chr_nvram_size) =
            if format == RomFormat::Nes2 {
                (
                    decode_nes2_ram_size(raw[10] & 0x0f, "PRG RAM")?,
                    decode_nes2_ram_size(raw[10] >> 4, "PRG NVRAM")?,
                    decode_nes2_ram_size(raw[11] & 0x0f, "CHR RAM")?,
                    decode_nes2_ram_size(raw[11] >> 4, "CHR NVRAM")?,
                )
            } else {
                let absent = raw[8] == 0 && raw[10] & 0x10 != 0;
                let size = if absent {
                    0
                } else if raw[8] == 0 {
                    0x2000
                } else {
                    raw[8] as usize * 0x2000
                };
                let chr_size = if chr_rom_size == 0 { 0x2000 } else { 0 };
                if battery {
                    // Preserve iNES behavior: the battery flag describes the
                    // mapper's PRG save RAM; a zero CHR count still implies
                    // ordinary volatile CHR RAM.
                    (0, size, chr_size, 0)
                } else {
                    (size, 0, chr_size, 0)
                }
            };
        let total_prg_ram = prg_ram_size
            .checked_add(prg_nvram_size)
            .ok_or_else(|| "combined PRG RAM size overflows the host address space".to_string())?;
        let total_chr_ram = chr_ram_size
            .checked_add(chr_nvram_size)
            .ok_or_else(|| "combined CHR RAM size overflows the host address space".to_string())?;
        let prg_ram_kind = if total_prg_ram == 0 {
            MemoryKind::Absent
        } else if prg_nvram_size != 0 {
            MemoryKind::NonVolatileRam
        } else {
            MemoryKind::VolatileRam
        };
        let chr_kind = if chr_rom_size != 0 {
            MemoryKind::Rom
        } else if chr_nvram_size != 0 {
            MemoryKind::NonVolatileRam
        } else if total_chr_ram != 0 {
            MemoryKind::VolatileRam
        } else {
            MemoryKind::Absent
        };

        Ok(Rom {
            prg_rom: raw[prg_rom_start..(prg_rom_start + prg_rom_size)].to_vec(),
            chr_rom: raw[chr_rom_start..(chr_rom_start + chr_rom_size)].to_vec(),
            mapper: mapper,
            metadata: CartridgeMetadata {
                format,
                submapper,
                prg_ram_size,
                prg_nvram_size,
                chr_ram_size,
                chr_nvram_size,
            },
            screen_mirroring: screen_mirroring,
            memory: CartridgeMemory {
                prg_rom: MemoryRegion::new(MemoryKind::Rom, prg_rom_size),
                prg_ram: MemoryRegion::new(prg_ram_kind, total_prg_ram),
                chr: MemoryRegion::new(
                    chr_kind,
                    if chr_rom_size == 0 {
                        total_chr_ram
                    } else {
                        chr_rom_size
                    },
                ),
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

fn decode_nes2_rom_size(
    lsb: u8,
    msb: u8,
    linear_unit: usize,
    label: &str,
) -> Result<usize, String> {
    if msb != 0x0f {
        let pages = (usize::from(msb) << 8) | usize::from(lsb);
        return pages
            .checked_mul(linear_unit)
            .ok_or_else(|| format!("{label} size overflows the host address space"));
    }

    let exponent = u32::from(lsb >> 2);
    let multiplier = usize::from((lsb & 0x03) * 2 + 1);
    1usize
        .checked_shl(exponent)
        .and_then(|base| base.checked_mul(multiplier))
        .ok_or_else(|| format!("{label} exponent/multiplier size is too large"))
}

fn decode_nes2_ram_size(shift: u8, label: &str) -> Result<usize, String> {
    if shift == 0 {
        return Ok(0);
    }
    64usize
        .checked_shl(u32::from(shift))
        .ok_or_else(|| format!("{label} shift size is too large"))
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
    fn parses_nes2_extended_mapper_submapper_and_linear_sizes() {
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4e, 0x45, 0x53, 0x1a, 0x01, // 16 KiB PRG ROM
                0x01, // 8 KiB CHR ROM
                0xc1, // mapper low nibble C, vertical mirroring
                0xb8, // mapper middle nibble B, NES 2.0 marker
                0xda, // submapper D, mapper high nibble A
                0x00, // linear PRG/CHR size MSBs
                0x87, // 8 KiB PRG RAM, 16 KiB PRG NVRAM
                0x56, // 4 KiB CHR RAM, 2 KiB CHR NVRAM
                0x00, 0x00, 0x00, 0x00,
            ],
            trainer: None,
            pgp_rom: vec![1; PRG_ROM_PAGE_SIZE],
            chr_rom: vec![2; CHR_ROM_PAGE_SIZE],
        });
        let rom = Rom::new(&test_rom).unwrap();
        assert_eq!(rom.mapper, 0x0abc);
        assert_eq!(rom.metadata.format, RomFormat::Nes2);
        assert_eq!(rom.metadata.submapper, 0x0d);
        assert_eq!(rom.metadata.prg_ram_size, 0x2000);
        assert_eq!(rom.metadata.prg_nvram_size, 0x4000);
        assert_eq!(rom.metadata.chr_ram_size, 0x1000);
        assert_eq!(rom.metadata.chr_nvram_size, 0x0800);
        assert_eq!(rom.memory.prg_rom.size, PRG_ROM_PAGE_SIZE);
        assert_eq!(rom.memory.prg_ram.size, 0x6000);
        assert_eq!(rom.memory.chr.size, CHR_ROM_PAGE_SIZE);
        assert_eq!(rom.screen_mirroring, Mirroring::Vertical);
    }

    #[test]
    fn parses_nes2_exponent_multiplier_rom_sizes() {
        assert_eq!(
            decode_nes2_rom_size(2, 1, PRG_ROM_PAGE_SIZE, "PRG ROM").unwrap(),
            258 * PRG_ROM_PAGE_SIZE
        );
        // PRG: 2^12 * 3 = 12 KiB. CHR: 2^11 * 5 = 10 KiB.
        let test_rom = create_rom(TestRom {
            header: vec![
                0x4e, 0x45, 0x53, 0x1a, 0x31, 0x2e, 0, 0x08, 0, 0xff, 0, 0, 0, 0, 0, 0,
            ],
            trainer: None,
            pgp_rom: vec![1; 12 * 1024],
            chr_rom: vec![2; 10 * 1024],
        });
        let rom = Rom::new(&test_rom).unwrap();
        assert_eq!(rom.prg_rom.len(), 12 * 1024);
        assert_eq!(rom.chr_rom.len(), 10 * 1024);
    }

    #[test]
    fn nes2_zero_rom_and_ram_shifts_mean_absent_memory() {
        let raw = create_rom(TestRom {
            header: vec![
                0x4e, 0x45, 0x53, 0x1a, 1, 0, 0, 0x08, 0, 0, 0, 0x70, 0, 0, 0, 0,
            ],
            trainer: None,
            pgp_rom: vec![0; PRG_ROM_PAGE_SIZE],
            chr_rom: vec![],
        });
        let rom = Rom::new(&raw).unwrap();
        assert_eq!(rom.metadata.prg_ram_size, 0);
        assert_eq!(rom.metadata.prg_nvram_size, 0);
        assert_eq!(rom.metadata.chr_ram_size, 0);
        assert_eq!(rom.metadata.chr_nvram_size, 0x2000);
        assert_eq!(rom.memory.prg_ram, MemoryRegion::new(MemoryKind::Absent, 0));
        assert_eq!(
            rom.memory.chr,
            MemoryRegion::new(MemoryKind::NonVolatileRam, 0x2000)
        );
    }

    #[test]
    fn rejects_invalid_markers_truncated_payloads_and_size_overflow() {
        assert!(Rom::new(&[0; 15]).unwrap_err().contains("16-byte header"));
        assert!(Rom::new(&[0; 16]).unwrap_err().contains("iNES/NES 2.0"));

        let mut header = [0u8; 16];
        header[..4].copy_from_slice(&NES_TAG);
        header[7] = 0x04;
        assert!(Rom::new(&header).unwrap_err().contains("format marker"));

        header[7] = 0;
        header[4] = 1;
        assert!(Rom::new(&header).unwrap_err().contains("header requires"));

        header[4] = 0;
        header[6] = 0x04;
        assert!(Rom::new(&header)
            .unwrap_err()
            .contains("requires at least 528"));

        header[7] = 0x08;
        header[6] = 0;
        header[4] = 0xff; // exponent 63, multiplier 7
        header[9] = 0x0f;
        assert!(Rom::new(&header).unwrap_err().contains("too large"));
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
        assert_eq!(volatile.metadata.format, RomFormat::INes);
        assert_eq!(volatile.metadata.submapper, 0);
        assert_eq!(volatile.metadata.prg_ram_size, 0x2000);
        assert_eq!(volatile.metadata.prg_nvram_size, 0);
        assert_eq!(
            volatile.memory.prg_rom,
            MemoryRegion::new(MemoryKind::Rom, 0x4000)
        );
        assert_eq!(
            volatile.memory.prg_ram,
            MemoryRegion::new(MemoryKind::VolatileRam, 0x2000)
        );
        assert_eq!(
            volatile.memory.chr,
            MemoryRegion::new(MemoryKind::VolatileRam, 0x2000)
        );

        raw[6] |= 0x02;
        raw[8] = 4;
        let battery = Rom::new(&raw).unwrap();
        assert_eq!(battery.metadata.prg_ram_size, 0);
        assert_eq!(battery.metadata.prg_nvram_size, 0x8000);
        assert_eq!(battery.metadata.chr_ram_size, 0x2000);
        assert_eq!(battery.metadata.chr_nvram_size, 0);
        assert_eq!(
            battery.memory.prg_ram,
            MemoryRegion::new(MemoryKind::NonVolatileRam, 0x8000)
        );
        assert_eq!(
            battery.memory.chr,
            MemoryRegion::new(MemoryKind::VolatileRam, 0x2000)
        );
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
