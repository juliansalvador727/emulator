# Rust NES Emulator

A playable NES emulator written in Rust. It currently combines a complete
official 6502 instruction set with a dot-driven PPU renderer, mapper-controlled
banking and IRQs, controller input, and five-channel audio.

The emulator is NTSC-oriented. Video, keyboard input, and bound-stream audio
share one bundled SDL3 runtime with a single, carefully managed lifecycle. NES
documentation and test ROMs are the sources of truth for hardware behaviour.

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
- Automatic recovery from wedged host audio (stall watchdog with staged
  resume/clear/reopen; device open/close isolated on a helper thread)
- One standard controller through `$4016`
- Fullscreen and borderless windowed-fullscreen with aspect-correct black bars
- Native Windows cross-build from WSL (`cargo win`)
- Headless performance probes and deterministic visual regression tests

The test suite currently contains 220 passing tests. The prioritized remaining
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
video speed. The SDL stream itself stays at a fixed 48 kHz.

`--latency-debug` reports presentation time, queued/pending audio, playback
correction, backpressure, paused-device resumes, drops, underflows, and
sampled-input-to-controller-poll time once per second. SDL queue inspection and
submission happen at most once every 16 ms, matching the display's frame-rate
cadence. Queue pressure is bounded without clearing the live SDL
stream: feeding stops at the target and only excess not-yet-submitted samples
are discarded. The equivalent environment variables are `NES_AUDIO_PROFILE`,
`NES_AUDIO_LATENCY_MS`, `NES_RUN_AHEAD_FRAMES`, and `NES_LATENCY_DEBUG`.

A stall watchdog recovers wedged host audio automatically: sustained
backpressure triggers an in-place resume after 250 ms, a queue clear at
600 ms, and a full stream reopen at 1.25 s, with recovery only trusted after
500 ms of continuous health. Stream open/destroy run on a helper thread, so
even an SDL call that blocks forever (a fully wedged WSLg server) leaves
gameplay, video, and memory bounds intact — audio simply stays off until the
server returns. If WSLg audio dies permanently, `wsl --shutdown` from Windows
PowerShell is the only full reset; running natively on Windows (below) avoids
the problem entirely.

With no argument, the emulator tries `games/pacman.nes`.

### Fullscreen

Both modes preserve the NES aspect ratio, filling leftover space with black
bars instead of stretching:

- `--fullscreen` (or F11 in-game): SDL fullscreen at desktop resolution.
- `--windowed-fullscreen` (alias `--borderless`): a borderless window covering
  the desktop; alt-tabs like a normal window and never owns the display mode.

### Native Windows build (preferred on WSL hosts)

Under WSL2, audio crosses the WSLg RDP bridge, which adds latency and can
wedge mid-session. Building for Windows lets SDL3 talk directly to WASAPI and
the compositor — measured result: no audio, fps, or input-latency issues, and
lower latency targets (20 ms) work. One-time setup:

```bash
sudo apt-get install -y gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
```

Then, from `nes_emulator/`, build and run the Windows binary straight from the
WSL shell (it launches as a real Windows process via WSL interop):

```bash
cargo win -- ../games/mario.nes --fullscreen
```

`cargo win` and `cargo win-build` are aliases in `nes_emulator/.cargo/config.toml`.
The first build is slow because SDL3 compiles from source. The `.exe` at
`target/x86_64-pc-windows-gnu/release/julian_nes_emulator.exe` also runs from
the Windows side directly. Native Windows builds always default to the
low-latency audio profile; the WSL detection is compile-time-disabled there
because `WSL_DISTRO_NAME` leaks into Windows processes launched from WSL.

### Controls

| NES control | Keyboard   |
| ----------- | ---------- |
| D-pad       | Arrow keys |
| A           | A          |
| B           | S          |
| Select      | Space      |
| Start       | Enter      |
| Fullscreen  | F11        |
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

## License

Released under the [MIT License](LICENSE). This repository ships no game ROMs;
supply your own dumps of titles you legally own.
