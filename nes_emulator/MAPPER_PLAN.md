# Adding Mapper Support to the Rust NES Emulator — Handoff

Context handoff for a future session. This describes exactly what it takes to
add cartridge mapper support to `nes_emulator/` (the Rust emulator following
bugzmanov/nes_ebook). Everything below is grounded in the current code with
file:line references. Read this top to bottom before touching anything.

## Current state (what exists today)

The Rust emulator is **NROM-only**. It parses the mapper number from the iNES
header but nothing uses it:

- `cartridge.rs:25` computes `mapper` and stores it on `Rom`, but no code
  branches on it.
- `bus.rs:14` owns `prg_rom: Vec<u8>`; `read_prg_rom` (`bus.rs:76`) maps it with
  a hardcoded fixed scheme (subtract `0x8000`, mirror a 16 KB image). Writes to
  `$8000-$FFFF` **panic** (`bus.rs:179`).
- `ppu/mod.rs:11` separately owns `chr_rom: Vec<u8>` and indexes it flat in
  `read_data` (`ppu/mod.rs:181`). CHR writes just print a warning
  (`ppu/mod.rs:155`). Mirroring is a fixed `self.mirroring` field used in
  `mirror_vram_addr` (`ppu/mod.rs:205`).
- `render/mod.rs` reaches **directly** into `ppu.chr_rom[...]` during background
  and sprite drawing (background tile fetch, sprite fetch, and `show_tile`).

There are 95 passing tests (`cargo test`). Keeping them green through the
refactor is part of the definition of done.

## The core problem

A mapper must be visible from **both** sides of the machine at once:

- the **bus** needs it for PRG reads/writes (`$6000-$FFFF`), and
- the **PPU + renderer** need it for CHR reads/writes (`$0000-$1FFF`) **and** for
  mirroring (mappers like MMC1 change mirroring at runtime).

Today PRG lives in the bus and CHR lives in the PPU as two separate owners.
Rust won't let two structs own one value, so the real work is a small ownership
refactor, not the mappers themselves. The renderer reaching straight into
`ppu.chr_rom` is the fiddliest part to unpick.

## Design

### 1. A `Mapper` trait

New module `src/mapper/mod.rs` plus one file per mapper (`nrom.rs`, `uxrom.rs`,
`cnrom.rs`, `mmc1.rs`, `mmc3.rs`, ...).

```rust
pub trait Mapper {
    fn cpu_read(&mut self, addr: u16) -> u8;      // $6000-$FFFF: PRG-RAM + PRG-ROM
    fn cpu_write(&mut self, addr: u16, data: u8); // bank-register writes land here
    fn ppu_read(&mut self, addr: u16) -> u8;      // $0000-$1FFF CHR
    fn ppu_write(&mut self, addr: u16, data: u8); // CHR-RAM
    fn mirroring(&self) -> Mirroring;             // MMC1 sets this at runtime
    fn on_scanline(&mut self) {}                   // MMC3 IRQ hook (default no-op)
}
```

The mapper owns PRG ROM, CHR ROM/RAM, PRG RAM, its bank registers, and mirroring
state.

### 2. Shared ownership

Use `Rc<RefCell<Box<dyn Mapper>>>`, constructed in `cartridge.rs` and **cloned**
into both `Bus` and `NesPPU`. This is the smallest diff and matches the existing
layout.

- Alternative: an `enum Mapper { Nrom(..), Uxrom(..), ... }` with static dispatch
  (faster, no `RefCell`), but it's a larger churn. Prefer `Rc<RefCell>` unless
  profiling later says otherwise.
- Note the renderer takes `&NesPPU`, so it borrows the mapper through the PPU's
  `Rc` — CHR fetches in `render/mod.rs` become `mapper.borrow_mut().ppu_read(addr)`.

### 3. Rewire the three consumers

- **`bus.rs`**
  - `read_prg_rom` (`bus.rs:76`) → `self.mapper.borrow_mut().cpu_read(addr)`.
  - Add a `$6000..=$7FFF` arm in `mem_read`/`mem_write` for PRG-RAM.
  - The panicking `$8000..=$FFFF` write arm (`bus.rs:179`) → `cpu_write`. **This
    is how bank switches arrive** — writes to ROM space are the mapper's control
    registers, not errors.
  - Drop the `prg_rom` field; hold the shared mapper instead.
- **`ppu/mod.rs`**
  - Remove the `chr_rom` field (`ppu/mod.rs:11`); hold the shared mapper.
  - `read_data` / `write_to_data` `0..=0x1fff` arms (`:179`, `:155`) go through
    `ppu_read` / `ppu_write`.
  - `mirror_vram_addr` (`:205`) reads `self.mapper.borrow().mirroring()` instead
    of the fixed `self.mirroring` field.
  - `NesPPU::new` / `new_empty_rom` signatures change to take the mapper.
