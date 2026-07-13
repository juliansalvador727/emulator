//! Headless runner for blargg and nesdev test ROM result conventions.
//!
//! A compatible test writes the signature DE B0 61 to $6001-$6003, keeps
//! $6000 at $80 or above while running, then writes a result below $80. Zero
//! means pass; text beginning at $6004 is a NUL-terminated diagnostic. Older
//! PPU/MMC3 suites end in a `JMP`-to-self and leave result code 1 at $00F8 on
//! success; that convention is supported as well.

use crate::bus::Bus;
use crate::cartridge::Rom;
use crate::cpu::{Mem, CPU};

const STATUS_ADDR: u16 = 0x6000;
const SIGNATURE_ADDR: u16 = 0x6001;
const MESSAGE_ADDR: u16 = 0x6004;
const SIGNATURE: [u8; 3] = [0xde, 0xb0, 0x61];

pub fn run(path: &str, max_instructions: u64) -> Result<(), String> {
    if max_instructions == 0 {
        return Err("instruction limit must be greater than zero".to_string());
    }

    let bytes = std::fs::read(path).map_err(|err| format!("failed to read {path}: {err}"))?;
    let rom = Rom::new(&bytes).map_err(|err| format!("failed to parse {path}: {err}"))?;
    let bus = Bus::new(rom, |_, _, _| {});
    let mut cpu = CPU::new(bus);
    cpu.reset();

    let mut instructions = 0_u64;
    let mut protocol_started = false;
    let mut result = None;
    let mut legacy_result = false;
    cpu.run_until(|cpu| {
        instructions += 1;
        let status = cpu.mem_read(STATUS_ADDR);
        let signature = [
            cpu.mem_read(SIGNATURE_ADDR),
            cpu.mem_read(SIGNATURE_ADDR + 1),
            cpu.mem_read(SIGNATURE_ADDR + 2),
        ];
        if signature == SIGNATURE && status >= 0x80 {
            protocol_started = true;
        } else if protocol_started && signature == SIGNATURE && status < 0x80 {
            result = Some(status);
            return true;
        }

        // Older blargg PPU and MMC3 suites have no $6000 signature. Their
        // reporting path stores a code at $F8 and ends at `JMP exit`, where
        // exit is the address of that same instruction.
        let pc = cpu.program_counter;
        if instructions > 1_000
            && cpu.mem_read(pc) == 0x4c
            && (u16::from(cpu.mem_read(pc.wrapping_add(1)))
                | (u16::from(cpu.mem_read(pc.wrapping_add(2))) << 8))
                == pc
        {
            result = Some(cpu.mem_read(0x00f8));
            legacy_result = true;
            return true;
        }
        instructions >= max_instructions
    });

    let status = result.ok_or_else(|| {
        if protocol_started {
            format!("timed out after {instructions} instructions while test was running")
        } else {
            format!(
                "timed out after {instructions} instructions without seeing the blargg signature"
            )
        }
    })?;
    let message = if legacy_result {
        format!("legacy result code {status}")
    } else {
        read_message(&mut cpu)
    };
    println!(
        "TEST_ROM_RESULT protocol={} status={} instructions={} message={}",
        if legacy_result { "legacy-f8" } else { "blargg-6000" },
        status,
        instructions,
        escape_message(&message)
    );

    if (legacy_result && status == 1) || (!legacy_result && status == 0) {
        Ok(())
    } else {
        Err(format!("test ROM returned status {status}: {message}"))
    }
}

fn read_message(cpu: &mut CPU<'_>) -> String {
    let bytes: Vec<u8> = (MESSAGE_ADDR..=0x60ff)
        .map(|addr| cpu.mem_read(addr))
        .take_while(|byte| *byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn escape_message(message: &str) -> String {
    message
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_messages_are_kept_on_one_machine_readable_line() {
        assert_eq!(escape_message("pass\r\nline\\two"), "pass\\r\\nline\\\\two");
    }
}
