use crate::cartridge::Mirroring;
use crate::mapper::SharedMapper;
use crate::render::{frame::Frame, palette::SYSTEM_PALLETE};
use registers::control::ControlRegister;
use registers::loopy::LoopyRegister;
use registers::mask::MaskRegister;
use registers::status::StatusRegister;

pub mod registers;

// The 2C02's CPU-facing I/O bus is a dynamic latch. Hardware measurements
// vary with console and temperature (roughly 3-30 ms), so use a deterministic
// 10 ms NTSC value. Decay is applied lazily when the CPU next observes or
// drives the latch; this is equivalent to clocking it on every PPU dot.
const PPU_IO_BUS_DECAY_DOTS: u64 = 53_693;

// PPUMASK's rendering-enable signal crosses a short internal pipeline before
// it hands VRAM/OAM ownership to the renderer. A write made between PPU dots
// leaves the next four dots in the old state; pixel-component and color bits
// themselves remain directly observable from PPUMASK.
const PPUMASK_RENDER_DELAY_DOTS: u64 = 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeDiagnostics {
    pub oam_dma_count: u64,
    pub visible_register_writes: u64,
    pub last_register: u16,
    pub last_scanline: u16,
    pub last_dot: usize,
}

#[derive(Clone, Copy, Default)]
struct SpriteUnit {
    pattern_lo: u8,
    pattern_hi: u8,
    attributes: u8,
    x_counter: u8,
    oam_index: u8,
    valid: bool,
}

#[derive(Clone)]
pub struct NesPPU {
    pub mapper: SharedMapper,
    pub palette_table: [u8; 32],
    pub vram: [u8; 2048],
    pub oam_data: [u8; 256],

    loopy: LoopyRegister,
    pub ctrl: ControlRegister,
    pub mask: MaskRegister,
    pub status: StatusRegister,
    pub oam_addr: u8,
    rendering_enabled: bool,
    pending_rendering_enabled: bool,
    rendering_change_at: Option<u64>,
    // Value currently visible on the PPU's internal OAM data bus. During
    // rendering, $2004 observes this bus instead of indexing primary OAM with
    // OAMADDR.
    oam_data_bus: u8,
    internal_data_buf: u8,
    io_data_bus: u8,
    io_data_bus_refreshed_at: [u64; 8],

    // Current NTSC raster position. Dots are numbered 0..=340 and scanlines
    // 0..=261, with 261 being the pre-render line.
    scanline: u16,
    dot: u16,
    odd_frame: bool,
    odd_skip_armed: bool,
    total_dots: u64,
    // Host-facing presentation event. This is intentionally separate from
    // both frame wrap and the NMI line: a completed image is ready when
    // vblank starts even if the game has disabled NMI.
    frame_ready: bool,
    suppress_vblank: bool,
    pub nmi_interrupt: Option<u8>,
    nmi_interrupt_at: u64,

    // Background fetch latches and the four 16-bit pixel shifters. The high
    // byte contains the currently visible tile; the low byte is reloaded with
    // the tile fetched over the preceding eight dots.
    next_tile_id: u8,
    next_tile_attr: u8,
    next_tile_lo: u8,
    next_tile_hi: u8,
    bg_pattern_lo: u16,
    bg_pattern_hi: u16,
    bg_attr_lo: u16,
    bg_attr_hi: u16,

    // Secondary-OAM result and sprite fetch state for the following scanline,
    // plus the eight active sprite shifters for the current scanline.
    secondary_oam: [u8; 32],
    secondary_indices: [u8; 8],
    secondary_count: usize,
    sprite_eval_n: usize,
    sprite_eval_m: usize,
    sprite_eval_latch: u8,
    current_sprites: [SpriteUnit; 8],
    next_sprites: [SpriteUnit; 8],

    // The PPU writes one pixel into this frame on every visible dot.
    frame: Frame,
    probe_diagnostics: ProbeDiagnostics,
}

impl NesPPU {
    pub fn new(mapper: SharedMapper) -> Self {
        NesPPU {
            mapper: mapper,
            vram: [0; 2048],
            oam_data: [0; 64 * 4],
            // Power-on palette RAM as NES black ($0F) rather than $00 (a visible
            // gray). Games that only initialize the background palettes leave the
            // sprite palettes ($3F11..) untouched; with a $00 default any unused
            // sprite (e.g. Pac-Man clears OAM to tile 0 at 0,0 on the title
            // screen) would render as a gray block. $0F keeps those invisible.
            palette_table: [0x0F; 32],
            loopy: LoopyRegister::new(),
            ctrl: ControlRegister::new(),
            mask: MaskRegister::new(),
            status: StatusRegister::new(),
            oam_addr: 0,
            rendering_enabled: false,
            pending_rendering_enabled: false,
            rendering_change_at: None,
            oam_data_bus: 0,
            internal_data_buf: 0,
            io_data_bus: 0,
            io_data_bus_refreshed_at: [0; 8],
            scanline: 0,
            dot: 0,
            odd_frame: false,
            odd_skip_armed: false,
            total_dots: 0,
            frame_ready: false,
            suppress_vblank: false,
            nmi_interrupt: None,
            nmi_interrupt_at: 0,
            next_tile_id: 0,
            next_tile_attr: 0,
            next_tile_lo: 0,
            next_tile_hi: 0,
            bg_pattern_lo: 0,
            bg_pattern_hi: 0,
            bg_attr_lo: 0,
            bg_attr_hi: 0,
            secondary_oam: [0xff; 32],
            secondary_indices: [0xff; 8],
            secondary_count: 0,
            sprite_eval_n: 0,
            sprite_eval_m: 0,
            sprite_eval_latch: 0xff,
            current_sprites: [SpriteUnit::default(); 8],
            next_sprites: [SpriteUnit::default(); 8],
            frame: Frame::new(),
            probe_diagnostics: ProbeDiagnostics::default(),
        }
    }

