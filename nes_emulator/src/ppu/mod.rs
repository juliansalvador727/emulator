use crate::cartridge::Mirroring;
use crate::mapper::SharedMapper;
use crate::render::{self, frame::Frame};
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
    pending_sprite_zero_hit_dot: Option<u16>,
    suppress_vblank: bool,
    pub nmi_interrupt: Option<u8>,

    // The PPU renders directly into this frame, one scanline at a time, as it
    // crosses each line (see `tick`). Held in an Option only so a single line
    // can be composited: `composite_scanline` detaches it to hand `&self` to
    // the compositor, then puts it back. It is always `Some` between calls.
    frame: Option<Frame>,
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
            pending_sprite_zero_hit_dot: None,
            suppress_vblank: false,
            nmi_interrupt: None,
            frame: Some(Frame::new()),
            probe_diagnostics: ProbeDiagnostics::default(),
        }
    }

    // The finished frame, ready to present. Valid to read at vblank, once every
    // visible line has been composited during this frame's ticks.
    pub fn frame(&self) -> &Frame {
        self.frame
            .as_ref()
            .expect("frame is detached only transiently during a scanline render")
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

    // Advance one PPU dot. The scanline compositor remains deliberately
    // coarse, but state changes now occur at their hardware raster positions,
    // providing a narrow timing layer for register races and mapper work.
    fn clock_dot(&mut self) -> bool {
        if self.scanline < 240 {
            if self.dot == 0 {
                self.pending_sprite_zero_hit_dot = self
                    .composite_scanline(self.scanline as usize)
                    .map(|x| x as u16 + 1);
            }

            if self.pending_sprite_zero_hit_dot == Some(self.dot) {
                self.status.set_sprite_zero_hit(true);
                self.pending_sprite_zero_hit_dot = None;
            }

            if self.dot == 256 && self.mask.show_background() {
                self.loopy.increment_y();
            }
            if self.dot == 257 && self.rendering_enabled() {
                self.loopy.copy_horizontal();
            }
            // This remains the existing scanline approximation until the next
            // TODO item replaces it with fetch-driven A12 edge detection.
            if self.dot == 260 && self.rendering_enabled() {
                self.mapper.borrow_mut().on_scanline();
            }
        } else if self.scanline == 241 && self.dot == 1 {
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
            if self.dot == 257 && self.rendering_enabled() {
                self.loopy.copy_horizontal();
            }
            if (280..=304).contains(&self.dot) && self.rendering_enabled() {
                self.loopy.copy_vertical();
            }
        }

        // On odd rendered NTSC frames the pre-render line omits dot 340.
        let last_dot = if self.scanline == 261 && self.odd_frame && self.rendering_enabled() {
            339
        } else {
            340
        };
        if self.dot == last_dot {
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
        }
    }

    // Render one visible scanline into the owned frame. The frame is detached
    // for the duration so the compositor can borrow `&self` (it reads VRAM,
    // OAM, palette, and CHR through the mapper) while writing pixels; it is
    // restored before returning. `take` swaps in `None`, so there is no
    // allocation on this path.
    fn composite_scanline(&mut self, line: usize) -> Option<usize> {
        let mut frame = self
            .frame
            .take()
            .expect("frame present at start of scanline render");
        let sprite_zero_hit = render::render_scanline_with_sprite_zero(self, &mut frame, line);
        self.frame = Some(frame);
        sprite_zero_hit
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
        match addr {
            0..=0x1fff => self.mapper.borrow_mut().ppu_write(addr, value),
            0x2000..=0x2fff => {
                self.vram[self.mirror_vram_addr(addr) as usize] = value;
            }
            0x3000..=0x3eff => unimplemented!("addr {} shouldn't be used in reallity", addr),

            //Addresses $3F10/$3F14/$3F18/$3F1C are mirrors of $3F00/$3F04/$3F08/$3F0C
            0x3f10 | 0x3f14 | 0x3f18 | 0x3f1c => {
                let add_mirror = addr - 0x10;
                self.palette_table[(add_mirror - 0x3f00) as usize] = value;
            }
            0x3f00..=0x3fff => {
                self.palette_table[(addr - 0x3f00) as usize] = value;
            }
            _ => panic!("unexpected access to mirrored space {}", addr),
        }
        self.increment_vram_addr();
    }

    pub fn read_data(&mut self) -> u8 {
        let addr = self.loopy.current();
        self.increment_vram_addr();

        match addr {
            0..=0x1fff => {
                let result = self.internal_data_buf;
                self.internal_data_buf = self.mapper.borrow_mut().ppu_read(addr);
                result
            }
            0x2000..=0x2fff => {
                let result = self.internal_data_buf;
                self.internal_data_buf = self.vram[self.mirror_vram_addr(addr) as usize];
                result
            }
            0x3000..=0x3eff => panic!(
                "addr space 0x3000..0x3eff is not expected to be used, request = {} ",
                addr
            ),
            0x3f00..=0x3fff => self.palette_table[(addr - 0x3f00) as usize],
            _ => panic!("unexpected access to mirrored space {}", addr),
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
        // MMC3 once and the mapper holds its level-triggered IRQ line high.
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
        // Tile 1 is opaque on its first row for both background and sprite.
        ppu.mapper.borrow_mut().ppu_write(16, 0xff);
        ppu.oam_data[0..4].copy_from_slice(&[0, 1, 0, 8]);
        ppu.scanline = 1;
        ppu.dot = 0;

        ppu.tick(9); // process dots 0..=8
        assert_eq!(ppu.status.snapshot() & 0x40, 0);
        ppu.tick(1); // sprite x=8 becomes visible on PPU dot 9
        assert_eq!(ppu.status.snapshot() & 0x40, 0x40);
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
        assert_eq!(ppu.loopy.current() & 0x001f, 0);
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