- **`render/mod.rs`**
  - Every `ppu.chr_rom[idx]` → `mapper.borrow_mut().ppu_read(addr)` (background
    tile fetch, sprite fetch, `show_tile`).
- **`cartridge.rs`**
  - Build the correct mapper from the `mapper` byte already parsed at
    `cartridge.rs:25`.
  - Add PRG-RAM.
  - Add CHR-RAM: when CHR size is 0 (`raw[5] == 0`), allocate 8 KB of writable
    CHR instead of a ROM slice.
  - The existing `test::test_rom` helper (`cartridge.rs:84`) and bus/ppu tests
    must be updated to construct through the mapper path.

## Per-mapper effort (after plumbing exists)

| Mapper | Difficulty | Notes |
|---|---|---|
| #0 NROM | trivial | Current fixed mapping, moved behind the trait |
| #3 CNROM | easy | 8 KB CHR bank select on any `$8000+` write |
| #2 UxROM | easy | 16 KB PRG bank at `$8000`, fixed last bank |
| #7 AxROM / #66 GNROM | easy | Single bank register; AxROM adds single-screen mirroring |
| #1 MMC1 | medium | 5-bit serial shift register, 4 control regs, runtime mirroring + PRG/CHR bank modes |
| #4 MMC3 | hard | Banking is fine; the scanline IRQ is the problem (see caveats) |

Simple mappers are a few dozen lines each. MMC1 ~half a day. MMC3 is the hard one.

## Caveats — read before attempting MMC3

The PPU renders the **whole frame at once at vblank** (the callback fired in
`bus.rs:62`), not dot-by-dot. Two consequences:

1. **MMC3 scanline IRQ** counts PPU A12 rising edges from pattern-table fetches.
   This PPU doesn't model per-dot fetches, so the IRQ has to be *approximated* —
   e.g. tick the MMC3 counter once per visible scanline inside `ppu.tick()`
   (`ppu/mod.rs:56`) via `on_scanline()`. Mid-screen status-bar splits (SMB3,
   Kirby) will mostly work but jitter at the split line.
2. **Mid-frame CHR bank switches** (also MMC3) can't be reproduced at all — the
   renderer only ever sees the bank state present at vblank.

Full MMC3 correctness ultimately wants a dot-accurate PPU rewrite. You can get
"good enough" without it, but not perfect. The simple mappers and most of MMC1
are unaffected by this limitation.

## Recommended order of work

1. **Plumbing refactor** + NROM behind the trait. Definition of done: behavior
   identical to today, all 95 tests green. ~1 day. This is the bulk of the risk.
2. **UxROM + CNROM + AxROM + GNROM.** ~half a day total. Biggest payoff per line
   — jumps from a few NROM games to a large fraction of the library (Mega Man,
   Castlevania, Contra, Metal Gear, ...).
3. **MMC1.** ~half a day (Zelda, Metroid, Mega Man 2, Final Fantasy).
4. **MMC3.** ~1–2 days for banking + approximate IRQ; imperfect until the PPU is
   dot-accurate.

Suggested first slice for a single session: **step 1 + step 2** (plumbing,
NROM, UxROM, CNROM). Leave MMC1/MMC3 for follow-ups.

## Testing

- `cargo test` must stay green (95 tests) at every step.
- Update `cartridge.rs::test::test_rom`, and the `bus.rs` / `ppu/mod.rs` tests
  that construct a PPU/bus directly, to go through the mapper.
- Manual verification: run real ROMs per mapper. `cargo run -- <path.nes>`.
  The headless probe (`cargo run --release -- probe <rom> "<script>" <frames>`)
  and `PROBE_SHOTS=<dir>` screenshot dumps are useful for eyeballing output
  without a display.
- Good test ROMs to drop in `nes_emulator/games/` (gitignored): a UxROM game
  (e.g. Mega Man), a CNROM game, an MMC1 game (Zelda), an MMC3 game (SMB3) to
  exercise the IRQ approximation.

## Reference: the C emulator next door

`../NES/` (the C emulator, obaraemmanuel/NES) has a working, mature mapper
system to crib from:

- `NES/src/mappers/mapper.h` — the mapper struct-of-function-pointers interface
  (the C equivalent of the trait above), including the `on_scanline` hook.
- `NES/src/mappers/{nrom via mapper.tmpl.c, uxrom.c, cnrom.c, mmc1.c, mmc3.c,
  axrom.c, gnrom.c, ...}` — concrete implementations.
- `NES/src/ppu.c:292` shows where the C PPU calls `mapper->on_scanline()`
  mid-frame (dot 260-ish) — the timing this Rust port can only approximate.

Its banking math and register decode for each mapper translate almost directly.
