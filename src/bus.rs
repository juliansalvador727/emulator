use std::rc::Rc;

use crate::apu::NesAPU;
use crate::cartridge::Rom;
use crate::cpu::Mem;
use crate::joypad::Joypad;
use crate::mapper::{self, SharedMapper};
use crate::ppu::NesPPU;

const RAM: u16 = 0x0000;
const RAM_MIRRORS_END: u16 = 0x1FFF;
// const PPU_REGISTERS: u16 = 0x2000; unused
const PPU_REGISTERS_MIRRORS_END: u16 = 0x3FFF;

pub struct Bus<'call> {
    cpu_vram: [u8; 2048],
    mapper: SharedMapper,
    ppu: NesPPU,
    pub apu: NesAPU,

    cycles: usize,
    // Page selected by the last $4014 write, consumed on the next tick so the
    // 513/514-cycle stall begins at the following CPU-cycle boundary.
    oam_dma_page: Option<u8>,
    // Cycles a DMC sample fetch has been waiting without a read cycle to steal.
    // Reset once serviced; the safety net in `tick` services it if the CPU runs
    // a stretch of non-read cycles so the fetch is never dropped.
    dmc_pending_ticks: u8,
    gameloop_callback: Box<dyn FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call>,
    audio_chunk_samples: Option<usize>,
    audio_callback: Box<dyn FnMut(Vec<f32>) + 'call>,
    audio_delivery_enabled: bool,
    host_frame_ready: bool,
    joypad1: Joypad,
}

pub struct BusSnapshot {
    cpu_vram: [u8; 2048],
    mapper: SharedMapper,
    ppu: NesPPU,
    apu: NesAPU,
    cycles: usize,
    // Page selected by the last $4014 write, consumed on the next tick so the
    // 513/514-cycle stall begins at the following CPU-cycle boundary.
    oam_dma_page: Option<u8>,
    dmc_pending_ticks: u8,
    audio_delivery_enabled: bool,
    host_frame_ready: bool,
    joypad1: Joypad,
}

impl<'a> Bus<'a> {
    pub fn new<'call, F>(rom: Rom, gameloop_callback: F) -> Bus<'call>
    where
        F: FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call,
    {
        Self::new_with_audio(rom, gameloop_callback, usize::MAX, |_| {})
    }

