use std::cell::RefCell;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use crate::cartridge::{CartridgeMemory, Mirroring, Rom};

pub mod axrom;
pub mod cnrom;
pub mod gnrom;
pub mod mmc1;
pub mod mmc2;
pub mod mmc3;
pub mod nrom;
pub mod uxrom;

use axrom::Axrom;
use cnrom::Cnrom;
use gnrom::Gnrom;
use mmc1::Mmc1;
use mmc2::Mmc2;
use mmc3::Mmc3;
use nrom::Nrom;
use uxrom::Uxrom;

// A cartridge mapper. It owns PRG ROM, CHR ROM/RAM, PRG RAM, any bank
// registers, and the mirroring state, and is visible from both sides of the
// machine at once: the CPU bus drives PRG space ($6000-$FFFF) and the PPU +
// dot pipeline drives CHR space ($0000-$1FFF) plus nametable mirroring. Writes to
// PRG ROM space are how bank switches arrive, so they land in `cpu_write`
// rather than being errors.
pub trait Mapper: MapperClone {
    fn cpu_read(&mut self, addr: u16) -> u8; // $6000-$FFFF: PRG-RAM + PRG-ROM
    fn cpu_write(&mut self, addr: u16, data: u8); // bank-register writes land here
    fn cpu_write_at(&mut self, addr: u16, data: u8, _cpu_cycle: u64) {
        self.cpu_write(addr, data);
    }
    fn ppu_read(&mut self, addr: u16) -> u8; // $0000-$1FFF CHR
    fn ppu_write(&mut self, addr: u16, data: u8); // CHR-RAM
    fn mirroring(&self) -> Mirroring; // MMC1 etc. set this at runtime
    // Observe the address driven by a real PPU memory access. `ppu_cycle` is a
    // monotonically increasing PPU-dot timestamp, allowing edge-sensitive
    // mappers to distinguish a qualified A12 low period from a short pulse.
    fn on_ppu_bus_access(&mut self, _addr: u16, _ppu_cycle: u64) {}
    // Mapper IRQ line (MMC3). Level-triggered like the APU's: stays asserted
    // until the program acknowledges it (MMC3: a write to $E000). Default off
    // for mappers with no IRQ source.
    fn irq_pending(&self) -> bool {
        false
    }
    fn reset(&mut self) {}
    fn prg_ram(&self) -> Option<&[u8]> {
        None
    }
    fn prg_ram_mut(&mut self) -> Option<&mut [u8]> {
        None
    }
    fn chr_ram(&self) -> Option<&[u8]> {
        None
    }
    fn chr_ram_mut(&mut self) -> Option<&mut [u8]> {
        None
    }
    fn flush_persistent_ram(&self) -> Result<(), String> {
        Ok(())
    }
}

pub trait MapperClone {
    fn clone_box(&self) -> Box<dyn Mapper>;
}

