//! Headless runner for blargg and nesdev test ROM result conventions.
//!
//! A compatible test writes the signature DE B0 61 to $6001-$6003, keeps
//! $6000 at $80 or above while running, then writes a result below $80. Zero
//! means pass; text beginning at $6004 is a NUL-terminated diagnostic. Older
//! PPU/MMC3 suites end in a `JMP`-to-self and leave result code 1 at $00F8 on
//! success; that convention is supported as well.
//!
//! A third class reports no result this runner can see. It prints to an
//! on-screen console and a bit-banged serial port, and self-checks with an
//! internal `check_crc` -- there is no $6000 signature and no $00F8 code, so a
//! run that completes normally still ends in "timed out ... without seeing the
//! blargg signature". That is not a hang. `dmc_dma_during_read4` is one of
//! these.
//!
//! To read one, set `PRINT_HOOK` to the hex address of the ROM's `print_char_`
//! and every byte it prints is dumped to stderr at the end of the run:
//!
//! ```text
//! PRINT_HOOK=e679 <emulator> test-rom dmc_dma_during_read4/dma_4016_read.nes 20000000
//! ```
//!
//! $e679 is `print_char_` in `dmc_dma_during_read4`. In other ROMs, find it by
//! scanning PRG for `JSR abs; JMP abs`: the shell defines `print_char_` as
//! `jsr console_print / jmp serial_write`. There is no early exit, so pass an
//! instruction cap that covers the run.
//!
//! Two traps when judging these ROMs:
//!
//! - The shipped .nes can disagree with the header comment in its `source/`
//!   directory; the comments are stale and the `check_crc` constant compiled
//!   into the binary is the only oracle. `dma_2007_write` genuinely passes while
//!   printing output its own comment does not document.
//! - `check_crc` is a standard CRC-32 over the *raw bytes* passed to
//!   `print_a`/`print_hex`, not over the printed ASCII, and it excludes spaces
//!   and newlines. So `zlib.crc32(bytes([...]))` against that constant predicts
//!   pass/fail without running anything.

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
    // These ROMs (e.g. the BRK and interrupt suites) execute BRK as a real
    // software interrupt rather than a halt.
    cpu.set_halt_on_brk(false);
    cpu.power_on();

    let mut instructions = 0_u64;
    let mut protocol_started = false;
    let mut result = None;
    let mut legacy_result = false;
    let mut resets = 0_u8;
    let print_hook = std::env::var("PRINT_HOOK")
        .ok()
        .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    let mut captured = String::new();
    cpu.run_until(|cpu| {
        instructions += 1;
        if let Some(hook) = print_hook {
            if cpu.program_counter == hook {
                captured.push(cpu.register_a as char);
            }
        }
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
            // Reset-test ROMs publish status $81, finish configuring the state
            // they want preserved, then enter a JMP-to-self while waiting for
            // the front-panel reset button. Reset only once that loop is
            // reached; reacting immediately to the status write would cut off
            // the ROM's remaining setup instructions.
            if protocol_started && status == 0x81 {
                resets = resets.saturating_add(1);
                if resets > 8 {
                    return true;
                }
                cpu.reset();
                return false;
            }
            // Zero is the unset/running value for the legacy convention and
            // many timing ROMs intentionally use a temporary JMP-to-self while
            // waiting for an IRQ. Only stop once the ROM publishes a nonzero
            // legacy result (1 means pass, larger values mean failure).
            if !protocol_started {
                let legacy_status = cpu.mem_read(0x00f8);
                if legacy_status != 0 {
                    result = Some(legacy_status);
                    legacy_result = true;
                    return true;
                }
            }
        }
        instructions >= max_instructions
    });

    if print_hook.is_some() {
        eprintln!("PRINT_CAPTURE ({instructions} instrs):\n{captured}\n--- end capture ---");
    }

    let status = result.ok_or_else(|| {
        let pc = cpu.program_counter;
        if protocol_started {
            format!(
                "timed out after {instructions} instructions while test was running (PC=${pc:04X})"
            )
        } else {
            format!(
                "timed out after {instructions} instructions without seeing the blargg signature (PC=${pc:04X})"
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
