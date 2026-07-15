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
- On-launch game-selection menu: an arrow-key ROM picker that scans `games/`,
  drawn through the same 256×240 frame pipeline as gameplay
- Fullscreen and borderless windowed-fullscreen with aspect-correct black bars
- Native Windows cross-build from WSL, wired as the default `cargo run` target
- Headless performance probes and deterministic visual regression tests

The test suite currently contains 245 passing tests. The prioritized remaining
work is tracked in [`TODO.md`](TODO.md).

## Requirements

- A current Rust toolchain
- A C compiler and CMake (to build the bundled SDL3 runtime)
- The mingw-w64 toolchain and the `x86_64-pc-windows-gnu` Rust target, since
  that is the default build target on this project (see
  [Native Windows build](#native-windows-build-preferred-on-wsl-hosts) for
  setup); native-Linux builds via `cargo lin*` need only the C compiler and CMake

On Debian or Ubuntu:

```bash
sudo apt install build-essential cmake
```

ROM images are not required to build or run the unit tests. Only use ROMs that
you are legally permitted to use.

## Default build target

`.cargo/config.toml` sets the default cargo target to
`x86_64-pc-windows-gnu`, so a bare `cargo run` / `cargo build` / `cargo test`
cross-compiles to a native Windows binary — and `cargo run` launches it through
WSL interop. This is deliberate: under WSL2 the WSLg audio bridge adds latency
and can wedge, while a real Windows process talks straight to WASAPI (see
[Native Windows build](#native-windows-build-preferred-on-wsl-hosts)). The
first Windows build is slow because SDL3 compiles from source.

Native-Linux builds and tests — the fast iteration path, since they skip the
mingw link and interop launch — are available through one-word aliases (an
explicit `--target x86_64-unknown-linux-gnu` on any cargo command is
equivalent):

```bash
cargo lin-build                  # build on native Linux
cargo lin-test                   # run the unit tests on native Linux
cargo lin -- games/pacman.nes    # run a game on native Linux
```

## Build and test

```bash
cargo build --release            # Windows binary (default target)
cargo lin-test                   # unit tests, native Linux
```

Run a game by passing its path (default target is Windows via WSL interop; use
`cargo lin -- <rom>` to run natively on Linux instead):

```bash
cargo run --release -- games/pacman.nes
cargo run --release -- /path/to/game.nes
```

With no ROM argument, launch opens the [game-selection
menu](#game-selection-menu):

```bash
cargo run --release
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

### Game-selection menu

Launched with no ROM argument, the emulator opens a selection screen instead of
booting a fixed game. It scans the `games/` directory for `.nes` files and lists
them alphabetically; drop a new ROM into the folder and it appears
automatically. The screen is rendered with a built-in bitmap font into the same
256×240 frame the emulator presents for gameplay, so it inherits the aspect-
correct scaling and fullscreen handling.

| Menu action    | Keyboard         |
| -------------- | ---------------- |
| Move selection | Up / Down        |
| Launch game    | Enter or Space   |
| Quit           | Escape           |

Pressing Escape during a game returns to this menu rather than quitting; closing
the window quits the program. Passing a ROM path bypasses the menu and boots
straight into that game.

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
lower latency targets (20 ms) work. Because this is the good path on a WSL host,
`x86_64-pc-windows-gnu` is the **default** cargo target (see [Default build
target](#default-build-target)). One-time setup:

```bash
sudo apt-get install -y gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
```

A bare `cargo run -- games/mario.nes` now builds and launches the Windows
binary straight from the WSL shell (it runs as a real Windows process via WSL
interop). `.cargo/config.toml` also defines convenience aliases:

```bash
cargo win -- games/mario.nes --fullscreen   # same as cargo run, but --release
cargo win-build                              # release Windows build, no launch
cargo lin -- games/mario.nes                 # run on native Linux instead
```

The first build is slow because SDL3 compiles from source. The `.exe` at
`target/x86_64-pc-windows-gnu/{debug,release}/julian_nes_emulator.exe` also runs
from the Windows side directly. Native Windows builds always default to the
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
| Back / quit | Escape     |

Escape returns to the [game-selection menu](#game-selection-menu) when the game
was launched from it, and otherwise quits.

## Validation and diagnostics

These run on the native-Linux target via the `cargo lin` alias — that keeps the
trace output line-ending-clean for the diff and skips the interop launch.

Run the bundled `nestest` trace mode:

```bash
cargo lin -- nestest > mynes.log 2>/dev/null
diff <(sed 's/ PPU:.*//' nestest.log | head -n "$(wc -l < mynes.log)") mynes.log
```

The trace matches all 5,003 official-opcode entries and stops when `nestest`
reaches its first unofficial opcode.

Run a headless optimized probe with optional scripted input:

```bash
cargo lin --release -- probe games/mario.nes "start@120-135,right@350-" 2100
```

The probe reports frame timing, audio production and drift, frame hashes, DMA
activity, and visible-time PPU writes. It can also create deterministic BMPs
and compare them against reviewed baselines. See
[`probes/README.md`](probes/README.md) for all probe
options and the visual-regression runner.

Inspect a CHR tile in an SDL window:

```bash
cargo lin -- tiles
cargo lin -- tiles /path/to/game.nes
```

## Known limitations

- Rendering-time `$2004/$2007` restrictions and exact PPUMASK transition timing
  remain incomplete.
- OAM DMA uses alternating get/put bus cycles with complete DMC-DMA arbitration,
  including start, middle, and end-window collisions.
- Unofficial 6502 opcodes, NES 2.0, PAL/Dendy timing, battery saves, save
  states, and a second controller remain to be implemented.

## License

Released under the [MIT License](LICENSE). This repository ships no game ROMs;
supply your own dumps of titles you legally own.
