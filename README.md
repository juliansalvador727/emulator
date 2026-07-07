## NES Emulator Progress

Following [bugzmanov/nes_ebook](https://github.com/bugzmanov/nes_ebook).

**Current chapter: 6.2 (PPU clock + NMI interrupt) — complete. Next up: 6.3 (rendering CHR tiles).**

- [x] 6502 CPU (all official opcodes)
- [x] Bus
- [x] Running Snake
- [x] Cartridge Support (iNES format)
- [x] CPU trace + `nestest` validation — matches the reference log for all 5003 legal opcodes
- [x] PPU registers (ch 6.1) — CTRL/MASK/STATUS/OAMADDR/OAMDATA/SCROLL/ADDR/DATA + OAM DMA, wired through the bus
- [x] PPU clock + NMI interrupt (ch 6.2) — `ppu.tick()` runs 3× per CPU cycle, raises vblank NMI at scanline 241, CPU services it via the $FFFA vector
- [ ] Rendering CHR tiles (ch 6.3) / background (ch 6.4)  ← you are here
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