    /// Construct a bus that forwards completed audio chunks independently of
    /// video frames. `audio_chunk_samples` is a host-delivery quantum, not an
    /// emulation timing parameter; the APU still runs at the CPU clock.
    pub fn new_with_audio<'call, F, A>(
        rom: Rom,
        gameloop_callback: F,
        audio_chunk_samples: usize,
        audio_callback: A,
    ) -> Bus<'call>
    where
        F: FnMut(&NesPPU, &mut NesAPU, &mut Joypad) + 'call,
        A: FnMut(Vec<f32>) + 'call,
    {
        assert!(audio_chunk_samples > 0);
        let mapper = mapper::from_rom(rom);
        let ppu = NesPPU::new(Rc::clone(&mapper));
        Bus {
            cpu_vram: [0; 2048],
            mapper: mapper,
            ppu: ppu,
            apu: NesAPU::new(),
            cycles: 0,
            oam_dma_page: None,
            dmc_pending_ticks: 0,
            gameloop_callback: Box::from(gameloop_callback),
            audio_chunk_samples: (audio_chunk_samples != usize::MAX)
                .then_some(audio_chunk_samples),
            audio_callback: Box::from(audio_callback),
            audio_delivery_enabled: true,
            host_frame_ready: false,
            joypad1: Joypad::new(),
        }
    }

    #[inline]
    fn deliver_audio_chunks(&mut self) {
        let Some(chunk_samples) = self.audio_chunk_samples else {
            return;
        };
        if !self.audio_delivery_enabled {
            return;
        }
        while self.apu.buffered_samples() >= chunk_samples {
            let samples = self.apu.drain_sample_chunk(chunk_samples);
            (self.audio_callback)(samples);
        }
    }

    pub fn tick(&mut self, cycles: u8) {
        self.cycles += cycles as usize;
        self.apu.tick(cycles);
        self.ppu.tick(cycles * 3);
        let mut frame_ready = self.ppu.take_frame_ready();

        // OAM DMA halts the CPU and copies a page into OAM one byte per pair
        // of CPU cycles. Deferring it to the tick after the $4014 write means
        // the halt begins at the following CPU-cycle boundary, and the modeled
        // get/put cycles keep the PPU and APU running throughout.
        if let Some(page) = self.oam_dma_page.take() {
            self.run_oam_dma(page, &mut frame_ready);
        }

        // A DMC sample fetch outside an OAM DMA is serviced when the CPU next
        // performs a read cycle (`dmc_halt_before_read`), so the RDY halt can
        // re-read the CPU's own address -- the DMC-DMA-during-read behavior.
        // As a safety net for a rare run of non-read cycles, service a pending
        // fetch here without a re-read once it has been waiting.
        if self.apu.dmc_dma_request().is_some() {
            self.dmc_pending_ticks = self.dmc_pending_ticks.saturating_add(1);
            if self.dmc_pending_ticks >= 3 {
                self.run_dmc_dma(None, &mut frame_ready);
            }
        } else {
            self.dmc_pending_ticks = 0;
        }

        self.deliver_audio_chunks();

        // Presentation, input sampling, and audio delivery happen at the start
        // of vblank, before the CPU can service the NMI and poll the controller.
        // The event remains independent of NMI enable so transitions with NMI
        // disabled cannot skip host frames or accumulate audio.
        if frame_ready {
            self.host_frame_ready = true;
            (self.gameloop_callback)(&self.ppu, &mut self.apu, &mut self.joypad1);
        }
    }

    // One halted CPU cycle: the CPU is stopped for DMA, but the PPU (3 dots)
    // and APU (1 cycle) keep running. Returns nothing; `frame_ready` collects
    // any vblank that starts mid-transfer so the host frame is not lost.
    #[inline]
    fn dma_cpu_cycle(&mut self, frame_ready: &mut bool) {
        self.cycles += 1;
        self.apu.tick(1);
        self.ppu.tick(3);
        *frame_ready |= self.ppu.take_frame_ready();
    }

    // Perform one OAM DMA. The transfer is 512 alternating get (read) and put
    // (OAM write) cycles, preceded by a halt cycle and, when the DMA starts on
    // an odd CPU cycle, an alignment cycle. This reproduces the hardware
    // 513/514-cycle stall and places every read and write on its real CPU
    // cycle rather than copying the page atomically.
    fn run_oam_dma(&mut self, page: u8, frame_ready: &mut bool) {
        self.ppu.note_oam_dma_start();
        let base = (page as u16) << 8;

        // Halt cycle, plus an alignment cycle when the DMA begins on an odd
        // CPU cycle. Evaluated at entry so the stall is 513 cycles on an even
        // boundary and 514 on an odd one.
        let alignment = self.cycles & 1;
        for _ in 0..(1 + alignment) {
            self.dma_cpu_cycle(frame_ready);
        }

        for offset in 0..256u16 {
            // The DMC memory reader has priority over OAM DMA and steals the
            // bus on a get cycle before the OAM read is allowed to proceed.
            self.service_dmc_during_oam(frame_ready);

            let value = self.mem_read(base + offset); // get cycle
            self.dma_cpu_cycle(frame_ready);
            self.ppu.oam_dma_write(value); // put cycle
            self.dma_cpu_cycle(frame_ready);
        }
    }

    // DMC/OAM DMA arbitration. When the DMC's sample buffer empties during an
    // OAM DMA the DMC steals the bus: it inserts a halt/alignment cycle and
    // then reads its byte on a get cycle. That is two CPU cycles of added
    // stall per fetch on top of the OAM transfer, with the OAM read that would
    // have happened simply delayed. The rarer sub-cycle case where the steal
    // lands so as to corrupt an OAM byte depends on the exact CPU bus cycle of
    // the fetch and is deferred to the cycle-accurate CPU work; OAM data
    // integrity is preserved here, matching the common aligned behavior.
    fn service_dmc_during_oam(&mut self, frame_ready: &mut bool) {
        if let Some(addr) = self.apu.dmc_dma_request() {
            self.dma_cpu_cycle(frame_ready); // halt/alignment cycle
            let value = self.mem_read(addr);
            self.apu.dmc_dma_load(value);
            self.dma_cpu_cycle(frame_ready); // DMC get cycle
        }
    }

    // Service a pending DMC sample fetch: RDY halts the CPU for four cycles.
    // When the halt lands on a read cycle, `re_read` is the address the CPU was
    // reading and it is re-read on each of the three halt cycles before the DMC
    // get. Re-reading a side-effecting register ($2007/$2002/$4016) several
    // times is the DMC-DMA-during-read behavior real hardware exhibits; for
    // ordinary memory the extra reads are harmless.
    fn run_dmc_dma(&mut self, re_read: Option<u16>, frame_ready: &mut bool) {
        let Some(dmc_addr) = self.apu.dmc_dma_request() else {
            return;
        };
        self.dmc_pending_ticks = 0;
        // Hardware re-reads 2-3 times depending on where the halt lands relative
        // to the get/put phase; we always re-read three times. dmc_2007_read
        // accepts the three-read variant, but dmc_4016_read shows the cost:
        // $4016's shift register only advances once across the repeated reads,
        // so three re-reads eat two bits too many. Deriving the count from
        // `self.cycles` parity is not the fix - the DMA is driven off the
        // handshake in Bus::tick rather than the DMC timer, so the halt already
        // lands a cycle late and the parity here is not the hardware's phase.
        for _ in 0..3 {
            if let Some(addr) = re_read {
                let _ = self.mem_read(addr);
            }
            self.dma_cpu_cycle(frame_ready);
        }
        let value = self.mem_read(dmc_addr); // get cycle
        self.apu.dmc_dma_load(value);
        self.dma_cpu_cycle(frame_ready);
    }

    // Called at the start of a CPU read cycle. If a DMC sample fetch is pending,
    // the CPU is halted and `addr` re-read before the DMC get; the instruction's
    // real read of `addr` then proceeds, so a side-effecting register ends up
    // read several times.
    pub fn dmc_halt_before_read(&mut self, addr: u16) {
        if self.apu.dmc_dma_request().is_none() {
            return;
        }
        let mut frame_ready = false;
        self.run_dmc_dma(Some(addr), &mut frame_ready);
        if frame_ready {
            self.host_frame_ready = true;
            (self.gameloop_callback)(&self.ppu, &mut self.apu, &mut self.joypad1);
        }
    }

    pub fn take_host_frame_ready(&mut self) -> bool {
        std::mem::take(&mut self.host_frame_ready)
    }

    pub fn ppu(&self) -> &NesPPU {
        &self.ppu
    }

    pub fn joypad_mut(&mut self) -> &mut Joypad {
        &mut self.joypad1
    }

    pub fn joypad(&self) -> &Joypad {
        &self.joypad1
    }

    pub fn set_audio_delivery_enabled(&mut self, enabled: bool) {
        self.audio_delivery_enabled = enabled;
    }

    pub fn snapshot(&self) -> BusSnapshot {
        let cloned_mapper = self.mapper.borrow().as_ref().clone_box();
        let mapper = Rc::new(std::cell::RefCell::new(cloned_mapper));
        let ppu = self.ppu.clone_with_mapper(Rc::clone(&mapper));
        BusSnapshot {
            cpu_vram: self.cpu_vram,
            mapper,
            ppu,
            apu: self.apu.clone(),
            cycles: self.cycles,
            oam_dma_page: self.oam_dma_page,
            dmc_pending_ticks: self.dmc_pending_ticks,
            audio_delivery_enabled: self.audio_delivery_enabled,
            host_frame_ready: self.host_frame_ready,
            joypad1: self.joypad1.clone(),
        }
    }

    pub fn restore(&mut self, snapshot: BusSnapshot) {
        self.cpu_vram = snapshot.cpu_vram;
        self.mapper = snapshot.mapper;
        self.ppu = snapshot.ppu;
        self.apu = snapshot.apu;
        self.cycles = snapshot.cycles;
        self.oam_dma_page = snapshot.oam_dma_page;
        self.dmc_pending_ticks = snapshot.dmc_pending_ticks;
        self.audio_delivery_enabled = snapshot.audio_delivery_enabled;
        self.host_frame_ready = snapshot.host_frame_ready;
        self.joypad1 = snapshot.joypad1;
    }

    pub fn poll_nmi_status(&mut self) -> Option<u8> {
        self.ppu.poll_nmi_interrupt()
    }

    // The IRQ line seen by the CPU. Both sources are level-triggered and stay
    // asserted until acknowledged: the APU (frame counter or DMC) and the
    // mapper (MMC3 scanline counter, acked by a write to $E000).
    pub fn poll_irq_status(&self) -> bool {
        self.apu.irq_pending() || self.mapper.borrow().irq_pending()
    }

    #[cfg(test)]
    pub(crate) fn cpu_cycles(&self) -> usize {
        self.cycles
    }
}

