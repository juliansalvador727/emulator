## NES Emulator Progress

Following [bugzmanov/nes_ebook](https://github.com/bugzmanov/nes_ebook).

**Current chapter: 6.4 (rendering the background) — complete. Next up: ch7 (sprites + joypad → playable games).**

- [x] 6502 CPU (all official opcodes)
- [x] Bus
- [x] Running Snake
- [x] Cartridge Support (iNES format)
- [x] CPU trace + `nestest` validation — matches the reference log for all 5003 legal opcodes
- [x] PPU registers (ch 6.1) — CTRL/MASK/STATUS/OAMADDR/OAMDATA/SCROLL/ADDR/DATA + OAM DMA, wired through the bus
- [x] PPU clock + NMI interrupt (ch 6.2) — `ppu.tick()` runs 3× per CPU cycle, raises vblank NMI at scanline 241, CPU services it via the $FFFA vector
- [x] Rendering CHR tiles (ch 6.3) — `render::show_tile` decodes a tile's two bitplanes into a `Frame`; `cargo run -- tiles` blits it to an SDL window
- [x] Rendering the background (ch 6.4) — `Bus` fires a per-frame callback at vblank; `render::render` draws the nametable to a 256×240 `Frame`, with per-tile palettes from the attribute table (`bg_palette`)
- [~] ch7 — makes Pac-Man / Donkey Kong playable  ← you are here
  - [x] Joypad input — `Joypad` wired into the bus at `$4016`; keyboard mapped in `run_game`
  - [ ] Sprite rendering — draw `oam_data` on top of the background in `render::render`
  - [ ] Scrolling (ch8) — SMB1
- [ ] Unofficial/illegal opcodes (optional; needed to finish the rest of `nestest`)

## Commands

All commands run from the `nes_emulator/` directory:

```bash
cd nes_emulator

# Build the project
cargo build

# Run the unit + trace tests (expect: 21 passed)
cargo test

# Run a game ROM and render its background (ch 6.4).
# Expects a `game.nes` (with CHR ROM) in nes_emulator/. Esc to quit.
# (needs SDL2 on the system: `sudo apt install libsdl2-dev` on Debian/WSL)
cargo run

# Dump the CPU trace while running nestest (the ch 5.1 deliverable)
cargo run -- nestest

# View a CHR tile in a window (the ch 6.3 deliverable).
# Optional ROM filename (must have CHR ROM); defaults to nestest.nes.
# Esc or window-close to quit.
cargo run -- tiles                # nestest.nes
cargo run -- tiles pacman.nes     # any ROM you drop in nes_emulator/

# Validate the trace against the reference log.
# It matches for all 5003 legal opcodes, then stops at the first illegal
# opcode (0x04 *NOP) — that's expected until you implement illegal opcodes.
cargo run -- nestest > mynes.log 2>/dev/null
diff <(sed 's/ PPU:.*//' nestest.log | head -n "$(wc -l < mynes.log)") mynes.log
# ^ empty output = perfect match
```