    // The finished frame, ready to present. Valid to read at vblank, once every
    // visible line has been composited during this frame's ticks.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Consume the host presentation event raised at the start of vblank.
    /// The CPU has not serviced the corresponding NMI yet, so a frontend can
    /// sample controller input here and make it visible to the game's vblank
    /// handler without adding an avoidable frame of latency.
    pub(crate) fn take_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.frame_ready)
    }

    pub(crate) fn clone_with_mapper(&self, mapper: SharedMapper) -> Self {
        let mut clone = self.clone();
        clone.mapper = mapper;
        clone
    }

    /// Cumulative counters used by the headless probe when an intermittent
    /// visual glitch needs to be correlated with DMA or a visible-time PPU
    /// register write. They deliberately do not affect emulation state.
    pub fn probe_diagnostics(&self) -> ProbeDiagnostics {
        self.probe_diagnostics
    }

    fn note_register_write(&mut self, register: u16) {
        if self.scanline < 240 {
            self.probe_diagnostics.visible_register_writes += 1;
            self.probe_diagnostics.last_register = register;
            self.probe_diagnostics.last_scanline = self.scanline;
            self.probe_diagnostics.last_dot = self.dot as usize;
        }
    }

    fn decay_io_data_bus(&mut self) {
        for bit in 0..8 {
            if self.io_data_bus & (1 << bit) != 0
                && self
                    .total_dots
                    .wrapping_sub(self.io_data_bus_refreshed_at[bit])
                    >= PPU_IO_BUS_DECAY_DOTS
            {
                self.io_data_bus &= !(1 << bit);
            }
        }
    }

    fn drive_io_data_bus(&mut self, value: u8, driven_bits: u8) {
        self.decay_io_data_bus();
        self.io_data_bus = (self.io_data_bus & !driven_bits) | (value & driven_bits);
        for bit in 0..8 {
            if driven_bits & (1 << bit) != 0 {
                self.io_data_bus_refreshed_at[bit] = self.total_dots;
            }
        }
    }

    /// Read a nominally write-only PPU register. No PPU circuitry drives the
    /// bus during this access, so the current (possibly decayed) latch value is
    /// returned without refreshing it.
    pub fn read_io_data_bus(&mut self) -> u8 {
        self.decay_io_data_bus();
        self.io_data_bus
    }

    pub fn tick(&mut self, cycles: u8) -> bool {
        let mut frame_complete = false;
        for _ in 0..cycles {
            frame_complete |= self.clock_dot();
        }
        frame_complete
    }

    fn rendering_enabled(&self) -> bool {
        self.rendering_enabled
    }

    fn rendering_requested(&self) -> bool {
        self.mask.show_background() || self.mask.show_sprites()
    }

    fn apply_pending_rendering_state(&mut self) {
        if self
            .rendering_change_at
            .is_some_and(|change_at| self.total_dots >= change_at)
        {
            self.rendering_enabled = self.pending_rendering_enabled;
            self.rendering_change_at = None;
        }
    }

    fn rendering_in_progress(&self) -> bool {
        (self.scanline < 240 || self.scanline == 261) && self.rendering_enabled()
    }

    // Advance one PPU dot. Rendering, fetches, scrolling and mapper-visible
    // address-bus activity all originate from this timeline.
    fn clock_dot(&mut self) -> bool {
        self.apply_pending_rendering_state();
        let render_line = self.scanline < 240 || self.scanline == 261;
        if self.scanline == 261 && self.dot == 0 {
            self.odd_skip_armed = false;
        } else if self.scanline == 261 && self.dot == 337 {
            // PPUMASK is sampled before the skipped-clock point. A rendering
            // enable that lands after this dot is too late to shorten the
            // current odd frame.
            // This model decides the later dot-339 skip here at dot 337. Use
            // the directly written PPUMASK request: by the actual skip point,
            // its internal rendering signal has crossed the transition delay.
            self.odd_skip_armed = self.odd_frame && self.rendering_requested();
        }
        if render_line && self.rendering_enabled() {
            if (1..=256).contains(&self.dot) || (321..=336).contains(&self.dot) {
                self.clock_background_pipeline();
            } else if (337..=340).contains(&self.dot) {
                // Two unused nametable fetches still drive the PPU bus.
                if self.dot == 337 || self.dot == 339 {
                    let addr = 0x2000 | (self.loopy.current() & 0x0fff);
                    let _ = self.ppu_bus_read(addr);
                }
            }

            if (1..=64).contains(&self.dot) {
                self.clock_secondary_oam_clear();
            } else if (65..=256).contains(&self.dot) {
                self.clock_sprite_evaluation();
            }
            if (257..=320).contains(&self.dot) {
                // The sprite fetch sequencer continually forces OAMADDR to
                // zero. A CPU write during this interval only survives until
                // the next PPU dot.
                self.oam_addr = 0;
                self.clock_sprite_fetch();
            } else if self.dot == 0 || (321..=340).contains(&self.dot) {
                // Outside evaluation/fetch, the first byte of secondary OAM
                // remains selected on the internal OAM bus.
                self.oam_data_bus = self.secondary_oam[0];
            }

            if self.dot == 256 {
                self.loopy.increment_y();
            }
            if self.dot == 257 {
                self.loopy.copy_horizontal();
            }
            if self.scanline == 261 && (280..=304).contains(&self.dot) {
                self.loopy.copy_vertical();
            }
        }

        // The video output continues to produce backdrop pixels while the
        // fetch/evaluation pipeline is disabled. Keeping this outside the
        // rendering block also makes mid-scanline PPUMASK transitions land on
        // the exact first affected pixel instead of leaving stale frame data.
        if self.scanline < 240 && (1..=256).contains(&self.dot) {
            self.render_pixel();
        }

        if self.scanline == 241 && self.dot == 0 {
            // All 240 visible lines are complete. Notify the host separately
            // from NMI generation so presentation and input sampling occur at
            // the real vblank boundary even when NMI is disabled.
            self.frame_ready = true;
            if !self.suppress_vblank {
                self.status.set_vblank_status(true);
                if self.ctrl.generate_vblank_nmi() {
                    self.nmi_interrupt = Some(1);
                    self.nmi_interrupt_at = self.total_dots.wrapping_add(3);
                }
            }
            self.suppress_vblank = false;
        } else if self.scanline == 261 {
            if self.dot == 0 {
                self.status.reset_vblank_status();
                self.status.set_sprite_zero_hit(false);
                self.status.set_sprite_overflow(false);
            }
        }

        // On odd rendered NTSC frames the pre-render line omits dot 340. Test
        // the skip at dot 339, but always let dot 340 end the line if rendering
        // was enabled too late; deriving a mutable `last_dot` could otherwise
        // strand the raster beyond dot 340 when PPUMASK changes at the edge.
        let frame_end =
            self.dot == 340 || (self.scanline == 261 && self.dot == 339 && self.odd_skip_armed);
        let frame_complete = if frame_end {
            if render_line && self.rendering_enabled() {
                self.current_sprites = self.next_sprites;
                self.next_sprites = [SpriteUnit::default(); 8];
            }
            self.dot = 0;
            if self.scanline == 261 {
                self.scanline = 0;
                self.odd_frame = !self.odd_frame;
                true
            } else {
                self.scanline += 1;
                false
            }
        } else {
            self.dot += 1;
            false
        };
        self.total_dots = self.total_dots.wrapping_add(1);
        frame_complete
    }

    fn clock_background_pipeline(&mut self) {
        self.bg_pattern_lo <<= 1;
        self.bg_pattern_hi <<= 1;
        self.bg_attr_lo <<= 1;
        self.bg_attr_hi <<= 1;

        match (self.dot - 1) & 7 {
            0 => {
                self.bg_pattern_lo = (self.bg_pattern_lo & 0xff00) | self.next_tile_lo as u16;
                self.bg_pattern_hi = (self.bg_pattern_hi & 0xff00) | self.next_tile_hi as u16;
                self.bg_attr_lo = (self.bg_attr_lo & 0xff00)
                    | if self.next_tile_attr & 1 != 0 { 0xff } else { 0 };
                self.bg_attr_hi = (self.bg_attr_hi & 0xff00)
                    | if self.next_tile_attr & 2 != 0 { 0xff } else { 0 };
                let addr = 0x2000 | (self.loopy.current() & 0x0fff);
                self.next_tile_id = self.ppu_bus_read(addr);
            }
            2 => {
                let v = self.loopy.current();
                let addr = 0x23c0 | (v & 0x0c00) | ((v >> 4) & 0x38) | ((v >> 2) & 0x07);
                let attr = self.ppu_bus_read(addr);
                let shift = ((v >> 4) & 4) | (v & 2);
                self.next_tile_attr = (attr >> shift) & 3;
            }
            4 => {
                let fine_y = (self.loopy.current() >> 12) & 7;
                let addr = self.ctrl.bknd_pattern_addr()
                    + self.next_tile_id as u16 * 16
                    + fine_y;
                self.next_tile_lo = self.ppu_bus_read(addr);
            }
            6 => {
                let fine_y = (self.loopy.current() >> 12) & 7;
                let addr = self.ctrl.bknd_pattern_addr()
                    + self.next_tile_id as u16 * 16
                    + fine_y
                    + 8;
                self.next_tile_hi = self.ppu_bus_read(addr);
            }
            7 => self.loopy.increment_x(),
            _ => {}
        }
    }

    fn target_sprite_scanline(&self) -> usize {
        if self.scanline == 261 { 0 } else { self.scanline as usize + 1 }
    }

    fn clock_secondary_oam_clear(&mut self) {
        if self.dot == 1 {
            self.secondary_count = 0;
            self.secondary_indices = [0xff; 8];
            self.sprite_eval_n = 0;
            self.sprite_eval_m = 0;
        }
        // The clear circuit overrides OAM reads with $FF on all 64 dots.
        self.oam_data_bus = 0xff;
        // Secondary OAM is filled with $FF over alternating read/write dots.
        // Model the externally relevant write half of each pair.
        if self.dot & 1 == 0 {
            self.secondary_oam[(self.dot / 2 - 1) as usize] = 0xff;
        }
    }

    fn clock_sprite_evaluation(&mut self) {
        if self.dot == 65 && self.oam_addr >= 8 {
            // 2C02G/H refresh bug: beginning evaluation with a nonzero OAM
            // row copies that row over sprite 0 and 1.
            let source = (self.oam_addr & 0xf8) as usize;
            self.oam_data.copy_within(source..source + 8, 0);
        }
        if self.sprite_eval_n >= 64 {
            return;
        }

        // Odd dots read primary OAM; even dots run the evaluation/write half.
        if self.dot & 1 != 0 {
            self.sprite_eval_latch =
                self.oam_data[self.sprite_eval_n * 4 + self.sprite_eval_m];
            self.oam_data_bus = self.sprite_eval_latch;
            return;
        }

        // Normally the preceding primary-OAM read remains on the bus. Once
        // secondary OAM is full, its write-disable path turns even-dot writes
        // into reads of the first selected sprite's Y coordinate.
        self.oam_data_bus = if self.secondary_count >= 8 {
            self.secondary_oam[0]
        } else {
            self.sprite_eval_latch
        };

        let line = self.target_sprite_scanline();
        let height = if self.ctrl.sprite_size_16() { 16 } else { 8 };
        if self.secondary_count < 8 {
            if self.sprite_eval_m == 0 {
                let y = self.sprite_eval_latch as usize;
                // OAM Y is the scanline before the sprite begins; subtraction
                // avoids incorrectly wrapping $FF onto scanline zero.
                let in_range = line != 0 && line > y && line <= y + height;
                if !in_range {
                    self.sprite_eval_n += 1;
                    return;
                }
                self.secondary_indices[self.secondary_count] = self.sprite_eval_n as u8;
            }

            let dst = self.secondary_count * 4 + self.sprite_eval_m;
            self.secondary_oam[dst] = self.sprite_eval_latch;
            self.sprite_eval_m += 1;
            if self.sprite_eval_m == 4 {
                self.sprite_eval_m = 0;
                self.sprite_eval_n += 1;
                self.secondary_count += 1;
            }
        } else {
            // Once secondary OAM is full, the hardware's broken increment
            // logic tests diagonal bytes from primary OAM as possible Y values.
            let candidate_y = self.sprite_eval_latch as usize;
            if line != 0 && line > candidate_y && line <= candidate_y + height {
                self.status.set_sprite_overflow(true);
            }
            self.sprite_eval_m = (self.sprite_eval_m + 1) & 3;
            self.sprite_eval_n += 1;
        }
    }

    fn sprite_pattern_addr(&self, slot: usize) -> u16 {
        if slot >= self.secondary_count {
            // Empty secondary-OAM slots contain $FF. The PPU still performs
            // their pattern fetches, and the resulting address matters to
            // A12-sensitive mappers even though no sprite pixel is produced.
            return if self.ctrl.sprite_size_16() {
                0x1ff0
            } else {
                self.ctrl.sprt_pattern_addr() + 0x0ff0
            };
        }
        let base = slot * 4;
        let y = self.secondary_oam[base] as usize;
        let tile = self.secondary_oam[base + 1];
        let attr = self.secondary_oam[base + 2];
        let height = if self.ctrl.sprite_size_16() { 16 } else { 8 };
        let mut row = self.target_sprite_scanline().saturating_sub(y + 1);
        if attr & 0x80 != 0 {
            row = height - 1 - row;
        }
        if height == 16 {
            let bank = (tile as u16 & 1) << 12;
            let tile = (tile & 0xfe).wrapping_add((row / 8) as u8);
            bank + tile as u16 * 16 + (row & 7) as u16
        } else {
            self.ctrl.sprt_pattern_addr() + tile as u16 * 16 + row as u16
        }
    }

    fn clock_sprite_fetch(&mut self) {
        let offset = self.dot - 257;
        let slot = (offset / 8) as usize;
        let phase = offset & 7;
        self.oam_data_bus = if slot < self.secondary_count {
            let byte = if phase < 4 { phase as usize } else { 3 };
            self.secondary_oam[slot * 4 + byte]
        } else if slot == self.secondary_count && slot < 8 && phase == 0 {
            // The first unused slot exposes sprite 63's Y coordinate before
            // the remaining empty-slot reads settle at $FF.
            self.oam_data[0xfc]
        } else {
            0xff
        };
        if phase == 0 {
            if slot < self.secondary_count {
                let base = slot * 4;
                self.next_sprites[slot] = SpriteUnit {
                    pattern_lo: 0,
                    pattern_hi: 0,
                    attributes: self.secondary_oam[base + 2],
                    x_counter: self.secondary_oam[base + 3],
                    oam_index: self.secondary_indices[slot],
                    valid: true,
                };
            }
            let addr = 0x2000 | (self.loopy.current() & 0x0fff);
            let _ = self.ppu_bus_read(addr);
        } else if phase == 2 {
            let addr = 0x2000 | (self.loopy.current() & 0x0fff);
            let _ = self.ppu_bus_read(addr);
        } else if phase == 4 || phase == 6 {
            let addr = self.sprite_pattern_addr(slot) + if phase == 6 { 8 } else { 0 };
            let mut value = self.ppu_bus_read(addr);
            if slot < self.secondary_count && self.secondary_oam[slot * 4 + 2] & 0x40 != 0 {
                value = value.reverse_bits();
            }
            if phase == 4 {
                self.next_sprites[slot].pattern_lo = value;
            } else {
                self.next_sprites[slot].pattern_hi = value;
            }
        }
    }

    fn render_pixel(&mut self) {
        let x = (self.dot - 1) as usize;
        let show_bg = self.rendering_enabled()
            && self.mask.show_background()
            && (x >= 8 || self.mask.leftmost_8pxl_background());
        let selector = 0x8000u16 >> self.loopy.fine_x();
        let bg_pixel = if show_bg {
            ((self.bg_pattern_hi & selector != 0) as u8) << 1
                | (self.bg_pattern_lo & selector != 0) as u8
        } else {
            0
        };
        let bg_palette = ((self.bg_attr_hi & selector != 0) as u8) << 1
            | (self.bg_attr_lo & selector != 0) as u8;

        let show_sprites = self.rendering_enabled()
            && self.mask.show_sprites()
            && (x >= 8 || self.mask.leftmost_8pxl_sprite());
        let mut sprite_pixel = 0;
        let mut sprite_palette = 0;
        let mut sprite_behind = false;
        let mut sprite_zero = false;
        if show_sprites {
            for sprite in &self.current_sprites {
                if !sprite.valid || sprite.x_counter != 0 {
                    continue;
                }
                let pixel = ((sprite.pattern_hi >> 7) & 1) << 1 | ((sprite.pattern_lo >> 7) & 1);
                if pixel != 0 {
                    sprite_pixel = pixel;
                    sprite_palette = sprite.attributes & 3;
                    sprite_behind = sprite.attributes & 0x20 != 0;
                    sprite_zero = sprite.oam_index == 0;
                    break;
                }
            }
        }

        if sprite_zero && bg_pixel != 0 && x != 255 {
            self.status.set_sprite_zero_hit(true);
        }
        let palette_index = match (bg_pixel, sprite_pixel) {
            (0, 0) => self.backdrop_palette_index(),
            (0, sp) => 0x10 + sprite_palette as usize * 4 + sp as usize,
            (bg, 0) => bg_palette as usize * 4 + bg as usize,
            (bg, _) if sprite_behind => bg_palette as usize * 4 + bg as usize,
            (_, sp) => 0x10 + sprite_palette as usize * 4 + sp as usize,
        };
        let color = self.output_color(self.palette_table[palette_index]);
        self.frame.set_pixel(x, self.scanline as usize, color);

        if self.rendering_enabled() {
            for sprite in &mut self.current_sprites {
                if !sprite.valid {
                    continue;
                }
                if sprite.x_counter > 0 {
                    sprite.x_counter -= 1;
                } else {
                    sprite.pattern_lo <<= 1;
                    sprite.pattern_hi <<= 1;
                }
            }
        }
    }

    fn backdrop_palette_index(&self) -> usize {
        // With rendering disabled, a palette address left in v is presented
        // directly as the backdrop color. Otherwise the universal background
        // entry is used, including during the short PPUMASK disable delay.
        let addr = self.loopy.current() & 0x3fff;
        if !self.rendering_enabled() && addr >= 0x3f00 {
            Self::palette_index(addr)
        } else {
            0
        }
    }

    fn output_color(&self, palette_value: u8) -> (u8, u8, u8) {
        // Greyscale is an index mask in the 2C02, not an RGB conversion.
        let palette_value = if self.mask.is_grayscale() {
            palette_value & 0x30
        } else {
            palette_value & 0x3f
        };
        let (mut red, mut green, mut blue) = SYSTEM_PALLETE[palette_value as usize];

        // The NTSC emphasis signals primarily attenuate the two channels not
        // being emphasized. Compose the three signals so combinations retain
        // their tint while an all-bits mask darkens the whole image.
        if self.mask.emphasise_red() {
            green = attenuate(green);
            blue = attenuate(blue);
        }
        if self.mask.emphasise_green() {
            red = attenuate(red);
            blue = attenuate(blue);
        }
        if self.mask.emphasise_blue() {
            red = attenuate(red);
            green = attenuate(green);
        }

        (red, green, blue)
    }

    fn palette_index(addr: u16) -> usize {
        let index = ((addr - 0x3f00) & 0x1f) as usize;
        if index >= 0x10 && index & 0x03 == 0 {
            index - 0x10
        } else {
            index
        }
    }

    fn ppu_bus_read(&mut self, addr: u16) -> u8 {
        let addr = addr & 0x3fff;
        self.mapper.borrow_mut().on_ppu_bus_access(addr, self.total_dots);
        match addr {
            0x0000..=0x1fff => self.mapper.borrow_mut().ppu_read(addr),
            0x2000..=0x3eff => self.vram[self.mirror_vram_addr(addr) as usize],
            0x3f00..=0x3fff => self.palette_table[Self::palette_index(addr)],
            _ => unreachable!(),
        }
    }

    fn ppu_bus_write(&mut self, addr: u16, value: u8) {
        let addr = addr & 0x3fff;
        self.mapper.borrow_mut().on_ppu_bus_access(addr, self.total_dots);
        match addr {
            0x0000..=0x1fff => self.mapper.borrow_mut().ppu_write(addr, value),
            0x2000..=0x3eff => {
                let mirrored = self.mirror_vram_addr(addr) as usize;
                self.vram[mirrored] = value;
            }
            0x3f00..=0x3fff => {
                self.palette_table[Self::palette_index(addr)] = value;
            }
            _ => unreachable!(),
        }
    }

    pub fn poll_nmi_interrupt(&mut self) -> Option<u8> {
        // The vblank edge is synchronized into the CPU clock domain. Keep it
        // pending immediately so a later PPUSTATUS read cannot erase an edge
        // that has already crossed that boundary, but expose it after the
        // synchronization delay (three PPU dots in this timeline).
        if self.nmi_interrupt.is_some() && self.total_dots < self.nmi_interrupt_at {
            return None;
        }
        self.nmi_interrupt.take()
    }

    #[cfg(test)]
    pub fn new_empty_rom() -> Self {
        NesPPU::new(crate::mapper::test_nrom(vec![0; 0x2000], Mirroring::Horizontal))
    }

    pub fn write_to_ppu_addr(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2006);
        if self.loopy.write_addr(value) {
            self.notify_ppu_address_bus();
        }
    }

    pub fn write_to_ctrl(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2000);
        let generated_nmi = self.ctrl.generate_vblank_nmi();
        self.ctrl.update(value);
        self.loopy.write_ctrl(value);
        // Enabling NMI during an active vblank produces an immediate NMI edge.
        if !generated_nmi
            && self.ctrl.generate_vblank_nmi()
            && self.status.is_in_vblank()
            // In this dot-at-a-time representation, state dot 0 is the
            // boundary where pre-render clearing is visible to a CPU write.
            && !(self.scanline == 261 && self.dot == 0)
        {
            self.nmi_interrupt = Some(1);
            self.nmi_interrupt_at = self.total_dots.wrapping_add(6);
            // An NMI edge caused by enabling PPUCTRL during vblank is
            // recognized after the following CPU instruction.
        } else if !self.ctrl.generate_vblank_nmi() {
            if self.total_dots < self.nmi_interrupt_at {
                self.nmi_interrupt = None;
            }
        }
    }

    pub fn write_to_mask(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2001);
        self.mask.update(value);
        let requested = self.rendering_requested();
        self.pending_rendering_enabled = requested;
        if requested == self.rendering_enabled {
            // A second write can retract a transition before it reaches the
            // renderer (a pattern used by timing tests and a few games).
            self.rendering_change_at = None;
        } else if self.rendering_change_at.is_none() {
            self.rendering_change_at =
                Some(self.total_dots.wrapping_add(PPUMASK_RENDER_DELAY_DOTS));
        }
    }

    // Reading PPUSTATUS clears vblank and resets the single shared $2005/$2006
    // write latch.
    pub fn read_status(&mut self) -> u8 {
        // PPUSTATUS drives only bits 7-5. Bits 4-0 retain their independent
        // open-bus values and decay ages.
        let status = self.status.snapshot();
        self.drive_io_data_bus(status, 0xe0);
        let data = self.io_data_bus;
        // A read immediately before vblank suppresses that frame's vblank flag
        // and NMI. A read on dot 1 clears the just-set flag/NMI below.
        if self.scanline == 241 && self.dot == 0 {
            self.suppress_vblank = true;
        }
        self.status.reset_vblank_status();
        if self.total_dots < self.nmi_interrupt_at {
            self.nmi_interrupt = None;
        }
        self.loopy.reset_latch();
        data
    }

    pub fn write_to_status(&mut self, value: u8) {
        // PPUSTATUS is read-only, but a CPU write still drives the PPU I/O bus.
        self.drive_io_data_bus(value, 0xff);
    }

    pub fn write_to_oam_addr(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2003);
        self.oam_addr = value;
    }

    pub fn write_to_oam_data(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2004);
        if self.rendering_in_progress() {
            // Rendering owns OAM. The write is discarded, while the sprite
            // evaluation address receives the characteristic +4 increment.
            self.oam_addr = self.oam_addr.wrapping_add(4);
            return;
        }
        self.oam_data[self.oam_addr as usize] = value;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    pub fn read_oam_data(&mut self) -> u8 {
        let rendering = self.rendering_in_progress();
        let mut value = if rendering {
            self.oam_data_bus
        } else {
            self.oam_data[self.oam_addr as usize]
        };
        // Attribute bytes physically implement only bits 7-5 and 1-0. The
        // unimplemented middle bits read as zero rather than open bus.
        if !rendering && self.oam_addr & 0x03 == 0x02 {
            value &= 0xe3;
        }
        self.drive_io_data_bus(value, 0xff);
        value
    }

    pub fn write_to_scroll(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2005);
        self.loopy.write_scroll(value);
    }

    // Record the start of an OAM DMA transfer for probe diagnostics. The bus
    // then streams the 256 bytes one at a time through `oam_dma_write` so the
    // reads and writes land on their real alternating CPU cycles.
    pub fn note_oam_dma_start(&mut self) {
        self.probe_diagnostics.oam_dma_count += 1;
    }

    // One byte of an OAM DMA transfer. Electrically this is a $2004 write
    // driven by the DMA unit rather than the CPU, so it lands at the current
    // OAMADDR and post-increments it, wrapping through the 256-byte page.
    pub fn oam_dma_write(&mut self, value: u8) {
        if self.rendering_in_progress() {
            // Rendering owns OAM, so the write is discarded while still
            // driving the PPU I/O latch. Across a full DMA the 256 discarded
            // +4 sprite-evaluation address bumps that $2004 applies during
            // rendering wrap back to the starting OAMADDR, so we leave it put.
            self.drive_io_data_bus(value, 0xff);
            return;
        }
        self.oam_data[self.oam_addr as usize] = value;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    // Convenience path for unit tests and callers that already hold the whole
    // page in a buffer; equivalent to 256 consecutive `oam_dma_write`s.
    #[cfg(test)]
    pub fn write_oam_dma(&mut self, data: &[u8; 256]) {
        self.note_oam_dma_start();
        for &byte in data.iter() {
            self.oam_dma_write(byte);
        }
    }

    fn increment_vram_addr(&mut self) {
        if self.rendering_in_progress() {
            // During visible and pre-render scanlines the scrolling carry
            // chain is active, so a $2007 access clocks both axes regardless
            // of PPUCTRL's linear increment selection.
            self.loopy.increment_x();
            self.loopy.increment_y();
        } else {
            self.loopy.increment(self.ctrl.vram_addr_increment());
        }
    }

    pub fn write_to_data(&mut self, value: u8) {
        self.drive_io_data_bus(value, 0xff);
        self.note_register_write(0x2007);
        let addr = self.loopy.current();
        self.ppu_bus_write(addr, value);
        self.increment_vram_addr();
        self.notify_ppu_address_bus();
    }

    pub fn read_data(&mut self) -> u8 {
        let addr = self.loopy.current() & 0x3fff;
        self.increment_vram_addr();

        let result = if addr < 0x3f00 {
            let result = self.internal_data_buf;
            self.internal_data_buf = self.ppu_bus_read(addr);
            self.drive_io_data_bus(result, 0xff);
            result
        } else {
            let palette = self.ppu_bus_read(addr);
            let palette = if self.mask.is_grayscale() {
                palette & 0x30
            } else {
                palette & 0x3f
            };
            // Palette RAM drives only six data lines. The high two retain
            // their existing values and decay ages.
            self.drive_io_data_bus(palette, 0x3f);
            let result = self.io_data_bus;
            // Palette reads are immediate but still refill the delayed buffer
            // from the mirrored nametable address beneath palette space.
            self.internal_data_buf = self.ppu_bus_read(addr - 0x1000);
            result
        };
        self.notify_ppu_address_bus();
        result
    }

    fn notify_ppu_address_bus(&mut self) {
        self.mapper
            .borrow_mut()
            .on_ppu_bus_access(self.loopy.current() & 0x3fff, self.total_dots);
    }

    // Horizontal:
    //   [ A ] [ a ]
    //   [ B ] [ b ]

    // Vertical:
    //   [ A ] [ B ]
    //   [ a ] [ b ]
    // Nametable mirroring lives on the mapper (MMC1 and friends change it at
    // runtime), so read it through the shared handle rather than a fixed field.
    pub fn mirroring(&self) -> Mirroring {
        self.mapper.borrow().mirroring()
    }

    pub fn mirror_vram_addr(&self, addr: u16) -> u16 {
        let mirrored_vram = addr & 0b10111111111111; // mirror down 0x3000-0x3eff to 0x2000 - 0x2eff
        let vram_index = mirrored_vram - 0x2000; // to vram vector
        let name_table = vram_index / 0x400; // to the name table index
        match (self.mirroring(), name_table) {
            (Mirroring::Vertical, 2) | (Mirroring::Vertical, 3) => vram_index - 0x800,
            (Mirroring::Horizontal, 2) => vram_index - 0x400,
            (Mirroring::Horizontal, 1) => vram_index - 0x400,
            (Mirroring::Horizontal, 3) => vram_index - 0x800,
            // Every nametable collapses onto one physical page.
            (Mirroring::SingleScreenLower, _) => vram_index % 0x400,
            (Mirroring::SingleScreenUpper, _) => (vram_index % 0x400) + 0x400,
            _ => vram_index,
        }
    }

    #[cfg(test)]
    pub(crate) fn render_scroll(&self) -> (u16, u8) {
        (self.loopy.current(), self.loopy.fine_x())
    }

    #[cfg(test)]
    pub(crate) fn total_dots(&self) -> u64 {
        self.total_dots
    }

    #[cfg(test)]
    pub fn sync_scroll_for_test(&mut self) {
        self.loopy.copy_all_for_test();
    }
}

