# Adding Mapper Support to the Rust NES Emulator — Handoff

Context handoff for a future session. This describes what it takes to add
cartridge mapper support to `nes_emulator/` (the Rust emulator following
bugzmanov/nes_ebook). Everything below is grounded in the current code with
file:line references. Read this top to bottom before touching anything.

## Current state (steps 1 and 2 are DONE)

**Update (step 2):** UxROM (#2), CNROM (#3), AxROM (#7), and GNROM/GxROM (#66)
are implemented and merged. Each is a self-contained file in `src/mapper/`
(`uxrom.rs`, `cnrom.rs`, `axrom.rs`, `gnrom.rs`) with its own unit tests, plus a
`match` arm in `from_rom` (`mapper/mod.rs`). AxROM's single-screen mirroring
added two `Mirroring` variants — `SingleScreenLower`/`SingleScreenUpper`
(`cartridge.rs`) — wired through `ppu::mirror_vram_addr` and the renderer's
nametable selection. **105 tests pass** (was 96). No mapper 2/3/7/66 ROMs are in
`games/` yet, so these were verified by unit tests only, not a live render — drop
real carts in `games/` to eyeball them. Next up: **MMC1** (step 3).

## Current state (step 1 is DONE)

**The plumbing refactor and NROM (mapper 0) are implemented and merged into the
working tree.** The mapper trait now sits behind both the bus and the PPU, and
NROM games (SMB, Pac-Man, Donkey Kong) render correctly through it. What exists:

- `src/mapper/mod.rs` — the `Mapper` trait (see below), the
  `SharedMapper = Rc<RefCell<Box<dyn Mapper>>>` alias (`mapper/mod.rs:28`),
  `from_rom()` dispatch (`mapper/mod.rs:31`, panics with "Mapper N is not
  supported yet" for anything but 0), and a `#[cfg(test)] test_nrom()` helper
  (`mapper/mod.rs:42`) used by the ppu/render tests.
- `src/mapper/nrom.rs` — mapper 0. Owns PRG ROM (16 KB mirrored or 32 KB), CHR
  (ROM, or 8 KB CHR-RAM when the header ships no CHR), and 8 KB PRG-RAM at
  `$6000-$7FFF`. Writes to ROM space are inert.
- `bus.rs` holds a `SharedMapper` (`bus.rs:17`), builds it via `from_rom` and
  clones the `Rc` into the PPU (`bus.rs:31`). `$6000-$FFFF` reads/writes route
  through `cpu_read`/`cpu_write` (`bus.rs:107`, `bus.rs:177`) — **ROM-space
  writes are now the bank-switch entry point, not a panic.**
- `ppu/mod.rs` holds a `SharedMapper` (`ppu/mod.rs:12`); `NesPPU::new` takes it
  (`ppu/mod.rs:31`). CHR reads/writes go through `ppu_read`/`ppu_write`
  (`ppu/mod.rs:181`, `:155`). Mirroring is read from the mapper via a new
  `mirroring()` helper (`ppu/mod.rs:207`).
- `render/mod.rs` fetches CHR tiles through `read_tile()` (`render/mod.rs:114`),
  which reads via the mapper's interior mutability; mirroring comes from
  `ppu.mirroring()` (`render/mod.rs:22`).
- `cartridge.rs` still parses `mapper` (`cartridge.rs:25`) and keeps `Rom`'s
  `prg_rom`/`chr_rom` fields (so `show_tile` and the `tiles` viewer are
  untouched); `from_rom` consumes the `Rom` to build the mapper. `Mirroring`
  now derives `Clone, Copy`. The shared `test::test_rom` helper
  (`cartridge.rs:84`) declares mapper 0.

105 tests pass (`cargo test`). `on_scanline` exists as a default no-op with no
consumer yet — it's the MMC3 hook for later.

## The core problem (already solved for step 1, but the shape matters for MMC1+)

A mapper must be visible from **both** sides of the machine at once:

- the **bus** needs it for PRG reads/writes (`$6000-$FFFF`), and
- the **PPU + renderer** need it for CHR reads/writes (`$0000-$1FFF`) **and** for
  mirroring (mappers like MMC1 change mirroring at runtime).

This is why the mapper is a shared `Rc<RefCell<Box<dyn Mapper>>>` cloned into
both `Bus` and `NesPPU` rather than owned by one of them. The renderer takes
`&NesPPU`, so it reaches CHR through the PPU's `Rc` via interior mutability
(`render/mod.rs` `read_tile`).

## The `Mapper` trait (as implemented)

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
state. `mapper/nrom.rs` is the reference implementation to copy.

## How to add a new mapper (the step-2+ recipe)

The plumbing is done, so each new mapper is now self-contained:

1. Add `src/mapper/<name>.rs` with a struct implementing `Mapper`. Crib banking
   math from the C emulator (see the reference section) and model NROM's file.
2. Add `pub mod <name>;` and a `<n> => Box::new(<Name>::from_rom(rom))` arm to
   the `match` in `from_rom` (`mapper/mod.rs:31`).
3. Bank registers arrive as `cpu_write` calls for `$8000-$FFFF` — decode the
   address/data there and recompute your PRG/CHR bank offsets.
4. `cpu_read`/`ppu_read` apply the current bank offsets. `mirroring()` returns
   the current state (fixed for UxROM/CNROM; runtime for MMC1+).
5. Drop a real ROM in `games/` (gitignored) and eyeball it with the probe
   (see Testing). No bus/ppu/render changes should be needed.

## Per-mapper effort (plumbing exists; NROM done)

| Mapper | Difficulty | Notes |
|---|---|---|
| #0 NROM | **DONE** | `mapper/nrom.rs` — the reference implementation |
| #3 CNROM | **DONE** | `mapper/cnrom.rs` — 8 KB CHR bank select on any `$8000+` write |
| #2 UxROM | **DONE** | `mapper/uxrom.rs` — 16 KB PRG bank at `$8000`, fixed last bank |
| #7 AxROM | **DONE** | `mapper/axrom.rs` — 32 KB PRG bank + single-screen mirroring |
| #66 GNROM | **DONE** | `mapper/gnrom.rs` — one register: 32 KB PRG (bits 4-5) + 8 KB CHR (bits 0-1) |
| #1 MMC1 | medium | 5-bit serial shift register, 4 control regs, runtime mirroring + PRG/CHR bank modes |
| #4 MMC3 | hard | Banking is fine; the scanline IRQ is the problem (see caveats) |

Simple mappers are a few dozen lines each. MMC1 ~half a day. MMC3 is the hard one.

## Caveats — read before attempting MMC3

The PPU renders the **whole frame at once at vblank** (the callback fired in
`bus.rs:66`), not dot-by-dot. Two consequences:

1. **MMC3 scanline IRQ** counts PPU A12 rising edges from pattern-table fetches.
   This PPU doesn't model per-dot fetches, so the IRQ has to be *approximated* —
   e.g. tick the MMC3 counter once per visible scanline inside `ppu.tick()`
   (`ppu/mod.rs:55`) via `on_scanline()`. Mid-screen status-bar splits (SMB3,
   Kirby) will mostly work but jitter at the split line.
2. **Mid-frame CHR bank switches** (also MMC3) can't be reproduced at all — the
   renderer only ever sees the bank state present at vblank.

Full MMC3 correctness ultimately wants a dot-accurate PPU rewrite. You can get
"good enough" without it, but not perfect. The simple mappers and most of MMC1
are unaffected by this limitation.

## Recommended order of work

1. ~~**Plumbing refactor** + NROM behind the trait.~~ **DONE** — see "Current
   state" above.
2. ~~**UxROM + CNROM + AxROM + GNROM.**~~ **DONE** — see the step-2 update at the
   top. Each is a new file in `src/mapper/` plus one `match` arm.
3. **MMC1.** ← *start here next.* ~half a day (Zelda, Metroid, Mega Man 2,
   Final Fantasy). `games/zelda.nes` is a mapper-1 cart already present. First one
   with runtime `mirroring()` changes and a serial shift register.
4. **MMC3.** ~1–2 days for banking + approximate IRQ; imperfect until the PPU is
   dot-accurate. This is where `on_scanline()` finally gets wired up.

## Testing

- `cargo test` must stay green (105 tests) at every step.
- The mapper-aware test scaffolding already exists: `cartridge.rs::test::test_rom`
  builds a mapper-0 ROM, and the `ppu/mod.rs` / `render/mod.rs` tests construct a
  PPU via `mapper::test_nrom(chr, mirroring)` (`mapper/mod.rs:42`). New mappers
  can add their own unit tests directly against the struct in their file.
- Manual verification: run real ROMs per mapper. `cargo run -- <path.nes>`.
  The headless probe (`cargo run --release -- probe <rom> "<script>" <frames>`)
  and `PROBE_SHOTS=<dir>` screenshot dumps are useful for eyeballing output
  without a display. **Note:** `PROBE_SHOTS` writes to `<dir>/fNNNNN.bmp` and the
  dir must already exist (`std::fs::write` won't `mkdir -p`). Convert with
  `convert f00100.bmp out.png` to view.
- Good test ROMs to drop in `nes_emulator/games/` (gitignored): a UxROM game
  (e.g. Mega Man), a CNROM game, an MMC1 game (Zelda — currently panics with
  "Mapper 1 is not supported yet"), an MMC3 game (SMB3) to exercise the IRQ
  approximation. Existing NROM ROMs already present: pacman, donkeykong, mario.

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
