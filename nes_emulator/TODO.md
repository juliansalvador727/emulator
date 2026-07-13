# Rust NES Emulator — Next Steps

Current baseline: the Rust emulator has official 6502 opcodes, APU/DMC DMA,
NROM/MMC1/MMC3 and several simple mappers, scanline rendering, and loopy
scrolling. `cargo test` currently has 132 passing tests. The C emulator in
`../NES/` is a useful implementation reference, but NES test ROMs remain the
source of truth for behaviour.

## P0 — Stabilize the current renderer

- [ ] Profile a release build with representative NROM, MMC1, and MMC3 ROMs.
  Record emulated frames/sec, host frame time, audio queue depth, dropped
  samples, and screenshots before further PPU work. The immediate goal is a
  steady 60 FPS with no audio underruns or dropped samples.
  - Rust: `src/main.rs`, `src/audio.rs`, `src/probe.rs`
  - Acceptance: `cargo run --release -- probe ...` is repeatable; screenshots
    and audio statistics can be compared in CI or manually.

- [ ] Add ROM-level visual regression coverage. The unit tests cover individual
  pixels, but they cannot catch a HUD split, a bad CHR bank, or a stale scanline
  in a real game.
  - Use the existing `PROBE_SHOTS` path to make deterministic BMPs.
  - Start with SMB1 (sprite-0 split), Zelda (MMC1), and an MMC3 title such as
    SMB2/SMB3; keep ROMs out of git if licensing requires it, but document their
    hashes and expected probe scripts.
  - Acceptance: fixed frame numbers have reviewed image baselines and no audio
    queue drift during a multi-minute probe.

- [ ] Investigate any remaining top/occasional artifacts with a frame capture
  before changing timing again. Check whether they correlate with OAM DMA,
  sprite overflow, or a mid-frame PPU write. The `$FF` OAM-Y wrap bug is fixed,
  so further artifacts should have a reproducible capture attached.

## P1 — PPU timing and correctness

- [ ] Move from scanline-boundary state updates to a dot-based PPU state
  machine, or introduce a narrow dot-timing layer around the current renderer.
  The C PPU already models dots, visible/pre-render lines, and odd-frame skip
  in `../NES/src/ppu.c`.
  - Needed for exact `$2005/$2006` split timing, sprite-0 timing, vblank/NMI
    races, and mapper IRQ timing.
  - Acceptance: pass standard PPU timing tests for vblank/NMI, sprite-0 hit,
    scroll, and odd-frame behaviour.

- [ ] Replace the MMC3 "once per visible scanline" IRQ approximation with A12
  edge detection driven by background/sprite fetches. The C reference calls
  `on_scanline` around dot 260; real MMC3 reload/IRQ behaviour is edge-based.
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
  APU progression while DMA runs. The C implementation has this in
  `../NES/src/ppu.c::dma`; Rust currently copies the page through the bus but
  does not model the full stall.

## P1 — Compatibility beyond the current cartridge set

- [ ] Harden existing mapper behaviour before adding many new ones:
  - MMC3 PRG-RAM enable/write-protect (`$A001`), four-screen details, and A12
    timing;
  - MMC1 consecutive-write quirk and large-ROM/SUROM support when a target ROM
    needs them;
  - battery-backed PRG-RAM persistence.

- [ ] Add the highest-value missing mappers, using the C implementations as
  banking references and adding unit + ROM tests for each.
  - C implementations available now: Color Dreams/mapper 11, CPROM/13,
    mapper 75 (VRC1), mapper 180, and mapper 185.
  - Choose the next mapper from the desired game library rather than by mapper
    number; document each ROM that motivates it.

- [ ] Improve cartridge parsing: NES 2.0, submappers, PRG/CHR RAM size flags,
  battery saves, and stricter iNES validation. The C loader tracks mapper
  format/submapper and TV type in `../NES/src/mappers/mapper.h`; Rust currently
  rejects NES 2.0 in `src/cartridge.rs`.

## P1 — CPU and APU test-ROM compatibility

- [ ] Implement the commonly used unofficial 6502 opcodes and timing. The
  official-opcode trace is strong, but `nestest` stops at the first illegal
  opcode and some commercial ROMs depend on them.
  - Acceptance: complete `nestest` trace and dedicated illegal-opcode tests.

- [ ] Validate APU timing with test ROMs, especially `$4017` parity/delay, DMC
  DMA stalls, IRQ acknowledgement, mixer levels, and filter/sample-rate
  behaviour. The C APU has NTSC/PAL tables and a cycle-oriented implementation
  in `../NES/src/apu.c`; use it as a comparison aid, not a behavioural oracle.

## P2 — Broader hardware and product features

- [ ] Add PAL/Dendy timing and region selection. The C emulator supports NTSC,
  PAL, Dendy, and dual-region metadata; Rust is currently NTSC-oriented.
- [ ] Add second-controller support and any required peripherals (zapper,
  Four Score) for the target library.
- [ ] Add save states, reset/power-cycle semantics, and configurable battery
  save locations.
- [ ] Consider NSF playback and Game Genie support only if they are product
  goals. Both exist in the C emulator (`nsf.*`, `genie.*`) but are independent
  of core game emulation.

## Working rules

- Keep `cargo test` green; add a focused unit test for every hardware fix.
- For timing work, add a ROM-level probe or a test-ROM result in addition to a
  unit test.
- Keep the C reference links in commit messages or code comments, but verify
  against NES documentation/test ROMs when they disagree.
- Do performance work with `--release`; debug-mode frame rate is not a useful
  emulator-speed metric.
