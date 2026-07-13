# Rust NES Emulator

A playable NES emulator written in Rust. It currently combines a complete
official 6502 instruction set with a dot-driven PPU renderer, mapper-controlled
banking and IRQs, controller input, and five-channel audio.

The emulator is NTSC-oriented and uses SDL2 for video, keyboard input, and
audio. NES documentation and test ROMs are the sources of truth for hardware
behaviour.

## Current support

- All official 6502 opcodes, CPU interrupts, and `nestest` trace validation
- Dot-driven PPU rendering with fetch-timed loopy scrolling, sprite evaluation,
  sprite-0 hit timing, vblank/NMI races, and odd-frame skipping
- Background and sprite rendering, including 8×16 sprites, priority, clipping,
  and the eight-sprites-per-line limit
- NROM (0), MMC1 (1), UxROM (2), CNROM (3), MMC3 (4), AxROM (7), and GxROM (66)
- Fetch-driven MMC3 IRQs using qualified PPU A12 edges
- Pulse, triangle, noise, and DMC audio with IRQs, DMA, filtering, and SDL2
  playback
- One standard controller through `$4016`
- Headless performance probes and deterministic visual regression tests

The test suite currently contains 179 passing tests. The prioritized remaining
work is tracked in [`nes_emulator/TODO.md`](nes_emulator/TODO.md).

## Requirements

- A current Rust toolchain
- SDL2 development libraries

On Debian or Ubuntu:

```bash
sudo apt install libsdl2-dev
```

ROM images are not required to build or run the unit tests. Only use ROMs that
you are legally permitted to use.

## Build and test

Run commands from the Rust project directory:

```bash
cd nes_emulator
cargo build --release
cargo test
```

Run a game by passing its path:

```bash
cargo run --release -- games/pacman.nes
cargo run --release -- /path/to/game.nes
```

With no argument, the emulator tries `games/pacman.nes`.

### Controls

| NES control | Keyboard |
|---|---|
| D-pad | Arrow keys |
| A | A |
| B | S |
| Select | Space |
| Start | Enter |
| Quit | Escape |

## Validation and diagnostics

Run the bundled `nestest` trace mode:

```bash
cd nes_emulator
cargo run -- nestest > mynes.log 2>/dev/null
diff <(sed 's/ PPU:.*//' nestest.log | head -n "$(wc -l < mynes.log)") mynes.log
```

The trace matches all 5,003 official-opcode entries and stops when `nestest`
reaches its first unofficial opcode.

Run a headless optimized probe with optional scripted input:

```bash
cargo run --release -- probe games/mario.nes "start@120-135,right@350-" 2100
```

The probe reports frame timing, audio production and drift, frame hashes, DMA
activity, and visible-time PPU writes. It can also create deterministic BMPs
and compare them against reviewed baselines. See
[`nes_emulator/probes/README.md`](nes_emulator/probes/README.md) for all probe
options and the visual-regression runner.

Inspect a CHR tile in an SDL window:

```bash
cargo run -- tiles
cargo run -- tiles /path/to/game.nes
```

## Known limitations

- Rendering-time `$2004/$2007` restrictions and exact PPUMASK transition timing
  remain incomplete.
- OAM DMA has its 513/514-cycle stall, but not alternating bus cycles or complete
  DMC-DMA arbitration.
- Unofficial 6502 opcodes, NES 2.0, PAL/Dendy timing, battery saves, save
  states, and a second controller remain to be implemented.