impl<T> MapperClone for T
where
    T: Mapper + Clone + 'static,
{
    fn clone_box(&self) -> Box<dyn Mapper> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn Mapper> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// Shared between Bus and NesPPU so both see the same mapper state. Cloned
// (the Rc) into each; the RefCell gives interior mutability since the PPU
// accesses it throughout CPU and PPU bus activity.
pub type SharedMapper = Rc<RefCell<Box<dyn Mapper>>>;

#[derive(Clone)]
struct ManagedMapper {
    inner: Box<dyn Mapper>,
    memory: CartridgeMemory,
    save_path: Option<PathBuf>,
}

impl ManagedMapper {
    fn load(&mut self) -> Result<(), String> {
        let Some(path) = self.save_path.as_ref() else {
            return Ok(());
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(format!("failed to read save {}: {err}", path.display())),
        };
        let expected = self.memory.prg_nvram.size + self.memory.chr_nvram.size;
        if bytes.len() != expected {
            return Err(format!(
                "save {} has {} bytes; cartridge expects {expected}",
                path.display(),
                bytes.len()
            ));
        }
        let mut offset = 0;
        if self.memory.prg_nvram.size != 0 {
            let ram = self
                .inner
                .prg_ram_mut()
                .ok_or_else(|| "mapper exposes no PRG RAM".to_string())?;
            let start = self.memory.prg_ram.size;
            let end = start + self.memory.prg_nvram.size;
            ram.get_mut(start..end)
                .ok_or_else(|| "mapper PRG RAM is smaller than cartridge metadata".to_string())?
                .copy_from_slice(&bytes[offset..offset + self.memory.prg_nvram.size]);
            offset += self.memory.prg_nvram.size;
        }
        if self.memory.chr_nvram.size != 0 {
            let ram = self
                .inner
                .chr_ram_mut()
                .ok_or_else(|| "mapper exposes no CHR RAM".to_string())?;
            let start = self.memory.chr_ram.size;
            let end = start + self.memory.chr_nvram.size;
            ram.get_mut(start..end)
                .ok_or_else(|| "mapper CHR RAM is smaller than cartridge metadata".to_string())?
                .copy_from_slice(&bytes[offset..offset + self.memory.chr_nvram.size]);
        }
        Ok(())
    }

    fn save_bytes(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        if self.memory.prg_nvram.size != 0 {
            let ram = self
                .inner
                .prg_ram()
                .ok_or_else(|| "mapper exposes no PRG RAM".to_string())?;
            let start = self.memory.prg_ram.size;
            let end = start + self.memory.prg_nvram.size;
            bytes.extend_from_slice(
                ram.get(start..end).ok_or_else(|| {
                    "mapper PRG RAM is smaller than cartridge metadata".to_string()
                })?,
            );
        }
        if self.memory.chr_nvram.size != 0 {
            let ram = self
                .inner
                .chr_ram()
                .ok_or_else(|| "mapper exposes no CHR RAM".to_string())?;
            let start = self.memory.chr_ram.size;
            let end = start + self.memory.chr_nvram.size;
            bytes.extend_from_slice(
                ram.get(start..end).ok_or_else(|| {
                    "mapper CHR RAM is smaller than cartridge metadata".to_string()
                })?,
            );
        }
        Ok(bytes)
    }
}

impl Mapper for ManagedMapper {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.inner.cpu_read(addr)
    }
    fn cpu_write(&mut self, addr: u16, data: u8) {
        self.inner.cpu_write(addr, data)
    }
    fn cpu_write_at(&mut self, addr: u16, data: u8, cycle: u64) {
        self.inner.cpu_write_at(addr, data, cycle)
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.inner.ppu_read(addr)
    }
    fn ppu_write(&mut self, addr: u16, data: u8) {
        self.inner.ppu_write(addr, data)
    }
    fn mirroring(&self) -> Mirroring {
        self.inner.mirroring()
    }
    fn on_ppu_bus_access(&mut self, addr: u16, cycle: u64) {
        self.inner.on_ppu_bus_access(addr, cycle)
    }
    fn irq_pending(&self) -> bool {
        self.inner.irq_pending()
    }
    fn reset(&mut self) {
        self.inner.reset()
    }
    fn prg_ram(&self) -> Option<&[u8]> {
        self.inner.prg_ram()
    }
    fn prg_ram_mut(&mut self) -> Option<&mut [u8]> {
        self.inner.prg_ram_mut()
    }
    fn chr_ram(&self) -> Option<&[u8]> {
        self.inner.chr_ram()
    }
    fn chr_ram_mut(&mut self) -> Option<&mut [u8]> {
        self.inner.chr_ram_mut()
    }

    fn flush_persistent_ram(&self) -> Result<(), String> {
        let Some(path) = self.save_path.as_ref() else {
            return Ok(());
        };
        let bytes = self.save_bytes()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create save directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let file_name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("save.sav");
        let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
        let mut file = File::create(&temp)
            .map_err(|err| format!("failed to create temporary save {}: {err}", temp.display()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|err| format!("failed to write temporary save {}: {err}", temp.display()))?;
        fs::rename(&temp, path)
            .map_err(|err| format!("failed to replace save {}: {err}", path.display()))?;
        Ok(())
    }
}

// Builds the mapper named by the iNES header. Consumes the parsed Rom.
pub fn from_rom(rom: Rom) -> SharedMapper {
    let memory = rom.memory;
    let save_path = rom.save_path.clone();
    let inner: Box<dyn Mapper> = match rom.mapper {
        0 => Box::new(Nrom::from_rom(rom)),
        1 => Box::new(Mmc1::from_rom(rom)),
        2 => Box::new(Uxrom::from_rom(rom)),
        3 => Box::new(Cnrom::from_rom(rom)),
        4 => Box::new(Mmc3::from_rom(rom)),
        7 => Box::new(Axrom::from_rom(rom)),
        9 => Box::new(Mmc2::from_rom(rom)),
        66 => Box::new(Gnrom::from_rom(rom)),
        other => panic!("Mapper {} is not supported yet", other),
    };
    let mut mapper = ManagedMapper {
        inner,
        memory,
        save_path,
    };
    if let Err(err) = mapper.load() {
        eprintln!("warning: {err}");
    }
    let mapper: Box<dyn Mapper> = Box::new(mapper);
    Rc::new(RefCell::new(mapper))
}

// Test helper: an NROM cartridge with the given CHR (as writable CHR-RAM so
// tests may also exercise CHR writes) and mirroring, over 32 KB of zeroed PRG.
#[cfg(test)]
pub fn test_nrom(chr: Vec<u8>, mirroring: Mirroring) -> SharedMapper {
    let nrom = Nrom::new(vec![0; 0x8000], chr, true, mirroring);
    Rc::new(RefCell::new(Box::new(nrom) as Box<dyn Mapper>))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::{MemoryKind, MemoryRegion};

    fn battery_ram_rom(path: PathBuf) -> Rom {
        Rom {
            prg_rom: vec![0; 0x8000],
            chr_rom: vec![],
            mapper: 0,
            metadata: crate::cartridge::CartridgeMetadata::test_defaults(),
            screen_mirroring: Mirroring::Vertical,
            memory: CartridgeMemory {
                prg_rom: MemoryRegion::new(MemoryKind::Rom, 0x8000),
                prg_ram: MemoryRegion::new(MemoryKind::Absent, 0),
                prg_nvram: MemoryRegion::new(MemoryKind::NonVolatileRam, 0x2000),
                chr_rom: MemoryRegion::new(MemoryKind::Absent, 0),
                chr_ram: MemoryRegion::new(MemoryKind::Absent, 0),
                chr_nvram: MemoryRegion::new(MemoryKind::NonVolatileRam, 0x2000),
            },
            save_path: Some(path),
        }
    }

    #[test]
    fn battery_prg_and_chr_ram_round_trip_through_atomic_save() {
        let dir = std::env::temp_dir().join(format!(
            "nes-save-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("game.sav");
        let mapper = from_rom(battery_ram_rom(path.clone()));
        mapper.borrow_mut().cpu_write(0x6123, 0xa5);
        mapper.borrow_mut().ppu_write(0x0456, 0x5a);
        mapper.borrow().flush_persistent_ram().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 0x4000);
        assert_eq!(bytes[0x0123], 0xa5);
        assert_eq!(bytes[0x2000 + 0x0456], 0x5a);
        assert!(
            !dir.join(format!(".game.sav.tmp-{}", std::process::id()))
                .exists()
        );

        let loaded = from_rom(battery_ram_rom(path.clone()));
        assert_eq!(loaded.borrow_mut().cpu_read(0x6123), 0xa5);
        assert_eq!(loaded.borrow_mut().ppu_read(0x0456), 0x5a);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn volatile_ram_is_runtime_visible_but_excluded_from_save_data() {
        let dir = std::env::temp_dir().join(format!(
            "nes-split-save-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("game.sav");
        let mut rom = battery_ram_rom(path.clone());
        rom.memory.prg_ram = MemoryRegion::new(MemoryKind::VolatileRam, 0x400);
        rom.memory.prg_nvram = MemoryRegion::new(MemoryKind::NonVolatileRam, 0x400);
        rom.memory.chr_ram = MemoryRegion::new(MemoryKind::VolatileRam, 0x400);
        rom.memory.chr_nvram = MemoryRegion::new(MemoryKind::NonVolatileRam, 0x400);

        let mapper = from_rom(rom.clone());
        mapper.borrow_mut().cpu_write(0x6001, 0x11);
        mapper.borrow_mut().cpu_write(0x6402, 0x22);
        mapper.borrow_mut().ppu_write(0x0003, 0x33);
        mapper.borrow_mut().ppu_write(0x0404, 0x44);
        mapper.borrow().flush_persistent_ram().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 0x800);
        assert_eq!(bytes[2], 0x22);
        assert_eq!(bytes[0x400 + 4], 0x44);

        let loaded = from_rom(rom);
        assert_eq!(loaded.borrow_mut().cpu_read(0x6001), 0);
        assert_eq!(loaded.borrow_mut().cpu_read(0x6402), 0x22);
        assert_eq!(loaded.borrow_mut().ppu_read(0x0003), 0);
        assert_eq!(loaded.borrow_mut().ppu_read(0x0404), 0x44);
        fs::remove_dir_all(dir).unwrap();
    }
}
