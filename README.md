## NES Emulator Progress

Following [bugzmanov/nes_ebook](https://github.com/bugzmanov/nes_ebook).

**Current chapter: 5.1 (CPU trace / `nestest`) — complete. Next up: 6.1 (PPU).**

- [x] 6502 CPU (all official opcodes)
- [x] Bus
- [x] Running Snake
- [x] Cartridge Support (iNES format)
- [x] CPU trace + `nestest` validation — matches the reference log for all 5003 legal opcodes
- [ ] PPU  ← you are here (ch 6.1, `src/ppu.rs` is still empty)
  - [ ] Scrolling
- [ ] Unofficial/illegal opcodes (optional; needed to finish the rest of `nestest`)

## Commands

All commands run from the `nes_emulator/` directory:

```bash
cd nes_emulator

# Build the project
cargo build

# Run the unit + trace tests (expect: 10 passed)
cargo test

# Play the Snake game
# (needs SDL2 on the system: `sudo apt install libsdl2-dev` on Debian/WSL)
cargo run

# Dump the CPU trace while running nestest (the ch 5.1 deliverable)
cargo run -- nestest

# Validate the trace against the reference log.
# It matches for all 5003 legal opcodes, then stops at the first illegal
# opcode (0x04 *NOP) — that's expected until you implement illegal opcodes.
cargo run -- nestest > mynes.log 2>/dev/null
diff <(sed 's/ PPU:.*//' nestest.log | head -n "$(wc -l < mynes.log)") mynes.log
# ^ empty output = perfect match
```