impl Mem for Bus<'_> {
    fn mem_read(&mut self, addr: u16) -> u8 {
        match addr {
            RAM..=RAM_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00000111_11111111;
                self.cpu_vram[mirror_down_addr as usize]
            }
            // Reads from nominally write-only PPU ports expose the PPU's
            // internal I/O latch without refreshing it.
            0x2000 | 0x2001 | 0x2003 | 0x2005 | 0x2006 => self.ppu.read_io_data_bus(),
            0x2002 => self.ppu.read_status(),
            0x2004 => self.ppu.read_oam_data(),
            0x2007 => self.ppu.read_data(),

            0x2008..=PPU_REGISTERS_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00100000_00000111;
                self.mem_read(mirror_down_addr)
            }
            0x4015 => self.apu.read_status(),
            0x4016 => self.joypad1.read(),
            // $4017 reads controller 2 on real hardware. This emulator only
            // exposes joypad 1, so return an idle value instead of spamming
            // stdout for games that poll both controller ports.
            0x4017 => 0,
            // $6000-$7FFF is PRG-RAM; $8000-$FFFF is PRG-ROM. Both go through
            // the mapper.
            0x6000..=0xFFFF => self.mapper.borrow_mut().cpu_read(addr),

            _ => {
                println!("Ignoring mem access at {}", addr);
                0
            }
        }
    }

    fn mem_write(&mut self, addr: u16, data: u8) {
        match addr {
            RAM..=RAM_MIRRORS_END => {
                let mirror_down_addr = addr & 0b11111111111;
                self.cpu_vram[mirror_down_addr as usize] = data;
            }
            0x2000 => {
                self.ppu.write_to_ctrl(data);
            }
            0x2001 => {
                self.ppu.write_to_mask(data);
            }

            // PPUSTATUS is read-only, but writes still drive the PPU I/O bus.
            0x2002 => self.ppu.write_to_status(data),

            0x2003 => {
                self.ppu.write_to_oam_addr(data);
            }
            0x2004 => {
                self.ppu.write_to_oam_data(data);
            }
            0x2005 => {
                self.ppu.write_to_scroll(data);
            }
            0x2006 => {
                self.ppu.write_to_ppu_addr(data);
            }
            0x2007 => {
                self.ppu.write_to_data(data);
            }

            0x2008..=PPU_REGISTERS_MIRRORS_END => {
                let mirror_down_addr = addr & 0b00100000_00000111;
                self.mem_write(mirror_down_addr, data);
            }

            // OAM DMA ($4014): schedule a copy of CPU page $XX00-$XXFF into
            // PPU OAM. The transfer itself runs on the next tick so its reads
            // and writes land on their real alternating CPU cycles and can be
            // interrupted by DMC DMA (see `run_oam_dma`).
            0x4014 => {
                self.oam_dma_page = Some(data);
            }

            // APU registers ($4014 is OAM DMA above, $4016 the joypad below;
            // reads of $4017 would be joypad 2, but writes go to the APU
            // frame counter).
            0x4000..=0x4013 | 0x4015 | 0x4017 => {
                self.apu.write_register(addr, data);
            }

            0x4016 => {
                self.joypad1.write(data);
            }

            // Writes to $6000-$7FFF hit PRG-RAM; writes to $8000-$FFFF are the
            // mapper's bank-switch control registers, not errors.
            0x6000..=0xFFFF => self.mapper.borrow_mut().cpu_write(addr, data),

            _ => {
                println!("Ignoring mem write-access at {}", addr);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::test::test_rom;

    fn test_bus<'a>() -> Bus<'a> {
        Bus::new(test_rom(vec![]), |_, _, _| {})
    }

    #[test]
    fn apu_registers_reachable_through_bus() {
        let mut bus = test_bus();
        bus.mem_write(0x4015, 0x01); // enable pulse 1
        bus.mem_write(0x4003, 0x08); // load its length counter
        assert_eq!(bus.mem_read(0x4015) & 0x01, 0x01);
    }

    #[test]
    fn joypad2_reads_do_not_fall_through_to_unmapped_io() {
        let mut bus = test_bus();
        assert_eq!(bus.mem_read(0x4017), 0);
    }

    #[test]
    fn frame_irq_reaches_bus_irq_line_and_status_read_clears_it() {
        let mut bus = test_bus();
        // 4-step mode with IRQ enabled is the power-up default; one full
        // sequence is 29830 CPU cycles.
        for _ in 0..(29830 / 10) {
            bus.tick(10);
        }
        assert!(bus.poll_irq_status());
        assert_eq!(bus.mem_read(0x4015) & 0x40, 0x40);
        assert!(!bus.poll_irq_status());
    }

    #[test]
    fn dmc_dma_fetches_sample_bytes_from_prg_rom() {
        let mut bus = test_bus();
        // test_rom PRG is all zeroes; what matters here is that enabling
        // the DMC drains bytes via DMA as the bus ticks.
        bus.mem_write(0x4012, 0x00); // sample address $C000
        bus.mem_write(0x4013, 0x00); // length 1 byte
        bus.mem_write(0x4015, 0x10); // enable DMC
        // The fetch is serviced on the next CPU read cycle; with no CPU driving
        // reads here, the tick safety net services it after a few cycles.
        for _ in 0..4 {
            bus.tick(1);
        }
        assert_eq!(bus.mem_read(0x4015) & 0x10, 0); // 0 bytes remaining
    }

    #[test]
    fn dmc_dma_during_a_2007_read_repeats_the_read() {
        // With a DMC sample fetch pending, a CPU read cycle is halted (RDY low)
        // and the read address is re-read on each halt cycle before the real
        // read. For a side-effecting register like $2007 that means several
        // extra reads, each advancing the PPU address, ahead of the value the
        // CPU finally latches. A read with no DMC pending advances once.
        fn read_2007_after_priming(with_dmc: bool) -> (u8, usize) {
            let mut bus = test_bus();
            // Fill nametable $2000.. with distinct, sequential bytes.
            bus.mem_write(0x2006, 0x20);
            bus.mem_write(0x2006, 0x00);
            for i in 0..8u8 {
                bus.mem_write(0x2007, 0x40 + i);
            }
            if with_dmc {
                bus.mem_write(0x4012, 0x00); // sample address $C000
                bus.mem_write(0x4013, 0x00); // length 1 byte
                bus.mem_write(0x4015, 0x10); // enable DMC -> fetch pending
                assert!(bus.apu.dmc_dma_request().is_some());
            }
            // Point at $2000 and prime the buffered read.
            bus.mem_write(0x2006, 0x20);
            bus.mem_write(0x2006, 0x00);
            let _ = bus.mem_read(0x2007);
            // Simulate one CPU read cycle of $2007.
            let cycles_before = bus.cycles;
            bus.dmc_halt_before_read(0x2007);
            let value = bus.mem_read(0x2007);
            (value, bus.cycles - cycles_before)
        }

        let (plain, plain_cycles) = read_2007_after_priming(false);
        let (stolen, stolen_cycles) = read_2007_after_priming(true);
        // Three extra $2007 reads during the four-cycle DMC halt advance the
        // buffered value three positions past the un-stolen read.
        assert_eq!(stolen, plain.wrapping_add(3));
        assert_eq!(plain_cycles, 0); // no halt when nothing is pending
        assert_eq!(stolen_cycles, 4); // four-cycle DMC steal
    }

    #[test]
    fn frame_callback_fires_at_vblank_without_nmi_enabled() {
        let callbacks = std::rc::Rc::new(std::cell::Cell::new(0));
        let callback_count = callbacks.clone();
        let mut bus = Bus::new(test_rom(vec![]), move |_, _, _| {
            callback_count.set(callback_count.get() + 1);
        });

        // The first vblank begins while the raster is still well short of the
        // end-of-frame wrap. PPUCTRL remains at reset with NMI disabled.
        for _ in 0..27_393 {
            bus.tick(1);
        }
        assert_eq!(callbacks.get(), 0);
        bus.tick(1);
        assert_eq!(callbacks.get(), 1);
    }

    #[test]
    fn vblank_callback_input_is_visible_to_the_next_controller_poll() {
        let mut bus = Bus::new(test_rom(vec![]), |_, _, joypad| {
            joypad.set_button_pressed_status(crate::joypad::JoypadButton::BUTTON_A, true);
        });

        for _ in 0..27_394 {
            bus.tick(1);
        }
        bus.mem_write(0x4016, 1);
        bus.mem_write(0x4016, 0);
        assert_eq!(bus.mem_read(0x4016), 1);
    }

    #[test]
    fn audio_chunks_are_delivered_before_a_video_frame_completes() {
        let delivered = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let callback_delivered = delivered.clone();
        let mut bus = Bus::new_with_audio(
            test_rom(vec![]),
            |_, _, _| {},
            256,
            move |samples| {
                assert_eq!(samples.len(), 256);
                callback_delivered.set(callback_delivered.get() + samples.len());
            },
        );
        bus.apu.set_sample_rate(crate::audio::SAMPLE_RATE);

        // 11,000 CPU cycles produce about 271 samples, well before the first
        // vblank at ~27,394 cycles.
        for _ in 0..220 {
            bus.tick(50);
        }
        assert_eq!(delivered.get(), 256);
        assert!(bus.apu.buffered_samples() < 256);
    }

    #[test]
    fn oam_dma_stalls_cpu_while_ppu_and_apu_keep_running() {
        let mut bus = test_bus();
        bus.mem_write(0x4014, 0x00);
        bus.tick(4);

        assert_eq!(bus.cycles, 4 + 513);

        let mut odd_bus = test_bus();
        odd_bus.tick(1);
        odd_bus.mem_write(0x4014, 0x00);
        odd_bus.tick(4);

        assert_eq!(odd_bus.cycles, 1 + 4 + 514);
    }

    #[test]
    fn oam_dma_copies_the_page_into_oam_with_oamaddr_wrapping() {
        let mut bus = test_bus();
        // Fill the source page (zero page) with a recognizable pattern.
        for i in 0..256u16 {
            bus.mem_write(i, (i as u8) ^ 0x5a);
        }
        // Start the transfer at OAMADDR $10 so the destination wraps.
        bus.mem_write(0x2003, 0x10);
        bus.mem_write(0x4014, 0x00);
        bus.tick(1); // runs the deferred, cycle-stepped DMA

        for i in 0..256usize {
            let dst = (0x10 + i) & 0xff;
            assert_eq!(bus.ppu().oam_data[dst], (i as u8) ^ 0x5a);
        }
        // 256 post-incrementing writes wrap OAMADDR back to where it started.
        assert_eq!(bus.ppu().oam_addr, 0x10);
    }

    #[test]
    fn oam_dma_advances_ppu_and_apu_across_the_whole_stall() {
        let mut bus = test_bus();
        let ppu_dots_before = bus.ppu().total_dots();
        let apu_before = bus.apu.cycle_count();

        bus.mem_write(0x4014, 0x02);
        bus.tick(2); // even boundary -> 513-cycle stall on top of the 2 ticks
        let elapsed = bus.cycles; // started at zero

        assert_eq!(elapsed, 2 + 513);
        // Every halted CPU cycle still clocks the PPU three dots and the APU
        // one cycle, so no time is lost during the transfer.
        assert_eq!(bus.ppu().total_dots() - ppu_dots_before, elapsed as u64 * 3);
        assert_eq!(bus.apu.cycle_count() - apu_before, elapsed);
    }

    #[test]
    fn dmc_dma_steals_cycles_during_oam_dma_without_dropping_bytes() {
        let mut bus = test_bus();
        for i in 0..256u16 {
            bus.mem_write(i, i as u8);
        }
        // Arm the DMC so its buffer is empty and a fetch is pending the moment
        // the OAM DMA reaches its first get cycle. The slow default rate means
        // exactly one fetch is stolen during the ~514-cycle transfer.
        bus.mem_write(0x4012, 0x00); // sample address $C000
        bus.mem_write(0x4013, 0x10); // 257-byte sample: stays active
        bus.mem_write(0x4015, 0x10); // enable DMC

        let apu_before = bus.apu.cycle_count();
        let ppu_dots_before = bus.ppu().total_dots();
        bus.mem_write(0x4014, 0x00);
        bus.tick(1); // odd boundary -> 514-cycle base stall

        // One steal adds a halt/alignment cycle plus a DMC get cycle: two CPU
        // cycles on top of the one tick and the 514-cycle OAM transfer.
        assert_eq!(bus.cycles, 1 + 514 + 2);
        // The PPU and APU are clocked through the stolen cycles as well.
        assert_eq!(bus.apu.cycle_count() - apu_before, bus.cycles);
        assert_eq!(
            bus.ppu().total_dots() - ppu_dots_before,
            bus.cycles as u64 * 3
        );
        // The steal delays, but never drops, the OAM data: the page still
        // copies in full.
        for i in 0..256usize {
            assert_eq!(bus.ppu().oam_data[i], i as u8);
        }
        // The DMC actually took its byte and is still playing the sample.
        assert_eq!(bus.mem_read(0x4015) & 0x10, 0x10);
    }

    #[test]
    fn writes_to_read_only_ppustatus_drive_the_ppu_io_bus() {
        let mut bus = test_bus();
        bus.mem_write(0x2002, 0xa5);

        assert_eq!(bus.mem_read(0x2000), 0xa5);
        assert_eq!(bus.mem_read(0x3ff9), 0xa5);
    }
}
