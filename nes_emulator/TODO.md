# Rust NES Emulator — Next Steps

Current baseline: the Rust emulator has official 6502 opcodes, APU/DMC DMA,
NROM/MMC1/MMC3 and several simple mappers, a dot-timed PPU layer around the
scanline renderer, and loopy scrolling. `cargo test` currently has 142 passing
tests. NES documentation and test ROMs are the sources of truth for behaviour.

## P0 — Stabilize the current renderer

- [x] Profile a release build with representative NROM, MMC1, and MMC3 ROMs.
  Record emulated frames/sec, host frame time, audio queue depth, dropped
  samples, and screenshots before further PPU work. The immediate goal is a
  steady 60 FPS with no audio underruns or dropped samples.
  - Rust: `src/main.rs`, `src/audio.rs`, `src/probe.rs`
  - Acceptance: `cargo run --release -- probe ...` is repeatable; screenshots
    and audio statistics can be compared in CI or manually.
  - Completed: the probe now emits per-frame CSV plus a summary containing
    emulated FPS, host frame-time average/p95/max, audio queue min/max/end,
    sample-clock drift, drops, underflows, and device reopens. Two-minute
    release results are recorded in `probes/RESULTS.md`.

- [x] Add ROM-level visual regression coverage. The unit tests cover individual
  pixels, but they cannot catch a HUD split, a bad CHR bank, or a stale scanline
  in a real game.
  - Use the existing `PROBE_SHOTS` path to make deterministic BMPs.
  - Start with SMB1 (sprite-0 split), Zelda (MMC1), and an MMC3 title such as
    SMB2/SMB3; keep ROMs out of git if licensing requires it, but document their
    hashes and expected probe scripts.
  - Acceptance: fixed frame numbers have reviewed image baselines and no audio
    queue drift during a multi-minute probe.
  - Completed: `probes/run_visual_regressions.sh` verifies ROM SHA-256 values
    and byte-compares reviewed frames 180/360/600 for SMB1, Zelda, and SMB2.
    The probe's frame callback is now independent of NMI enable, fixing skipped
    presentation/audio drains; 7,200-frame runs have +0.089 sample clock drift.

- [x] Investigate any remaining top/occasional artifacts with a frame capture
  before changing timing again. Check whether they correlate with OAM DMA,
  sprite overflow, or a mid-frame PPU write. The `$FF` OAM-Y wrap bug is fixed,
  so further artifacts should have a reproducible capture attached.
  - Completed: `PROBE_CAPTURE_FRAME`/`PROBE_CAPTURE_RADIUS` save the surrounding
    frames and CSV event context. The reproducible SMB2 transition capture has
    normal OAM DMA and no visible-time PPU writes, pointing to the P1 MMC3 A12/
    dot-timing work; command and findings are in `probes/RESULTS.md`.

## P1 — PPU timing and correctness

- [x] Move from scanline-boundary state updates to a dot-based PPU state
  machine, or introduce a narrow dot-timing layer around the current renderer.
  - Needed for exact `$2005/$2006` split timing, sprite-0 timing, vblank/NMI
    races, and mapper IRQ timing.
  - Acceptance: pass standard PPU timing tests for vblank/NMI, sprite-0 hit,
    scroll, and odd-frame behaviour.
  - Completed: the PPU now advances through individual dots. Vblank/status/NMI,
    loopy increments and reloads, scanline composition, sprite-0 assertion,
    mapper clocks, and the NTSC odd-frame skip occur at explicit raster dots.
    Focused unit tests cover vblank and NMI transitions, suppression, sprite-0
    timing, scroll transfers, and the short odd frame. Timing ROM validation
    remains part of ongoing PPU hardening.

- [ ] Replace the MMC3 "once per visible scanline" IRQ approximation with A12
  edge detection driven by background/sprite fetches. The current timing layer
  calls `on_scanline` at dot 260; real MMC3 reload/IRQ behaviour is edge-based.
  - Rust: `src/ppu/mod.rs`, `src/mapper/mmc3.rs`
  - Acceptance: MMC3 IRQ test ROMs and status-bar splits are stable without
    one-line jitter.

- [ ] Finish PPU register and memory semantics:
  - `$3000-$3EFF` nametable mirrors instead of panicking;
  - palette read mirrors (`$3F10/$3F14/$3F18/$3F1C`), grayscale, and open-bus
    behaviour;
  - sprite overflow status, exact left-edge behavior, and render-enable delay;
  - correct pre-render/vblank ordering and NMI suppression windows.
  - Acceptance: targeted register tests plus relevant blargg/nesdev PPU ROMs.

- [ ] Implement real OAM DMA timing: 513/514 CPU-cycle stalls, parity, and PPU/
  APU progression while DMA runs. Rust currently copies the page through the
  bus but does not model the full stall.

## P1 — Compatibility beyond the current cartridge set

- [ ] Harden existing mapper behaviour before adding many new ones:
  - MMC3 PRG-RAM enable/write-protect (`$A001`), four-screen details, and A12
    timing;
  - MMC1 consecutive-write quirk and large-ROM/SUROM support when a target ROM
    needs them;
  - battery-backed PRG-RAM persistence.

- [ ] Add the highest-value missing mappers with unit and ROM tests for each.
  - Candidates include Color Dreams/mapper 11, CPROM/13, mapper 75 (VRC1),
    mapper 180, and mapper 185.
  - Choose the next mapper from the desired game library rather than by mapper
    number; document each ROM that motivates it.

- [ ] Improve cartridge parsing: NES 2.0, submappers, PRG/CHR RAM size flags,
  battery saves, TV/region metadata, and stricter iNES validation. Rust
  currently rejects NES 2.0 in `src/cartridge.rs`.

## P1 — CPU and APU test-ROM compatibility

- [ ] Implement the commonly used unofficial 6502 opcodes and timing. The
  official-opcode trace is strong, but `nestest` stops at the first illegal
  opcode and some commercial ROMs depend on them.
  - Acceptance: complete `nestest` trace and dedicated illegal-opcode tests.

- [ ] Validate APU timing with test ROMs, especially `$4017` parity/delay, DMC
  DMA stalls, IRQ acknowledgement, mixer levels, and filter/sample-rate
  behaviour. Add PAL timing tables when region support is introduced.

## P2 — Broader hardware and product features

- [ ] Add PAL/Dendy timing and region selection; Rust is currently
  NTSC-oriented.
- [ ] Add second-controller support and any required peripherals (zapper,
  Four Score) for the target library.
- [ ] Add save states, reset/power-cycle semantics, and configurable battery
  save locations.
- [ ] Consider NSF playback and Game Genie support only if they become product
  goals; both are independent of core game emulation.

## Working rules

- Keep `cargo test` green; add a focused unit test for every hardware fix.
- For timing work, add a ROM-level probe or a test-ROM result in addition to a
  unit test.
- Verify hardware behaviour against NES documentation and test ROMs.
- Do performance work with `--release`; debug-mode frame rate is not a useful
  emulator-speed metric.
