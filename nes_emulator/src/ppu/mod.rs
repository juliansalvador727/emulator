use crate::cartridge::Mirroring;
use crate::mapper::SharedMapper;
use crate::render::{frame::Frame, palette::SYSTEM_PALLETE};
use registers::control::ControlRegister;
use registers::loopy::LoopyRegister;
use registers::mask::MaskRegister;
use registers::status::StatusRegister;

pub mod registers;

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
    internal_data_buf: u8,

    // Current NTSC raster position. Dots are numbered 0..=340 and scanlines
    // 0..=261, with 261 being the pre-render line.
    scanline: u16,
    dot: u16,
    odd_frame: bool,
    total_dots: u64,
    suppress_vblank: bool,
    pub nmi_interrupt: Option<u8>,

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
            internal_data_buf: 0,
            scanline: 0,
            dot: 0,
            odd_frame: false,
            total_dots: 0,
            suppress_vblank: false,
            nmi_interrupt: None,
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

    pub fn tick(&mut self, cycles: u8) -> bool {
        let mut frame_complete = false;
        for _ in 0..cycles {
            frame_complete |= self.clock_dot();
        }
        frame_complete
    }

    fn rendering_enabled(&self) -> bool {
        self.mask.show_background() || self.mask.show_sprites()
    }

    // Advance one PPU dot. Rendering, fetches, scrolling and mapper-visible
    // address-bus activity all originate from this timeline.
    fn clock_dot(&mut self) -> bool {
        let render_line = self.scanline < 240 || self.scanline == 261;
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

            if self.scanline < 240 && (1..=256).contains(&self.dot) {
                self.render_pixel();
            }

            if (1..=64).contains(&self.dot) {
                self.clock_secondary_oam_clear();
            } else if (65..=256).contains(&self.dot) {
                self.clock_sprite_evaluation();
            }
            if (257..=320).contains(&self.dot) {
                self.clock_sprite_fetch();
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

        if self.scanline == 241 && self.dot == 1 {
            if !self.suppress_vblank {
                self.status.set_vblank_status(true);
                if self.ctrl.generate_vblank_nmi() {
                    self.nmi_interrupt = Some(1);
                }
            }
            self.suppress_vblank = false;
        } else if self.scanline == 261 {
            if self.dot == 1 {
                self.status.reset_vblank_status();
                self.status.set_sprite_zero_hit(false);
                self.status.set_sprite_overflow(false);
                self.nmi_interrupt = None;
            }
        }

        // On odd rendered NTSC frames the pre-render line omits dot 340.
        let last_dot = if self.scanline == 261 && self.odd_frame && self.rendering_enabled() {
            339
        } else {
            340
        };
        let frame_complete = if self.dot == last_dot {
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
        // Secondary OAM is filled with $FF over alternating read/write dots.
        // Model the externally relevant write half of each pair.
        if self.dot & 1 == 0 {
            self.secondary_oam[(self.dot / 2 - 1) as usize] = 0xff;
        }
    }

    fn clock_sprite_evaluation(&mut self) {
        if self.sprite_eval_n >= 64 {
            return;
        }

        // Odd dots read primary OAM; even dots run the evaluation/write half.
        if self.dot & 1 != 0 {
            self.sprite_eval_latch =
                self.oam_data[self.sprite_eval_n * 4 + self.sprite_eval_m];
            return;
        }

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
                self.sprite_eval_m = (self.sprite_eval_m + 1) & 3;
            }
            self.sprite_eval_n += 1;
        }
    }

    fn sprite_pattern_addr(&self, slot: usize) -> u16 {
        if slot >= self.secondary_count {
            return 0;
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
        let show_bg = self.mask.show_background()
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

        let show_sprites = self.mask.show_sprites()
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
            (0, 0) => 0,
            (0, sp) => 0x10 + sprite_palette as usize * 4 + sp as usize,
            (bg, 0) => bg_palette as usize * 4 + bg as usize,
            (bg, _) if sprite_behind => bg_palette as usize * 4 + bg as usize,
            (_, sp) => 0x10 + sprite_palette as usize * 4 + sp as usize,
        };
        let color = SYSTEM_PALLETE[(self.palette_table[palette_index] & 0x3f) as usize];
        self.frame.set_pixel(x, self.scanline as usize, color);

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

    fn ppu_bus_read(&mut self, addr: u16) -> u8 {
        let addr = addr & 0x3fff;
        self.mapper.borrow_mut().on_ppu_bus_access(addr, self.total_dots);
        match addr {
            0x0000..=0x1fff => self.mapper.borrow_mut().ppu_read(addr),
            0x2000..=0x3eff => self.vram[self.mirror_vram_addr(addr) as usize],
            0x3f00..=0x3fff => self.palette_table[(addr as usize - 0x3f00) & 0x1f],
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
                let mut index = (addr as usize - 0x3f00) & 0x1f;
                if matches!(index, 0x10 | 0x14 | 0x18 | 0x1c) {
                    index -= 0x10;
                }
                self.palette_table[index] = value;
            }
            _ => unreachable!(),
        }
    }

    pub fn poll_nmi_interrupt(&mut self) -> Option<u8> {
        self.nmi_interrupt.take()
    }

    #[cfg(test)]
    pub fn new_empty_rom() -> Self {
        NesPPU::new(crate::mapper::test_nrom(vec![0; 0x2000], Mirroring::Horizontal))
    }

    pub fn write_to_ppu_addr(&mut self, value: u8) {
        self.note_register_write(0x2006);
        self.loopy.write_addr(value);
    }

    pub fn write_to_ctrl(&mut self, value: u8) {
        self.note_register_write(0x2000);
        let generated_nmi = self.ctrl.generate_vblank_nmi();
        self.ctrl.update(value);
        self.loopy.write_ctrl(value);
        // Enabling NMI during an active vblank produces an immediate NMI edge.
        if !generated_nmi && self.ctrl.generate_vblank_nmi() && self.status.is_in_vblank() {
            self.nmi_interrupt = Some(1);
        } else if !self.ctrl.generate_vblank_nmi() {
            self.nmi_interrupt = None;
        }
    }

    pub fn write_to_mask(&mut self, value: u8) {
        self.note_register_write(0x2001);
        self.mask.update(value);
    }

    // Reading PPUSTATUS clears vblank and resets the single shared $2005/$2006
    // write latch.
    pub fn read_status(&mut self) -> u8 {
        let data = self.status.snapshot();
        // A read immediately before vblank suppresses that frame's vblank flag
        // and NMI. A read on dot 1 clears the just-set flag/NMI below.
        if self.scanline == 241 && self.dot == 0 {
            self.suppress_vblank = true;
        }
        self.status.reset_vblank_status();
        self.nmi_interrupt = None;
        self.loopy.reset_latch();
        data
    }

    pub fn write_to_oam_addr(&mut self, value: u8) {
        self.note_register_write(0x2003);
        self.oam_addr = value;
    }

    pub fn write_to_oam_data(&mut self, value: u8) {
        self.note_register_write(0x2004);
        self.oam_data[self.oam_addr as usize] = value;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    pub fn read_oam_data(&self) -> u8 {
        self.oam_data[self.oam_addr as usize]
    }

    pub fn write_to_scroll(&mut self, value: u8) {
        self.note_register_write(0x2005);
        self.loopy.write_scroll(value);
    }

    // OAM DMA: copy a 256-byte page (supplied by the bus from CPU RAM)
    // into OAM starting at the current oam_addr.
    pub fn write_oam_dma(&mut self, data: &[u8; 256]) {
        self.probe_diagnostics.oam_dma_count += 1;
        for x in data.iter() {
            self.oam_data[self.oam_addr as usize] = *x;
            self.oam_addr = self.oam_addr.wrapping_add(1);
        }
    }

    fn increment_vram_addr(&mut self) {
        self.loopy.increment(self.ctrl.vram_addr_increment());
    }

    pub fn write_to_data(&mut self, value: u8) {
        self.note_register_write(0x2007);
        let addr = self.loopy.current();
        self.ppu_bus_write(addr, value);
        self.increment_vram_addr();
    }

    pub fn read_data(&mut self) -> u8 {
        let addr = self.loopy.current();
        self.increment_vram_addr();

        if addr < 0x3f00 {
            let result = self.internal_data_buf;
            self.internal_data_buf = self.ppu_bus_read(addr);
            result
        } else {
            let result = self.ppu_bus_read(addr);
            // Palette reads are immediate but still refill the delayed buffer
            // from the mirrored nametable address beneath palette space.
            self.internal_data_buf = self.ppu_bus_read(addr - 0x1000);
            result
        }
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
    pub fn sync_scroll_for_test(&mut self) {
        self.loopy.copy_all_for_test();
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

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
    fn vblank_starts_at_scanline_241_dot_1() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.scanline = 241;
        ppu.dot = 0;
        ppu.write_to_ctrl(0x80);

        ppu.tick(1); // dot 0
        assert!(!ppu.status.is_in_vblank());
        assert_eq!(ppu.poll_nmi_interrupt(), None);

        ppu.tick(1); // dot 1
        assert!(ppu.status.is_in_vblank());
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
    fn enabling_nmi_during_vblank_raises_an_edge() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.status.set_vblank_status(true);

        ppu.write_to_ctrl(0x80);

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
    fn background_fetches_follow_the_eight_dot_bus_sequence() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_mask(0x08);
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
        ppu.write_to_mask(0x08);

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
        ppu.write_to_mask(0x08);
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
    fn test_ppu_vram_writes() {
        let mut ppu = NesPPU::new_empty_rom();
        ppu.write_to_ppu_addr(0x23);
        ppu.write_to_ppu_addr(0x05);
        ppu.write_to_data(0x66);

        assert_eq!(ppu.vram[0x0305], 0x66);
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
}
