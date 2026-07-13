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

    scanline: u16,
    cycles: usize,
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
            cycles: 0,
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
            self.probe_diagnostics.last_dot = self.cycles;
        }
    }

    pub fn tick(&mut self, cycles: u8) -> bool {
        self.cycles += cycles as usize;
        let mut frame_complete = false;

        // The bus advances the PPU in chunks (`ppu.tick(cycles * 3)`), so a
        // single call can carry the PPU past more than one 341-cycle scanline
        // boundary. Loop so every completed line is rendered and none skipped.
        while self.cycles >= 341 {
            self.cycles -= 341;

            // We just crossed the end of `self.scanline`. Composite it now
            // (visible lines are 0..=239) so scroll / ctrl / CHR banks / OAM are
            // sampled as they stand at this line, capturing mid-frame changes
            // that a single vblank snapshot could never see.
            if self.scanline < 240 {
                let line = self.scanline as usize;
                let sprite_zero_hit = self.composite_scanline(line);

                // Sprite-0 hit: set as soon as the line carrying the first
                // opaque sprite-0-over-opaque-background pixel is crossed, so
                // the CPU (which busy-polls $2002) can react — e.g. change the
                // scroll to split the screen — before the next lines composite.
                if sprite_zero_hit {
                    self.status.set_sprite_zero_hit(true);
                }

                // `v` walks vertically through the background while the
                // horizontal pieces are reloaded from `t` for the next line.
                // This is the scanline-level equivalent of the C PPU's dot
                // 256/257 loopy updates.
                if self.mask.show_background() {
                    self.loopy.increment_y();
                }
                if self.mask.show_background() || self.mask.show_sprites() {
                    self.loopy.copy_horizontal();
                }

                // Clock the mapper's scanline counter (MMC3 IRQ). Real hardware
                // drives this off A12 toggling during a rendered line, so it
                // only ticks when rendering is enabled; approximate that as
                // once per visible line while background or sprites are on.
                if self.mask.show_background() || self.mask.show_sprites() {
                    self.mapper.borrow_mut().on_scanline();
                }
            }

            self.scanline += 1;

            if self.scanline == 241 {
                self.status.set_vblank_status(true);
                self.status.set_sprite_zero_hit(false);
                if self.ctrl.generate_vblank_nmi() {
                    self.nmi_interrupt = Some(1);
                }
            }

            if self.scanline >= 262 {
                self.scanline = 0;
                self.nmi_interrupt = None;
                self.status.set_sprite_zero_hit(false);
                self.status.reset_vblank_status();
                // The real PPU copies vertical scroll bits during pre-render.
                // Our scanline model performs that copy at the frame boundary,
                // before scanline 0 is next composited.
                if self.mask.show_background() || self.mask.show_sprites() {
                    self.loopy.copy_vertical();
                    self.loopy.copy_horizontal();
                }
                frame_complete = true;
            }
        }

        frame_complete
    }

    // Render one visible scanline into the owned frame. The frame is detached
    // for the duration so the compositor can borrow `&self` (it reads VRAM,
    // OAM, palette, and CHR through the mapper) while writing pixels; it is
    // restored before returning. `take` swaps in `None`, so there is no
    // allocation on this path.
    fn composite_scanline(&mut self, line: usize) -> bool {
        let mut frame = self
            .frame
            .take()
            .expect("frame present at start of scanline render");
        let sprite_zero_hit = render::render_scanline(self, &mut frame, line);
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
        self.ctrl.update(value);
        self.loopy.write_ctrl(value);
    }

    pub fn write_to_mask(&mut self, value: u8) {
        self.note_register_write(0x2001);
        self.mask.update(value);
    }

    // Reading PPUSTATUS clears vblank and resets the single shared $2005/$2006
    // write latch.
    pub fn read_status(&mut self) -> u8 {
        let data = self.status.snapshot();
        self.status.reset_vblank_status();
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
