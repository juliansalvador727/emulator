# Rust NES Emulator

A playable NES emulator written in Rust. It currently combines a complete
official 6502 instruction set with a dot-driven PPU renderer, mapper-controlled
banking and IRQs, controller input, and five-channel audio.

The emulator is NTSC-oriented. Video, keyboard input, and bound-stream audio
share one bundled SDL3 runtime, matching the lifecycle used by the stable C
frontend. NES documentation and test ROMs are the sources of truth for hardware
behaviour.

## Current support

- All official 6502 opcodes, CPU interrupts, and `nestest` trace validation
- Dot-driven PPU rendering with fetch-timed loopy scrolling, sprite evaluation,
  sprite-0 hit timing, vblank/NMI races, and odd-frame skipping
- Background and sprite rendering, including 8×16 sprites, priority, clipping,
  and the eight-sprites-per-line limit
- NROM (0), MMC1 (1), UxROM (2), CNROM (3), MMC3 (4), AxROM (7), and GxROM (66)
- Fetch-driven MMC3 IRQs using qualified PPU A12 edges
- Pulse, triangle, noise, and DMC audio with IRQs, DMA, filtering, and SDL3
  playback
- One standard controller through `$4016`
- Headless performance probes and deterministic visual regression tests

The test suite currently contains 211 passing tests. The prioritized remaining
work is tracked in [`nes_emulator/TODO.md`](nes_emulator/TODO.md).

## Requirements

- A current Rust toolchain
- A C compiler and CMake (to build the bundled SDL3 runtime)

On Debian or Ubuntu:

```bash
sudo apt install build-essential cmake
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

For the lowest host latency on a native audio stack, select the low-latency
profile and optional one-frame run-ahead:

```bash
cargo run --release -- games/mario.nes \
  --audio-profile low --audio-latency-ms 40 --run-ahead 1
```

`low` delivers 256-sample chunks to a 48 kHz signed-16-bit SDL3 bound stream
and targets 40 ms of queued input. `balanced` targets 80 ms and is selected
automatically under WSL; `--audio-latency-ms` overrides either target. The
process-wide SDL3 transport uses a small adaptive sample-count correction (at
most 1.5%) before SDL to absorb host-clock drift without changing NES CPU or
video speed. The SDL stream itself stays at a fixed 48 kHz, matching the stable
C transport behavior.

`--latency-debug` reports presentation time, queued/pending audio, playback
correction, backpressure, paused-device resumes, drops, underflows, and
sampled-input-to-controller-poll time once per second. SDL queue inspection and
submission happen at most once every 16 ms, matching the stable C frontend's
frame-rate cadence. Queue pressure is bounded without clearing the live SDL
stream: feeding stops at the target and only excess not-yet-submitted samples
are discarded. A bound device found paused under backpressure is resumed in
place; the emulator never waits for a blocked WSLg device close. The equivalent
environment variables are `NES_AUDIO_PROFILE`, `NES_AUDIO_LATENCY_MS`,
`NES_RUN_AHEAD_FRAMES`, and `NES_LATENCY_DEBUG`.

If audio goes silent, `backpressure` climbs continuously, and
`/mnt/wslg/pulseaudio.log` contains
`rdp-sink ... data_send: send failed`, the Windows-side WSLg audio transport has
failed; no Linux audio client can consume normally in that state. Run
`wsl --shutdown` from Windows PowerShell, then reopen the distro before testing
again.

With no argument, the emulator tries `games/pacman.nes`.

### Controls

| NES control | Keyboard   |
| ----------- | ---------- |
| D-pad       | Arrow keys |
| A           | A          |
| B           | S          |
| Select      | Space      |
| Start       | Enter      |
| Quit        | Escape     |

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