fn attenuate(channel: u8) -> u8 {
    ((channel as u16 * 3) / 4) as u8
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::mapper::Mapper;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct RecordingMapper {
        chr: Vec<u8>,
        accesses: Rc<RefCell<Vec<(u16, u64)>>>,
    }

    impl Mapper for RecordingMapper {
        fn cpu_read(&mut self, _addr: u16) -> u8 {
            0
        }

        fn cpu_write(&mut self, _addr: u16, _data: u8) {}

        fn ppu_read(&mut self, addr: u16) -> u8 {
            self.chr[addr as usize]
        }

        fn ppu_write(&mut self, addr: u16, data: u8) {
            self.chr[addr as usize] = data;
        }

        fn mirroring(&self) -> Mirroring {
            Mirroring::Vertical
        }

        fn on_ppu_bus_access(&mut self, addr: u16, ppu_cycle: u64) {
            self.accesses.borrow_mut().push((addr, ppu_cycle));
        }
    }

    fn recording_ppu() -> (NesPPU, Rc<RefCell<Vec<(u16, u64)>>>) {
        let accesses = Rc::new(RefCell::new(Vec::new()));
        let mapper = RecordingMapper {
            chr: vec![0; 0x2000],
            accesses: Rc::clone(&accesses),
        };
        let mapper = Rc::new(RefCell::new(Box::new(mapper) as Box<dyn Mapper>));
        (NesPPU::new(mapper), accesses)
    }

    // Most dot-phase tests start the raster at an arbitrary position and are
    // concerned with the pipeline once it is already active. Keep those tests
    // independent of PPUMASK's transition delay; dedicated tests below cover
    // the delay itself.
    fn force_mask(ppu: &mut NesPPU, value: u8) {
        ppu.write_to_mask(value);
        let rendering = ppu.mask.show_background() || ppu.mask.show_sprites();
        ppu.rendering_enabled = rendering;
        ppu.pending_rendering_enabled = rendering;
        ppu.rendering_change_at = None;
    }

    fn mmc3_ppu() -> (NesPPU, crate::mapper::SharedMapper) {
        let mapper = crate::mapper::from_rom(crate::cartridge::Rom {
            // MMC3 fixes its final two 8 KB banks, so give it a conventional
            // 32 KB PRG image even though this timing test never reads it.
            prg_rom: vec![0; 0x8000],
            chr_rom: vec![0; 0x2000],
            mapper: 4,
            screen_mirroring: Mirroring::Vertical,
        });
        (NesPPU::new(mapper.clone()), mapper)
    }

    // Phase 2 integration: the PPU, not the CPU or renderer, is responsible
    // for clocking MMC3's scanline IRQ. A disabled screen must not create A12
    // edges, while the first visible rendered line must clock the configured
    // counter and expose the IRQ through the shared mapper.
    #[test]
    fn rendered_scanline_clocks_mmc3_irq_but_blank_scanline_does_not() {
        let (mut ppu, mapper) = mmc3_ppu();
        {
            let mut mapper = mapper.borrow_mut();
            mapper.cpu_write(0xc000, 0); // latch zero: assert on the first clock
            mapper.cpu_write(0xc001, 0); // request reload
            mapper.cpu_write(0xe001, 0); // enable IRQ
        }

        // Cross scanline 0 with rendering disabled. `tick` accepts a u8, so
        // use two calls to reach its 341 PPU-cycle boundary.
        ppu.tick(255);
        ppu.tick(86);
        assert!(!mapper.borrow().irq_pending());

        // Cross scanline 1 with background rendering enabled: the PPU clocks
        // MMC3 once from the sprite-table A12 edge and holds its IRQ line high.
        ppu.write_to_ctrl(0x08); // sprites at $1000, background at $0000
        ppu.write_to_mask(0b0000_1000);
        ppu.tick(255);
        ppu.tick(86);
        assert!(mapper.borrow().irq_pending());
    }

    #[test]
    fn mmc3_a12_edges_follow_background_and_sprite_pattern_table_selection() {
        for (ctrl, expected_irq) in [(0x00, false), (0x08, true), (0x10, true), (0x18, true)] {
            let (mut ppu, mapper) = mmc3_ppu();
            {
                let mut mapper = mapper.borrow_mut();
                mapper.cpu_write(0xc000, 0);
                mapper.cpu_write(0xc001, 0);
                mapper.cpu_write(0xe001, 0);
            }
            ppu.write_to_ctrl(ctrl);
            ppu.write_to_mask(0x18);

            // Two lines cover both intra-line table transitions and the
            // qualified edge after the dummy-fetch low period at line end.
            for _ in 0..2 {
                ppu.tick(255);
                ppu.tick(86);
            }

            assert_eq!(
                mapper.borrow().irq_pending(),
                expected_irq,
                "PPUCTRL pattern-table selection {ctrl:#04x}"
            );
        }
    }

    #[test]
    fn ppudata_accesses_are_visible_to_the_mmc3_a12_filter() {
        let (mut ppu, mapper) = mmc3_ppu();
        {
            let mut mapper = mapper.borrow_mut();
            mapper.cpu_write(0xc000, 0);
            mapper.cpu_write(0xc001, 0);
            mapper.cpu_write(0xe001, 0);
        }

        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_data(0);
        ppu.tick(8);
        ppu.write_to_ppu_addr(0x10);
        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_data(0);

        assert!(mapper.borrow().irq_pending());
    }

    #[test]
    fn ppuaddr_write_can_clock_mmc3_from_a12_low_to_high() {
        let (mut ppu, mapper) = mmc3_ppu();
        {
            let mut mapper = mapper.borrow_mut();
            mapper.cpu_write(0xc000, 0);
            mapper.cpu_write(0xc001, 0);
            mapper.cpu_write(0xe001, 0);
        }

        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_ppu_addr(0x00);
        ppu.tick(8);
        ppu.write_to_ppu_addr(0x10);
        ppu.write_to_ppu_addr(0x00);

        assert!(mapper.borrow().irq_pending());
    }

    #[test]
    fn ppudata_increment_across_0fff_clocks_mmc3_a12() {
        for read in [false, true] {
            let (mut ppu, mapper) = mmc3_ppu();
            {
                let mut mapper = mapper.borrow_mut();
                mapper.cpu_write(0xc000, 0);
                mapper.cpu_write(0xc001, 0);
                mapper.cpu_write(0xe001, 0);
            }
            ppu.write_to_ppu_addr(0x0f);
            ppu.write_to_ppu_addr(0xff);
            ppu.tick(8);

            if read {
                ppu.read_data();
            } else {
                ppu.write_to_data(0);
            }

            assert!(mapper.borrow().irq_pending(), "PPUDATA read={read}");
        }
    }

    #[test]
    fn vblank_starts_at_scanline_241_dot_1() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.scanline = 241;
        ppu.dot = 0;
        ppu.write_to_ctrl(0x80);

        ppu.tick(1); // enter dot 1
        assert!(ppu.status.is_in_vblank());
        assert_eq!(ppu.poll_nmi_interrupt(), None);

        ppu.tick(2); // synchronize the NMI edge into the CPU domain
        assert_eq!(ppu.poll_nmi_interrupt(), Some(1));
    }

    #[test]
    fn status_read_just_before_vblank_suppresses_flag_and_nmi() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.scanline = 241;
        ppu.dot = 0;
        ppu.write_to_ctrl(0x80);

        ppu.read_status();
        ppu.tick(2); // process dots 0 and 1

        assert!(!ppu.status.is_in_vblank());
        assert_eq!(ppu.poll_nmi_interrupt(), None);
    }

    #[test]
    fn pre_render_dot_one_clears_all_rendering_status_flags() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);
        ppu.status.set_sprite_zero_hit(true);
        ppu.status.set_sprite_overflow(true);
        ppu.scanline = 261;
        ppu.dot = 0;

        ppu.tick(1);

        assert_eq!(ppu.status.snapshot() & 0xe0, 0);
    }

    #[test]
    fn enabling_nmi_during_vblank_raises_an_edge_after_the_next_instruction() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);

        ppu.write_to_ctrl(0x80);

        assert_eq!(ppu.poll_nmi_interrupt(), None);
        ppu.tick(6);
        assert_eq!(ppu.poll_nmi_interrupt(), Some(1));
    }

    #[test]
    fn ppumask_rendering_ownership_changes_after_four_complete_dots() {
        let mut ppu = NesPPU::new_empty_rom();

        ppu.write_to_mask(0x08);
        assert!(!ppu.rendering_enabled());
        ppu.tick(1);
        assert!(!ppu.rendering_enabled());
        ppu.tick(1);
        assert!(!ppu.rendering_enabled());
        ppu.tick(1);
        assert!(!ppu.rendering_enabled());
        ppu.tick(1);
        assert!(!ppu.rendering_enabled());
        ppu.tick(1);
        assert!(ppu.rendering_enabled());

        ppu.write_to_mask(0x00);
        ppu.tick(1);
        assert!(ppu.rendering_enabled());
        ppu.tick(1);
        assert!(ppu.rendering_enabled());
        ppu.tick(1);
        assert!(ppu.rendering_enabled());
        ppu.tick(1);
        assert!(ppu.rendering_enabled());
        ppu.tick(1);
        assert!(!ppu.rendering_enabled());
    }

    #[test]
    fn ppumask_transition_can_be_retracted_before_it_reaches_the_renderer() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_mask(0x08);
        ppu.tick(1);
        ppu.write_to_mask(0x00);

        ppu.tick(3);

        assert!(!ppu.rendering_enabled());
        assert_eq!(ppu.rendering_change_at, None);
    }

    #[test]
    fn left_background_clip_changes_on_the_first_pixel_after_the_write() {
        let mut ppu = NesPPU::new_empty_rom();
        force_mask(&mut ppu, 0x08); // background on, left column clipped
        ppu.palette_table[0] = 0x0f;
        ppu.palette_table[1] = 0x30;

        ppu.bg_pattern_lo = 0x4000;
        ppu.dot = 1;
        ppu.tick(1);

        ppu.write_to_mask(0x0a); // expose background in the left column
        ppu.bg_pattern_lo = 0x4000;
        ppu.tick(1);

        ppu.write_to_mask(0x08); // clip it again on the following pixel
        ppu.bg_pattern_lo = 0x4000;
        ppu.tick(1);

        let backdrop = SYSTEM_PALLETE[0x0f];
        let foreground = SYSTEM_PALLETE[0x30];
        assert_eq!(
            &ppu.frame().data[0..3],
            &[backdrop.0, backdrop.1, backdrop.2]
        );
        assert_eq!(
            &ppu.frame().data[3..6],
            &[foreground.0, foreground.1, foreground.2]
        );
        assert_eq!(
            &ppu.frame().data[6..9],
            &[backdrop.0, backdrop.1, backdrop.2]
        );
    }

    #[test]
    fn blanked_output_uses_palette_address_left_in_v() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.palette_table[5] = 0x30;
        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x05);
        ppu.dot = 1;

        ppu.tick(1);

        let expected = SYSTEM_PALLETE[0x30];
        assert_eq!(
            &ppu.frame().data[0..3],
            &[expected.0, expected.1, expected.2]
        );
    }

    #[test]
    fn status_read_on_vblank_set_dot_does_not_poison_the_next_frame() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0x80);
        ppu.scanline = 241;
        ppu.dot = 0;
        ppu.tick(1);

        assert_eq!(ppu.read_status() & 0x80, 0x80);
        assert!(!ppu.suppress_vblank);
        assert_eq!(ppu.poll_nmi_interrupt(), None);

        // Jump to the next frame's set point. The post-set read above must not
        // be mistaken for a pre-set read and suppress this event as well.
        ppu.scanline = 241;
        ppu.dot = 0;
        ppu.tick(1);
        assert!(ppu.status.is_in_vblank());
    }

    #[test]
    fn status_read_suppression_window_ends_two_dots_after_vblank_set() {
        let mut suppressed = NesPPU::new_empty_rom();
        suppressed.write_to_ctrl(0x80);
        suppressed.scanline = 241;
        suppressed.dot = 0;
        suppressed.tick(1); // vblank set
        suppressed.tick(1); // one dot after set
        assert_eq!(suppressed.read_status() & 0x80, 0x80);
        suppressed.tick(2);
        assert_eq!(suppressed.poll_nmi_interrupt(), None);

        let mut recognized = NesPPU::new_empty_rom();
        recognized.write_to_ctrl(0x80);
        recognized.scanline = 241;
        recognized.dot = 0;
        recognized.tick(1); // vblank set
        recognized.tick(2); // two dots after set: CPU has recognized the edge
        assert_eq!(recognized.read_status() & 0x80, 0x80);
        assert_eq!(recognized.poll_nmi_interrupt(), Some(1));
    }

    #[test]
    fn disabling_nmi_only_cancels_an_edge_before_cpu_recognition() {
        let mut cancelled = NesPPU::new_empty_rom();
        cancelled.write_to_ctrl(0x80);
        cancelled.scanline = 241;
        cancelled.dot = 0;
        cancelled.tick(1);
        cancelled.tick(1); // one dot after vblank set
        cancelled.write_to_ctrl(0x00);
        cancelled.tick(2);
        assert_eq!(cancelled.poll_nmi_interrupt(), None);

        let mut recognized = NesPPU::new_empty_rom();
        recognized.write_to_ctrl(0x80);
        recognized.scanline = 241;
        recognized.dot = 0;
        recognized.tick(1);
        recognized.tick(2); // recognition point
        recognized.write_to_ctrl(0x00);
        assert_eq!(recognized.poll_nmi_interrupt(), Some(1));
    }

    #[test]
    fn toggling_ppuctrl_can_generate_multiple_immediate_nmis_in_one_vblank() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);

        ppu.write_to_ctrl(0x80);
        let first_edge_at = ppu.nmi_interrupt_at;
        ppu.write_to_ctrl(0x80);
        assert_eq!(ppu.nmi_interrupt_at, first_edge_at);
        ppu.tick(6);
        assert_eq!(ppu.poll_nmi_interrupt(), Some(1));

        ppu.write_to_ctrl(0x00);
        ppu.write_to_ctrl(0x80);
        ppu.tick(6);
        assert_eq!(ppu.poll_nmi_interrupt(), Some(1));
    }

    #[test]
    fn sprite_zero_hit_is_asserted_at_the_overlap_dot() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_mask(0b0001_1110);
        ppu.vram[0..33].fill(1);
        // Tile 1 is opaque on sprite row zero and background row one.
        ppu.mapper.borrow_mut().ppu_write(16, 0xff);
        ppu.mapper.borrow_mut().ppu_write(17, 0xff);
        ppu.oam_data[0..4].copy_from_slice(&[0, 1, 0, 8]);
        // Run the pre-render line and scanline zero so the hardware fetch and
        // sprite-evaluation pipelines prepare scanline one.
        ppu.scanline = 261;
        ppu.dot = 0;
        ppu.tick(255);
        ppu.tick(86);
        ppu.tick(255);
        ppu.tick(86);

        ppu.tick(9); // process dots 0..=8 on scanline one
        assert_eq!(ppu.status.snapshot() & 0x40, 0);
        ppu.tick(1); // sprite x=8 becomes visible on PPU dot 9
        assert_eq!(ppu.status.snapshot() & 0x40, 0x40);
    }

    #[test]
    fn sprite_zero_hit_obeys_left_clipping_and_never_fires_at_x255() {
        let mut ppu = NesPPU::new_empty_rom();
        let sprite = SpriteUnit {
            pattern_lo: 0x80,
            pattern_hi: 0,
            attributes: 0,
            x_counter: 0,
            oam_index: 0,
            valid: true,
        };

        force_mask(&mut ppu, 0x18); // rendering on, left eight pixels clipped
        // The background pipeline shifts immediately before composing a dot.
        ppu.bg_pattern_lo = 0x4000;
        ppu.current_sprites[0] = sprite;
        ppu.dot = 1;
        ppu.tick(1);
        assert_eq!(ppu.status.snapshot() & 0x40, 0);

        ppu.write_to_mask(0x1e); // show background and sprites in left edge
        ppu.bg_pattern_lo = 0x4000;
        ppu.current_sprites[0] = sprite;
        ppu.dot = 1;
        ppu.tick(1);
        assert_eq!(ppu.status.snapshot() & 0x40, 0x40);

        ppu.status.set_sprite_zero_hit(false);
        ppu.bg_pattern_lo = 0x4000;
        ppu.current_sprites[0] = sprite;
        ppu.dot = 256;
        ppu.tick(1);
        assert_eq!(ppu.status.snapshot() & 0x40, 0);
    }

    #[test]
    fn background_fetches_follow_the_eight_dot_bus_sequence() {
        let mut ppu = NesPPU::new_empty_rom();
        force_mask(&mut ppu, 0x08);
        ppu.vram[0] = 2;
        ppu.vram[0x03c0] = 0b11;
        ppu.mapper.borrow_mut().ppu_write(32, 0xaa);
        ppu.mapper.borrow_mut().ppu_write(40, 0x55);
        ppu.dot = 1;

        ppu.tick(1);
        assert_eq!(ppu.next_tile_id, 2);
        ppu.tick(2);
        assert_eq!(ppu.next_tile_attr, 3);
        ppu.tick(2);
        assert_eq!(ppu.next_tile_lo, 0xaa);
        ppu.tick(2);
        assert_eq!(ppu.next_tile_hi, 0x55);
        ppu.tick(1);
        assert_eq!(ppu.loopy.current() & 0x1f, 1);
    }

    #[test]
    fn background_fetch_phases_drive_the_expected_addresses_and_dots() {
        let (mut ppu, accesses) = recording_ppu();
        force_mask(&mut ppu, 0x08);
        ppu.vram[0] = 2;
        ppu.dot = 1;

        ppu.tick(8);

        assert_eq!(
            *accesses.borrow(),
            vec![(0x2000, 0), (0x23c0, 2), (0x0020, 4), (0x0028, 6)]
        );
        assert_eq!(ppu.loopy.current() & 0x001f, 1);
    }

    #[test]
    fn prefetch_and_dummy_fetches_drive_the_ppu_bus_on_dots_321_through_339() {
        let (mut ppu, accesses) = recording_ppu();
        force_mask(&mut ppu, 0x08);
        ppu.dot = 321;

        ppu.tick(20);

        assert_eq!(
            *accesses.borrow(),
            vec![
                (0x2000, 0),
                (0x23c0, 2),
                (0x0000, 4),
                (0x0008, 6),
                (0x2001, 8),
                (0x23c0, 10),
                (0x0000, 12),
                (0x0008, 14),
                (0x2002, 16),
                (0x2002, 18),
            ]
        );
    }

    #[test]
    fn sprite_fetch_slot_drives_two_garbage_and_two_pattern_accesses() {
        let (mut ppu, accesses) = recording_ppu();
        ppu.write_to_ctrl(0x08); // 8x8 sprite patterns at $1000
        force_mask(&mut ppu, 0x10);
        ppu.secondary_count = 1;
        ppu.secondary_indices[0] = 0;
        ppu.secondary_oam[0..4].copy_from_slice(&[0, 2, 0, 0]);
        ppu.dot = 257;

        ppu.tick(8);

        assert_eq!(
            *accesses.borrow(),
            vec![(0x2000, 0), (0x2000, 2), (0x1020, 4), (0x1028, 6)]
        );
        assert!(ppu.next_sprites[0].valid);
    }

    #[test]
    fn empty_sprite_slots_still_fetch_from_the_selected_pattern_table() {
        let (mut ppu, accesses) = recording_ppu();
        ppu.write_to_ctrl(0x08);
        force_mask(&mut ppu, 0x10);
        ppu.dot = 257;

        ppu.tick(8);

        assert_eq!(
            *accesses.borrow(),
            vec![(0x2000, 0), (0x2000, 2), (0x1ff0, 4), (0x1ff8, 6)]
        );
    }

    #[test]
    fn rendering_disabled_suppresses_all_pipeline_bus_accesses() {
        let (mut ppu, accesses) = recording_ppu();

        ppu.tick(255);
        ppu.tick(86);

        assert!(accesses.borrow().is_empty());
    }

    #[test]
    fn first_visible_pixel_comes_from_pre_render_fetches() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_mask(0x0a);
        ppu.palette_table[1] = 0x30;
        ppu.vram[0] = 1;
        ppu.mapper.borrow_mut().ppu_write(16, 0x80);
        ppu.scanline = 261;

        ppu.tick(255);
        ppu.tick(86);
        ppu.tick(2); // scanline zero dots 0 and 1

        let expected = SYSTEM_PALLETE[0x30];
        assert_eq!(&ppu.frame().data[0..3], &[expected.0, expected.1, expected.2]);
    }

    #[test]
    fn sprite_evaluation_selects_eight_and_sets_overflow() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_mask(0x10);
        for sprite in 0..9 {
            ppu.oam_data[sprite * 4..sprite * 4 + 4]
                .copy_from_slice(&[0, 1, 0, (sprite * 8) as u8]);
        }
        ppu.dot = 65;
        ppu.tick(192);
        assert_eq!(ppu.secondary_count, 8);
        assert_ne!(ppu.status.snapshot() & 0x20, 0);
    }

    #[test]
    fn loopy_scroll_copies_happen_on_their_hardware_dots() {
        let mut ppu = NesPPU::new_empty_rom();
        force_mask(&mut ppu, 0x08);

        // Establish v=0, then change only temporary horizontal scroll.
        ppu.write_to_scroll(0);
        ppu.write_to_scroll(0);
        ppu.sync_scroll_for_test();
        ppu.read_status();
        ppu.write_to_scroll(0x28); // t coarse X = 5
        ppu.scanline = 0;
        ppu.dot = 256;
        ppu.tick(1);
        assert_eq!(ppu.loopy.current() & 0x001f, 1);
        ppu.tick(1);
        assert_eq!(ppu.loopy.current() & 0x001f, 5);

        // Change temporary vertical scroll; it is copied during pre-render
        // dots 280-304, not at the frame boundary.
        ppu.read_status();
        ppu.write_to_scroll(0);
        ppu.write_to_scroll(0x28); // t coarse Y = 5
        ppu.scanline = 261;
        ppu.dot = 279;
        ppu.tick(1);
        assert_ne!((ppu.loopy.current() >> 5) & 0x1f, 5);
        ppu.tick(1);
        assert_eq!((ppu.loopy.current() >> 5) & 0x1f, 5);
    }

    #[test]
    fn odd_rendered_frame_skips_pre_render_dot_340() {
        let mut ppu = NesPPU::new_empty_rom();
        force_mask(&mut ppu, 0x08);
        ppu.scanline = 261;
        ppu.dot = 0;

        let mut even_dots = 0;
        while !ppu.tick(1) {
            even_dots += 1;
        }
        even_dots += 1;
        assert_eq!(even_dots, 341);

        ppu.scanline = 261;
        ppu.dot = 0;
        let mut odd_dots = 0;
        while !ppu.tick(1) {
            odd_dots += 1;
        }
        odd_dots += 1;
        assert_eq!(odd_dots, 340);
    }

    #[test]
    fn odd_blank_frame_keeps_pre_render_dot_340() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.scanline = 261;
        ppu.dot = 0;
        ppu.odd_frame = true;

        let mut dots = 0;
        while !ppu.tick(1) {
            dots += 1;
        }
        dots += 1;

        assert_eq!(dots, 341);
    }

    #[test]
    fn test_ppu_vram_writes() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ppu_addr(0x23);
        ppu.write_to_ppu_addr(0x05);
        ppu.write_to_data(0x66);

        assert_eq!(ppu.vram[0x0305], 0x66);
    }

    #[test]
    fn nametable_space_3000_through_3eff_mirrors_2000_through_2eff() {
        let mut ppu = NesPPU::new_empty_rom();

        ppu.write_to_ppu_addr(0x30);
        ppu.write_to_ppu_addr(0x05);
        ppu.write_to_data(0x66);
        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x05);
        ppu.read_data();
        assert_eq!(ppu.read_data(), 0x66);

        ppu.write_to_ppu_addr(0x2e);
        ppu.write_to_ppu_addr(0xff);
        ppu.write_to_data(0x77);
        ppu.write_to_ppu_addr(0x3e);
        ppu.write_to_ppu_addr(0xff);
        ppu.read_data();
        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x00);
        assert_eq!(ppu.read_data(), 0x77);
    }

    #[test]
    fn palette_universal_background_entries_alias_on_reads_and_writes() {
        let mut ppu = NesPPU::new_empty_rom();

        for (offset, value) in [(0x00, 0x01), (0x04, 0x12), (0x08, 0x23), (0x0c, 0x34)] {
            ppu.write_to_ppu_addr(0x3f);
            ppu.write_to_ppu_addr(offset);
            ppu.write_to_data(value);

            ppu.write_to_ppu_addr(0x3f);
            ppu.write_to_ppu_addr(0x10 + offset);
            assert_eq!(ppu.read_data(), value);
        }
    }

    #[test]
    fn palette_ram_mirrors_through_3fff() {
        let mut ppu = NesPPU::new_empty_rom();

        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_data(0x21);
        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x00);
        assert_eq!(ppu.read_data(), 0x21);

        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0xff);
        ppu.write_to_data(0x32);
        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x1f);
        assert_eq!(ppu.read_data(), 0x32);
    }

    #[test]
    fn palette_read_refills_buffer_from_underlying_nametable() {
        let mut ppu = NesPPU::new_empty_rom();
        let underlying = ppu.mirror_vram_addr(0x2f05) as usize;
        ppu.vram[underlying] = 0x5a;
        ppu.palette_table[5] = 0x2a;

        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x05);
        assert_eq!(ppu.read_data(), 0x2a);

        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x00);
        assert_eq!(ppu.read_data(), 0x5a);
    }

    #[test]
    fn palette_read_combines_six_bit_value_with_ppu_io_bus_high_bits() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.palette_table[0] = 0x2a;

        // The low address write is the last value on the I/O bus. $3FC0
        // mirrors $3F00 and leaves both high bits set for the palette read.
        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0xc0);
        assert_eq!(ppu.read_data(), 0xea);
    }

    #[test]
    fn grayscale_masks_palette_reads_and_pixel_palette_indices() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.palette_table[1] = 0x2f;
        ppu.write_to_mask(0x01);

        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x01);
        assert_eq!(ppu.read_data(), 0x20);
        assert_eq!(ppu.output_color(0x2f), SYSTEM_PALLETE[0x20]);
    }

    #[test]
    fn emphasis_attenuates_the_other_rgb_channels() {
        let mut ppu = NesPPU::new_empty_rom();

        ppu.write_to_mask(0x20);
        assert_eq!(ppu.output_color(0x30), (0xff, 0xbf, 0xbf));
        ppu.write_to_mask(0x40);
        assert_eq!(ppu.output_color(0x30), (0xbf, 0xff, 0xbf));
        ppu.write_to_mask(0x80);
        assert_eq!(ppu.output_color(0x30), (0xbf, 0xbf, 0xff));
    }

    #[test]
    fn ppustatus_preserves_low_bits_from_the_ppu_io_bus() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);
        ppu.write_to_status(0x1b);

        assert_eq!(ppu.read_status(), 0x9b);
    }

    #[test]
    fn write_only_register_reads_do_not_refresh_the_io_bus() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0xa5);

        ppu.total_dots = PPU_IO_BUS_DECAY_DOTS - 1;
        assert_eq!(ppu.read_io_data_bus(), 0xa5);
        ppu.total_dots += 1;
        assert_eq!(ppu.read_io_data_bus(), 0x00);
    }

    #[test]
    fn ppustatus_refreshes_only_the_three_status_bits() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_status(0x1f);
        ppu.status.set_vblank_status(true);

        ppu.total_dots = PPU_IO_BUS_DECAY_DOTS - 1;
        assert_eq!(ppu.read_status(), 0x9f);
        ppu.total_dots += 1;
        assert_eq!(ppu.read_io_data_bus(), 0x80);
    }

    #[test]
    fn palette_reads_refresh_only_the_six_palette_bits() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.palette_table[0] = 0x2a;
        ppu.write_to_ppu_addr(0x3f);
        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_status(0xc0);

        ppu.total_dots = PPU_IO_BUS_DECAY_DOTS - 1;
        assert_eq!(ppu.read_data(), 0xea);

        ppu.total_dots += 1;
        assert_eq!(ppu.read_io_data_bus(), 0x2a);
    }

    #[test]
    fn oam_attribute_reads_clear_unimplemented_bits() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_oam_addr(0x02);
        ppu.write_to_oam_data(0xff);
        ppu.write_to_oam_addr(0x02);

        assert_eq!(ppu.read_oam_data(), 0xe3);
        assert_eq!(ppu.read_io_data_bus(), 0xe3);
    }

    #[test]
    fn oamdata_reads_follow_the_internal_bus_during_rendering() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.oam_data[0] = 0x23;
        ppu.oam_data[1] = 0x45;
        force_mask(&mut ppu, 0x08);

        ppu.dot = 1;
        ppu.tick(1);
        assert_eq!(ppu.read_oam_data(), 0xff);

        ppu.dot = 65;
        ppu.tick(1);
        assert_eq!(ppu.read_oam_data(), 0x23);

        ppu.secondary_count = 1;
        ppu.secondary_oam[0..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        ppu.dot = 257;
        ppu.tick(1);
        assert_eq!(ppu.read_oam_data(), 0x12);
        ppu.tick(1);
        assert_eq!(ppu.read_oam_data(), 0x34);
    }

    #[test]
    fn oamdata_writes_during_rendering_are_discarded_and_increment_by_four() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.oam_data[0x10] = 0x55;
        ppu.write_to_oam_addr(0x10);
        force_mask(&mut ppu, 0x10);

        ppu.write_to_oam_data(0xaa);

        assert_eq!(ppu.oam_data[0x10], 0x55);
        assert_eq!(ppu.oam_addr, 0x14);
    }

    #[test]
    fn sprite_fetch_resets_oamaddr_and_evaluation_applies_refresh_corruption() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.oam_data[0x20..0x28].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        force_mask(&mut ppu, 0x08);
        ppu.write_to_oam_addr(0x23);
        ppu.dot = 65;

        ppu.tick(1);
        assert_eq!(&ppu.oam_data[0..8], &[1, 2, 3, 4, 5, 6, 7, 8]);

        ppu.write_to_oam_addr(0x80);
        ppu.dot = 257;
        ppu.tick(1);
        assert_eq!(ppu.oam_addr, 0);
    }

    #[test]
    fn ppudata_access_during_rendering_increments_both_scroll_axes() {
        for read in [false, true] {
            let mut ppu = NesPPU::new_empty_rom();
            ppu.write_to_ctrl(0x04); // +32 is ignored while rendering.
            ppu.write_to_ppu_addr(0x00);
            ppu.write_to_ppu_addr(0x00);
            force_mask(&mut ppu, 0x08);
            ppu.scanline = 100;
            ppu.dot = 100;

            if read {
                ppu.read_data();
            } else {
                ppu.write_to_data(0x5a);
            }

            assert_eq!(ppu.loopy.current(), 0x1001, "read={read}");
        }
    }

    #[test]
    fn ppudata_access_during_vblank_keeps_the_linear_increment() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0x04);
        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x00);
        ppu.write_to_mask(0x08);
        ppu.scanline = 241;
        ppu.dot = 20;

        ppu.read_data();

        assert_eq!(ppu.loopy.current(), 0x2020);
    }

    #[test]
    fn test_ppu_vram_reads() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0);
        ppu.vram[0x0305] = 0x66;

        ppu.write_to_ppu_addr(0x23);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load_into_buffer
        assert_eq!(ppu.loopy.current(), 0x2306);
        assert_eq!(ppu.read_data(), 0x66);
    }

    #[test]
    fn test_ppu_vram_reads_cross_page() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0);
        ppu.vram[0x01ff] = 0x66;
        ppu.vram[0x0200] = 0x77;

        ppu.write_to_ppu_addr(0x21);
        ppu.write_to_ppu_addr(0xff);

        ppu.read_data(); //load_into_buffer
        assert_eq!(ppu.read_data(), 0x66);
        assert_eq!(ppu.read_data(), 0x77);
    }

    #[test]
    fn test_ppu_vram_reads_step_32() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0b100);
        ppu.vram[0x01ff] = 0x66;
        ppu.vram[0x01ff + 32] = 0x77;
        ppu.vram[0x01ff + 64] = 0x88;

        ppu.write_to_ppu_addr(0x21);
        ppu.write_to_ppu_addr(0xff);

        ppu.read_data(); //load_into_buffer
        assert_eq!(ppu.read_data(), 0x66);
        assert_eq!(ppu.read_data(), 0x77);
        assert_eq!(ppu.read_data(), 0x88);
    }

    // Horizontal:
    //   [0x2000 A ] [0x2400 a ]
    //   [0x2800 B ] [0x2C00 b ]
    #[test]
    fn test_vram_horizontal_mirror() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ppu_addr(0x24);
        ppu.write_to_ppu_addr(0x05);

        ppu.write_to_data(0x66); //write to a

        ppu.write_to_ppu_addr(0x28);
        ppu.write_to_ppu_addr(0x05);

        ppu.write_to_data(0x77); //write to B

        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load into buffer
        assert_eq!(ppu.read_data(), 0x66); //read from A

        ppu.write_to_ppu_addr(0x2C);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load into buffer
        assert_eq!(ppu.read_data(), 0x77); //read from b
    }

    // Vertical:
    //   [0x2000 A ] [0x2400 B ]
    //   [0x2800 a ] [0x2C00 b ]
    #[test]
    fn test_vram_vertical_mirror() {
        let mut ppu = NesPPU::new(crate::mapper::test_nrom(vec![0; 0x2000], Mirroring::Vertical));

        ppu.write_to_ppu_addr(0x20);
        ppu.write_to_ppu_addr(0x05);

        ppu.write_to_data(0x66); //write to A

        ppu.write_to_ppu_addr(0x2C);
        ppu.write_to_ppu_addr(0x05);

        ppu.write_to_data(0x77); //write to b

        ppu.write_to_ppu_addr(0x28);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load into buffer
        assert_eq!(ppu.read_data(), 0x66); //read from a

        ppu.write_to_ppu_addr(0x24);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load into buffer
        assert_eq!(ppu.read_data(), 0x77); //read from B
    }

    #[test]
    fn test_read_status_resets_latch() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.vram[0x0305] = 0x66;

        ppu.write_to_ppu_addr(0x21);
        ppu.write_to_ppu_addr(0x23);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load_into_buffer
        assert_ne!(ppu.read_data(), 0x66);

        ppu.read_status();

        ppu.write_to_ppu_addr(0x23);
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load_into_buffer
        assert_eq!(ppu.read_data(), 0x66);
    }

    #[test]
    fn test_ppu_vram_mirroring() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ctrl(0);
        ppu.vram[0x0305] = 0x66;

        ppu.write_to_ppu_addr(0x63); //0x6305 -> 0x2305
        ppu.write_to_ppu_addr(0x05);

        ppu.read_data(); //load into_buffer
        assert_eq!(ppu.read_data(), 0x66);
    }

    #[test]
    fn test_read_status_resets_vblank() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);

        let status = ppu.read_status();

        assert_eq!(status >> 7, 1);
        assert_eq!(ppu.status.snapshot() >> 7, 0);
    }

    #[test]
    fn test_oam_read_write() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_oam_addr(0x10);
        ppu.write_to_oam_data(0x66);
        ppu.write_to_oam_data(0x77);

        ppu.write_to_oam_addr(0x10);
        assert_eq!(ppu.read_oam_data(), 0x66);

        ppu.write_to_oam_addr(0x11);
        assert_eq!(ppu.read_oam_data(), 0x77);
    }

    #[test]
    fn test_oam_dma() {
        let mut ppu = NesPPU::new_empty_rom();

        let mut data = [0x66; 256];
        data[0] = 0x77;
        data[255] = 0x88;

        ppu.write_to_oam_addr(0x10);
        ppu.write_oam_dma(&data);

        ppu.write_to_oam_addr(0xf); //wrap around
        assert_eq!(ppu.read_oam_data(), 0x88);

        ppu.write_to_oam_addr(0x10);
        assert_eq!(ppu.read_oam_data(), 0x77);

        ppu.write_to_oam_addr(0x11);
        assert_eq!(ppu.read_oam_data(), 0x66);
    }

    #[test]
    fn oam_dma_during_rendering_does_not_modify_oam() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.oam_data.fill(0x55);
        ppu.write_to_oam_addr(0x10);
        force_mask(&mut ppu, 0x10);

        ppu.write_oam_dma(&[0xaa; 256]);

        assert!(ppu.oam_data.iter().all(|&value| value == 0x55));
        assert_eq!(ppu.oam_addr, 0x10);
        assert_eq!(ppu.read_io_data_bus(), 0xaa);
    }
}
