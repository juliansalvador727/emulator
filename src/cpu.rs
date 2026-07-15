/*
note: NES CPU uses little endian addressing
Real Address	                0x8000
Address packed in big-endian	80 00
Address packed in little-endian	00 80
e.g. read data from 0x8000 into A reg:
LDA $8000      <=>    ad 00 80
*/

use crate::bus::{Bus, BusSnapshot, DmcHeldRead, InterruptBatch};
use crate::opcodes;
use std::collections::HashMap;

bitflags! {
    /// # Status Register (P) http://wiki.nesdev.com/w/index.php/Status_flags
    ///
    ///  7 6 5 4 3 2 1 0
    ///  N V _ B D I Z C
    ///  | |   | | | | +--- Carry Flag
    ///  | |   | | | +----- Zero Flag
    ///  | |   | | +------- Interrupt Disable
    ///  | |   | +--------- Decimal Mode (not used on NES)
    ///  | |   +----------- Break Command
    ///  | +--------------- Overflow Flag
    ///  +----------------- Negative Flag
    ///
    pub struct CpuFlags: u8 {
        const CARRY             = 0b00000001;
        const ZERO              = 0b00000010;
        const INTERRUPT_DISABLE = 0b00000100;
        const DECIMAL_MODE      = 0b00001000;
        const BREAK             = 0b00010000;
        const BREAK2            = 0b00100000;
        const OVERFLOW          = 0b01000000;
        const NEGATIV           = 0b10000000;
    }
}

const STACK: u16 = 0x0100;
const STACK_RESET: u8 = 0xfd;

#[derive(Debug)]
#[allow(non_camel_case_types)]
pub enum AddressingMode {
    Immediate,
    ZeroPage,
    ZeroPage_X,
    ZeroPage_Y,
    Absolute,
    Absolute_X,
    Absolute_Y,
    Indirect_X,
    Indirect_Y,
    NoneAddressing,
}

pub struct CPU<'a> {
    pub register_a: u8,
    pub register_x: u8,
    pub register_y: u8,
    pub status: CpuFlags,
    pub program_counter: u16,
    pub stack_pointer: u8,
    pub bus: Bus<'a>,
    additional_cycles: u8,
    // Number of real bus accesses issued by the instruction currently
    // executing. Each access advances the machine one CPU cycle, so at the end
    // of the instruction the remaining (internal, non-bus) cycles are the
    // difference between the opcode's cycle budget and this count.
    instr_accesses: u8,
    // True only while an instruction is being executed inside the run loop.
    // Memory accesses tick the PPU/APU one cycle each while set; inspection
    // accesses from the trace formatter, test setup, and assertions leave it
    // clear so they do not advance time.
    executing: bool,
    // Interrupt lines sampled at the end of every CPU cycle. NMI is edge
    // detected and latched sticky in `nmi_pending` until serviced; IRQ is a
    // level read into `irq_line`. The `_delayed` copies hold the value as of the
    // *previous* cycle, so an instruction boundary recognizes an interrupt only
    // if it was pending at the end of the penultimate cycle -- reproducing the
    // 6502's poll point and the taken-branch "ignores IRQ on its last cycle"
    // quirk.
    nmi_pending: bool,
    nmi_pending_delayed: bool,
    irq_line: bool,
    irq_line_delayed: bool,
    // Branch instructions poll interrupts at a fixed point -- the end of their
    // second cycle (the offset fetch) -- rather than at the generic penultimate
    // cycle. When a branch executes it latches its (nmi, irq) poll here so the
    // instruction-boundary recognition uses that point instead of `_delayed`.
    // This is the taken-branch IRQ-delay quirk `branch_delays_irq` checks.
    branch_poll: Option<(bool, bool)>,
    // RDY takes effect on the modeled core read after the DMA schedule point.
    // The token records which request/phase owns that slot instead of leaving
    // the one-slot deferral implicit in a boolean.
    dmc_held_read: Option<DmcHeldRead>,
    // When true, executing BRK ($00) returns from the run loop instead of
    // taking the interrupt. This core uses BRK as the halt sentinel for
    // nestest automation and the small `load_and_run` unit-test programs; test
    // ROMs that exercise real BRK/IRQ semantics clear it.
    halt_on_brk: bool,
}

pub struct CpuSnapshot {
    register_a: u8,
    register_x: u8,
    register_y: u8,
    status: CpuFlags,
    program_counter: u16,
    stack_pointer: u8,
    additional_cycles: u8,
    nmi_pending: bool,
    nmi_pending_delayed: bool,
    irq_line: bool,
    irq_line_delayed: bool,
    dmc_held_read: Option<DmcHeldRead>,
    bus: BusSnapshot,
}

pub trait Mem {
    fn mem_read(&mut self, addr: u16) -> u8;

    fn mem_write(&mut self, addr: u16, data: u8);

    fn mem_read_u16(&mut self, pos: u16) -> u16 {
        let lo = self.mem_read(pos) as u16;
        let hi = self.mem_read(pos + 1) as u16;
        (hi << 8) | (lo as u16)
    }

    fn mem_write_u16(&mut self, pos: u16, data: u16) {
        let hi = (data >> 8) as u8;
        let lo = (data & 0xff) as u8;
        self.mem_write(pos, lo);
        self.mem_write(pos + 1, hi);
    }
}

impl Mem for CPU<'_> {
    fn mem_read(&mut self, addr: u16) -> u8 {
        // RDY is sampled between core bus slots. Service a scheduled DMC DMA
        // now, then apply its held/repeated address to the next CPU read slot.
        // Inspection reads neither tick nor participate in DMA.
        if self.executing {
            if let Some(held_read) = self.dmc_held_read.take() {
                self.bus.repeat_dmc_halted_read(addr, held_read);
            }
            if let Some(halt) = self.bus.schedule_dmc_halt() {
                self.latch_halted_interrupts(halt.interrupt_samples);
                self.dmc_held_read = Some(halt.held_read);
            }
        }
        let value = self.bus.mem_read(addr);
        self.bus_cycle();
        value
    }

    fn mem_write(&mut self, addr: u16, data: u8) {
        self.bus.mem_write(addr, data);
        self.bus_cycle();
    }

    // The u16 helpers deliberately fall back to the trait default so each byte
    // is its own ticking `mem_read`/`mem_write`, matching the two real bus
    // cycles the 6502 spends on a 16-bit access.
}

impl<'a> CPU<'a> {
    pub fn new(bus: Bus<'a>) -> Self {
        CPU {
            register_a: 0,
            register_x: 0,
            register_y: 0,
            status: CpuFlags::from_bits_truncate(0b100100),
            program_counter: 0x8000,
            stack_pointer: STACK_RESET,
            bus,
            additional_cycles: 0,
            instr_accesses: 0,
            executing: false,
            nmi_pending: false,
            nmi_pending_delayed: false,
            irq_line: false,
            irq_line_delayed: false,
            branch_poll: None,
            dmc_held_read: None,
            halt_on_brk: true,
        }
    }

    // Advance the machine exactly one CPU cycle and, at the end of it, sample
    // the interrupt lines. The `_delayed` copies are shifted first so they hold
    // the state as of the previous cycle; a subsequent instruction boundary that
    // reads them therefore sees the penultimate-cycle poll point. NMI is edge
    // detected (the PPU hands out each vblank edge once) and latched sticky.
    #[inline]
    fn cycle(&mut self) {
        let samples = self.bus.tick(1);
        self.latch_cycle_batch(samples);
    }

    #[inline]
    fn latch_cycle_batch(&mut self, samples: InterruptBatch) {
        if samples.cycles == 0 {
            return;
        }
        // The first sample belongs to the CPU core cycle. Any remaining
        // samples occurred while RDY held the CPU for DMA: they update the
        // live lines but do not advance the instruction poll pipeline.
        self.nmi_pending_delayed = self.nmi_pending;
        if samples.nmi_first {
            self.nmi_pending = true;
        }
        self.irq_line_delayed = self.irq_line;
        self.irq_line = samples.irq_first;
        if samples.cycles > 1 {
            self.latch_halted_interrupts(samples);
        }
    }

    #[inline]
    fn latch_halted_interrupts(&mut self, samples: InterruptBatch) {
        if samples.nmi_any {
            self.nmi_pending = true;
        }
        self.irq_line = samples.irq_last;
    }

    // Advance the machine one CPU cycle for a bus access performed while an
    // instruction is executing, and count it toward the instruction's access
    // total. Accesses outside instruction execution (trace formatting, test
    // setup, and assertions) leave `executing` clear and do not tick.
    #[inline]
    fn bus_cycle(&mut self) {
        if self.executing {
            self.instr_accesses = self.instr_accesses.saturating_add(1);
            self.cycle();
        }
    }

    // Tick the internal (non-bus-access) cycles that remain after an
    // instruction's real accesses, keeping the total exactly the opcode's cycle
    // budget plus any page-cross/branch penalty. Internal cycles never touch an
    // externally visible register, so batching them at the end is cycle-exact
    // for PPU/APU-visible timing.
    #[inline]
    fn finish_instruction(&mut self, base_cycles: u8) {
        let expected = base_cycles as i16 + self.additional_cycles as i16;
        let remaining = expected - self.instr_accesses as i16;
        debug_assert!(
            remaining >= 0,
            "instruction issued {} accesses but budget is {}",
            self.instr_accesses,
            expected
        );
        for _ in 0..remaining.max(0) {
            self.cycle();
        }
    }

    // Opt into real BRK/IRQ semantics (used by CPU test ROMs). With this set,
    // BRK pushes the return address and status, sets the interrupt-disable
    // flag, and vectors through $FFFE instead of halting the run loop.
    pub fn set_halt_on_brk(&mut self, halt: bool) {
        self.halt_on_brk = halt;
    }

    pub fn snapshot(&self) -> CpuSnapshot {
        CpuSnapshot {
            register_a: self.register_a,
            register_x: self.register_x,
            register_y: self.register_y,
            status: self.status,
            program_counter: self.program_counter,
            stack_pointer: self.stack_pointer,
            additional_cycles: self.additional_cycles,
            nmi_pending: self.nmi_pending,
            nmi_pending_delayed: self.nmi_pending_delayed,
            irq_line: self.irq_line,
            irq_line_delayed: self.irq_line_delayed,
            dmc_held_read: self.dmc_held_read,
            bus: self.bus.snapshot(),
        }
    }

    pub fn restore(&mut self, snapshot: CpuSnapshot) {
        self.register_a = snapshot.register_a;
        self.register_x = snapshot.register_x;
        self.register_y = snapshot.register_y;
        self.status = snapshot.status;
        self.program_counter = snapshot.program_counter;
        self.stack_pointer = snapshot.stack_pointer;
        self.additional_cycles = snapshot.additional_cycles;
        self.nmi_pending = snapshot.nmi_pending;
        self.nmi_pending_delayed = snapshot.nmi_pending_delayed;
        self.irq_line = snapshot.irq_line;
        self.irq_line_delayed = snapshot.irq_line_delayed;
        self.dmc_held_read = snapshot.dmc_held_read;
        self.bus.restore(snapshot.bus);
    }

    pub fn get_absolute_address(&mut self, mode: &AddressingMode, addr: u16) -> u16 {
        match mode {
            AddressingMode::ZeroPage => self.mem_read(addr) as u16,

            AddressingMode::Absolute => self.mem_read_u16(addr),

            AddressingMode::ZeroPage_X => {
                let pos = self.mem_read(addr);
                let addr = pos.wrapping_add(self.register_x) as u16;
                addr
            }
            AddressingMode::ZeroPage_Y => {
                let pos = self.mem_read(addr);
                let addr = pos.wrapping_add(self.register_y) as u16;
                addr
            }

            AddressingMode::Absolute_X => {
                let base = self.mem_read_u16(addr);
                let addr = base.wrapping_add(self.register_x as u16);
                addr
            }

            AddressingMode::Absolute_Y => {
                let base = self.mem_read_u16(addr);
                let addr = base.wrapping_add(self.register_y as u16);
                addr
            }

            AddressingMode::Indirect_X => {
                let base = self.mem_read(addr);

                let ptr: u8 = (base as u8).wrapping_add(self.register_x);
                let lo = self.mem_read(ptr as u16);
                let hi = self.mem_read(ptr.wrapping_add(1) as u16);

                (hi as u16) << 8 | (lo as u16)
            }

            AddressingMode::Indirect_Y => {
                let base = self.mem_read(addr);

                let lo = self.mem_read(base as u16);
                let hi = self.mem_read((base as u8).wrapping_add(1) as u16);
                let deref_base = (hi as u16) << 8 | (lo as u16);
                let deref = deref_base.wrapping_add(self.register_y as u16);
                deref
            }

            _ => {
                panic!("mode {:?} is not supported", mode);
            }
        }
    }

    fn get_operand_addressing(&mut self, mode: &AddressingMode) -> u16 {
        match mode {
            AddressingMode::Immediate => self.program_counter,
            _ => self.get_absolute_address(mode, self.program_counter),
        }
    }

    // Indexed stores have fixed timing but always perform a dummy read at the
    // address formed with the un-carried high byte before writing. Like the
    // indexed-read dummy read, this is observable on hardware registers.
    fn indexed_write_addr(&mut self, base: u16, index: u8) -> u16 {
        let addr = base.wrapping_add(index as u16);
        let dummy = (base & 0xff00) | (addr & 0x00ff);
        let _ = self.mem_read(dummy);
        addr
    }

    // Effective address for a store, issuing the indexed-store dummy read.
    fn get_write_operand_addressing(&mut self, mode: &AddressingMode) -> u16 {
        match mode {
            AddressingMode::Absolute_X => {
                let base = self.mem_read_u16(self.program_counter);
                self.indexed_write_addr(base, self.register_x)
            }
            AddressingMode::Absolute_Y => {
                let base = self.mem_read_u16(self.program_counter);
                self.indexed_write_addr(base, self.register_y)
            }
            AddressingMode::Indirect_Y => {
                let base = self.mem_read(self.program_counter);
                let lo = self.mem_read(base as u16);
                let hi = self.mem_read(base.wrapping_add(1) as u16);
                let pointer = (hi as u16) << 8 | lo as u16;
                self.indexed_write_addr(pointer, self.register_y)
            }
            _ => self.get_operand_addressing(mode),
        }
    }

    // Add an index to a base address for a value read. On a page cross the
    // 6502 spends an extra cycle and, crucially, first performs a *dummy read*
    // from the address formed with the un-carried high byte. That access is
    // visible when it lands on a hardware register (e.g. reading $2007 twice),
    // so we issue it rather than only counting the cycle.
    fn indexed_read_addr(&mut self, base: u16, index: u8) -> u16 {
        let addr = base.wrapping_add(index as u16);
        if (base & 0xff00) != (addr & 0xff00) {
            self.additional_cycles += 1;
            let dummy = (base & 0xff00) | (addr & 0x00ff);
            let _ = self.mem_read(dummy);
        }
        addr
    }

    // Indexed reads take one additional cycle when their effective address
    // crosses a page. Indexed stores and read-modify-write instructions have
    // fixed timings, so only value-reading operations use this helper.
    fn get_read_operand_addressing(&mut self, mode: &AddressingMode) -> u16 {
        match mode {
            AddressingMode::Absolute_X => {
                let base = self.mem_read_u16(self.program_counter);
                self.indexed_read_addr(base, self.register_x)
            }
            AddressingMode::Absolute_Y => {
                let base = self.mem_read_u16(self.program_counter);
                self.indexed_read_addr(base, self.register_y)
            }
            AddressingMode::Indirect_Y => {
                let base = self.mem_read(self.program_counter);
                let lo = self.mem_read(base as u16);
                let hi = self.mem_read(base.wrapping_add(1) as u16);
                let pointer = (hi as u16) << 8 | lo as u16;
                self.indexed_read_addr(pointer, self.register_y)
            }
            _ => self.get_operand_addressing(mode),
        }
    }

    fn ldy(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.register_y = data;
        self.update_zero_and_negative_flags(self.register_y);
    }

    fn ldx(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.register_x = data;
        self.update_zero_and_negative_flags(self.register_x);
    }

    fn lda(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(&mode);
        let value = self.mem_read(addr);
        self.set_register_a(value);
    }

    fn sta(&mut self, mode: &AddressingMode) {
        let addr = self.get_write_operand_addressing(mode);
        self.mem_write(addr, self.register_a);
    }

    fn set_register_a(&mut self, value: u8) {
        self.register_a = value;
        self.update_zero_and_negative_flags(self.register_a);
    }

    fn and(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.set_register_a(data & self.register_a);
    }

    fn eor(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.set_register_a(data ^ self.register_a);
    }

    fn ora(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.set_register_a(data | self.register_a);
    }

    fn tax(&mut self) {
        self.register_x = self.register_a;
        self.update_zero_and_negative_flags(self.register_x);
    }

    fn add_to_register_a(&mut self, data: u8) {
        let sum = self.register_a as u16
            + data as u16
            + (if self.status.contains(CpuFlags::CARRY) {
                1
            } else {
                0
            }) as u16;

        let carry = sum > 0xff;

        if carry {
            self.status.insert(CpuFlags::CARRY);
        } else {
            self.status.remove(CpuFlags::CARRY);
        }

        let result = sum as u8;

        if (data ^ result) & (result ^ self.register_a) & 0x80 != 0 {
            self.status.insert(CpuFlags::OVERFLOW);
        } else {
            self.status.remove(CpuFlags::OVERFLOW)
        }

        self.set_register_a(result);
    }

    fn sbc(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(&mode);
        let data = self.mem_read(addr);
        self.add_to_register_a(((data as i8).wrapping_neg().wrapping_sub(1)) as u8);
    }

    fn adc(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let value = self.mem_read(addr);
        self.add_to_register_a(value);
    }

    fn stack_push(&mut self, data: u8) {
        self.mem_write((STACK as u16) + self.stack_pointer as u16, data);
        self.stack_pointer = self.stack_pointer.wrapping_sub(1)
    }

    fn stack_push_u16(&mut self, data: u16) {
        let hi = (data >> 8) as u8;
        let lo = (data & 0xff) as u8;
        self.stack_push(hi);
        self.stack_push(lo);
    }

    fn stack_pop_u16(&mut self) -> u16 {
        let lo = self.stack_pop() as u16;
        let hi = self.stack_pop() as u16;

        hi << 8 | lo
    }

    fn asl_accumulator(&mut self) {
        let mut data = self.register_a;
        if data >> 7 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data << 1;
        self.set_register_a(data)
    }

    fn asl(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        if data >> 7 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data << 1;
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn lsr_accumulator(&mut self) {
        let mut data = self.register_a;
        if data & 1 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data >> 1;
        self.set_register_a(data)
    }

    fn lsr(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        if data & 1 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data >> 1;
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn rol(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        let old_carry = self.status.contains(CpuFlags::CARRY);

        if data >> 7 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data << 1;
        if old_carry {
            data = data | 1;
        }
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn rol_accumulator(&mut self) {
        let mut data = self.register_a;
        let old_carry = self.status.contains(CpuFlags::CARRY);

        if data >> 7 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data << 1;
        if old_carry {
            data = data | 1;
        }
        self.set_register_a(data);
    }

    fn ror(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        let old_carry = self.status.contains(CpuFlags::CARRY);

        if data & 1 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data >> 1;
        if old_carry {
            data = data | 0b10000000;
        }
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn ror_accumulator(&mut self) {
        let mut data = self.register_a;
        let old_carry = self.status.contains(CpuFlags::CARRY);

        if data & 1 == 1 {
            self.set_carry_flag();
        } else {
            self.clear_carry_flag();
        }
        data = data >> 1;
        if old_carry {
            data = data | 0b10000000;
        }
        self.set_register_a(data);
    }

    // ---- Undocumented ("illegal") opcodes -------------------------------
    // The combination read-modify-write opcodes reuse the legal building
    // blocks (`asl`/`rol`/`lsr`/`ror`), which already perform the fixed-timing
    // `get_operand_addressing` access, the memory write-back, and the carry
    // update, then fold the second operation into the accumulator.

    // LAX: load both A and X from memory (LDA + LDX).
    fn lax(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        self.register_a = data;
        self.register_x = data;
        self.update_zero_and_negative_flags(data);
    }

    // SAX: store A & X. Affects no flags.
    fn sax(&mut self, mode: &AddressingMode) {
        let addr = self.get_operand_addressing(mode);
        self.mem_write(addr, self.register_a & self.register_x);
    }

    // Undocumented NOPs still fetch their operand, so indexed-absolute forms
    // take the page-cross penalty and every form drives the bus.
    fn nop_read(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let _ = self.mem_read(addr);
    }

    // DCP: decrement memory, then CMP it against A.
    fn dcp(&mut self, mode: &AddressingMode) {
        let addr = self.get_write_operand_addressing(mode);
        let data = self.mem_read(addr).wrapping_sub(1);
        self.mem_write(addr, data);
        self.status.set(CpuFlags::CARRY, data <= self.register_a);
        self.update_zero_and_negative_flags(self.register_a.wrapping_sub(data));
    }

    // ISB (ISC): increment memory, then SBC it from A.
    fn isb(&mut self, mode: &AddressingMode) {
        let addr = self.get_write_operand_addressing(mode);
        let data = self.mem_read(addr).wrapping_add(1);
        self.mem_write(addr, data);
        self.add_to_register_a((data as i8).wrapping_neg().wrapping_sub(1) as u8);
    }

    // SLO: ASL memory, then ORA into A.
    fn slo(&mut self, mode: &AddressingMode) {
        let data = self.asl(mode);
        self.set_register_a(self.register_a | data);
    }

    // RLA: ROL memory, then AND into A.
    fn rla(&mut self, mode: &AddressingMode) {
        let data = self.rol(mode);
        self.set_register_a(self.register_a & data);
    }

    // SRE: LSR memory, then EOR into A.
    fn sre(&mut self, mode: &AddressingMode) {
        let data = self.lsr(mode);
        self.set_register_a(self.register_a ^ data);
    }

    // RRA: ROR memory, then ADC into A. ROR leaves the shifted-out bit in
    // carry, which the ADC then consumes.
    fn rra(&mut self, mode: &AddressingMode) {
        let data = self.ror(mode);
        self.add_to_register_a(data);
    }

    // ANC: AND immediate, then copy bit 7 of the result into carry.
    fn anc(&mut self) {
        let data = self.mem_read(self.program_counter);
        self.set_register_a(self.register_a & data);
        self.status.set(CpuFlags::CARRY, self.register_a & 0x80 != 0);
    }

    // ALR (ASR): AND immediate, then LSR the accumulator.
    fn alr(&mut self) {
        let data = self.mem_read(self.program_counter);
        let value = self.register_a & data;
        self.status.set(CpuFlags::CARRY, value & 1 != 0);
        self.set_register_a(value >> 1);
    }

    // ARR: AND immediate, rotate right through carry, then derive C and V
    // from the two high bits of the result.
    fn arr(&mut self) {
        let data = self.mem_read(self.program_counter);
        let value = self.register_a & data;
        let carry_in = self.status.contains(CpuFlags::CARRY) as u8;
        let result = (value >> 1) | (carry_in << 7);
        self.set_register_a(result);
        self.status.set(CpuFlags::CARRY, result & 0x40 != 0);
        self.status.set(
            CpuFlags::OVERFLOW,
            ((result >> 6) & 1) ^ ((result >> 5) & 1) != 0,
        );
    }

    // AXS (SBX): X = (A & X) - immediate, with carry set like a compare.
    fn axs(&mut self) {
        let data = self.mem_read(self.program_counter);
        let base = self.register_a & self.register_x;
        self.status.set(CpuFlags::CARRY, base >= data);
        self.register_x = base.wrapping_sub(data);
        self.update_zero_and_negative_flags(self.register_x);
    }

    // ANE (XAA) and LXA are unstable: on real hardware the result depends on a
    // fluctuating analog "magic constant" ORed into the accumulator before the
    // AND. blargg's instr_test-v5 was captured with that constant reading as
    // all-ones, so 0xFF reproduces its expected results; no commercial NES game
    // relies on the exact value.
    const MAGIC: u8 = 0xff;

    fn ane(&mut self) {
        let data = self.mem_read(self.program_counter);
        self.set_register_a((self.register_a | Self::MAGIC) & self.register_x & data);
    }

    fn lxa(&mut self) {
        let data = self.mem_read(self.program_counter);
        let value = (self.register_a | Self::MAGIC) & data;
        self.register_a = value;
        self.register_x = value;
        self.update_zero_and_negative_flags(value);
    }

    // The base address and index register for an indexed store, without the
    // page-cross timing penalty (these opcodes have fixed timing).
    fn indexed_base(&mut self, mode: &AddressingMode) -> (u16, u8) {
        match mode {
            AddressingMode::Absolute_X => {
                (self.mem_read_u16(self.program_counter), self.register_x)
            }
            AddressingMode::Absolute_Y => {
                (self.mem_read_u16(self.program_counter), self.register_y)
            }
            AddressingMode::Indirect_Y => {
                let ptr = self.mem_read(self.program_counter);
                let lo = self.mem_read(ptr as u16);
                let hi = self.mem_read(ptr.wrapping_add(1) as u16);
                ((hi as u16) << 8 | lo as u16, self.register_y)
            }
            _ => unreachable!("unstable store with unexpected mode {:?}", mode),
        }
    }

    // The unstable store opcodes AND the source register with (high byte of the
    // base address + 1). When indexing crosses a page, the high byte of the
    // effective address is itself replaced by the stored value -- the hardware
    // quirk blargg's instr_test checks for.
    fn sh_store(&mut self, mode: &AddressingMode, value_from_high: impl Fn(&Self, u8) -> u8) {
        let (base, index) = self.indexed_base(mode);
        let high = (base >> 8) as u8;
        let value = value_from_high(self, high.wrapping_add(1));
        let effective = base.wrapping_add(index as u16);
        // Like any indexed store, the dummy read at the un-carried address
        // still happens (observable on APU registers).
        let dummy = (base & 0xff00) | (effective & 0x00ff);
        let _ = self.mem_read(dummy);
        let target = if (base & 0xff00) != (effective & 0xff00) {
            (effective & 0x00ff) | ((value as u16) << 8)
        } else {
            effective
        };
        self.mem_write(target, value);
    }

    // LAS: A, X and SP all receive (memory & SP).
    fn las(&mut self, mode: &AddressingMode) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr) & self.stack_pointer;
        self.register_a = data;
        self.register_x = data;
        self.stack_pointer = data;
        self.update_zero_and_negative_flags(data);
    }

    fn stack_pop(&mut self) -> u8 {
        self.stack_pointer = self.stack_pointer.wrapping_add(1);
        self.mem_read((STACK as u16) + self.stack_pointer as u16)
    }

    fn inx(&mut self) {
        self.register_x = self.register_x.wrapping_add(1);
        self.update_zero_and_negative_flags(self.register_x);
    }

    fn iny(&mut self) {
        self.register_y = self.register_y.wrapping_add(1);
        self.update_zero_and_negative_flags(self.register_y);
    }

    pub fn reset(&mut self) {
        self.register_a = 0;
        self.register_x = 0;
        self.register_y = 0;
        self.stack_pointer = STACK_RESET;
        self.status = CpuFlags::from_bits_truncate(0b100100);
        self.dmc_held_read = None;

        self.program_counter = self.mem_read_u16(0xFFFC);
    }

    fn set_carry_flag(&mut self) {
        self.status.insert(CpuFlags::CARRY)
    }

    fn clear_carry_flag(&mut self) {
        self.status.remove(CpuFlags::CARRY)
    }

    pub fn load_and_run(&mut self, program: Vec<u8>) {
        self.load(program);
        self.program_counter = self.mem_read_u16(0xFFFC);
        self.run()
    }

    pub fn load(&mut self, program: Vec<u8>) {
        // memory code
        // self.memory[0x0600..(0x0600 + program.len())].copy_from_slice(&program[..]);
        // self.mem_write_u16(0xFFFC, 0x0600);
        // temporarily adjust load fcn to load our test programs to vram.

        // bus code (bugged)
        // for i in 0..(program.len() as u16) {
        //     self.mem_write(0x0000 + i, program[i as usize]);
        // }
        // self.mem_write_u16(0xFFFC, 0x0000);

        let start = 0x0600;
        for (i, byte) in program.iter().enumerate() {
            self.mem_write(start + i as u16, *byte);
        }
        self.program_counter = start;
    }

    fn inc(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        data = data.wrapping_add(1);
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn dey(&mut self) {
        self.register_y = self.register_y.wrapping_sub(1);
        self.update_zero_and_negative_flags(self.register_y);
    }

    fn dex(&mut self) {
        self.register_x = self.register_x.wrapping_sub(1);
        self.update_zero_and_negative_flags(self.register_x);
    }

    fn dec(&mut self, mode: &AddressingMode) -> u8 {
        let addr = self.get_write_operand_addressing(mode);
        let mut data = self.mem_read(addr);
        data = data.wrapping_sub(1);
        self.mem_write(addr, data);
        self.update_zero_and_negative_flags(data);
        data
    }

    fn pla(&mut self) {
        let data = self.stack_pop();
        self.set_register_a(data);
    }

    fn plp(&mut self) {
        self.status.bits = self.stack_pop();
        self.status.remove(CpuFlags::BREAK);
        self.status.insert(CpuFlags::BREAK2);
    }

    fn php(&mut self) {
        //http://wiki.nesdev.com/w/index.php/CPU_status_flag_behavior
        let mut flags = self.status.clone();
        flags.insert(CpuFlags::BREAK);
        flags.insert(CpuFlags::BREAK2);
        self.stack_push(flags.bits());
    }

    fn bit(&mut self, mode: &AddressingMode) {
        let addr = self.get_operand_addressing(mode);
        let data = self.mem_read(addr);
        let and = self.register_a & data;
        if and == 0 {
            self.status.insert(CpuFlags::ZERO);
        } else {
            self.status.remove(CpuFlags::ZERO);
        }

        self.status.set(CpuFlags::NEGATIV, data & 0b10000000 > 0);
        self.status.set(CpuFlags::OVERFLOW, data & 0b01000000 > 0);
    }

    fn compare(&mut self, mode: &AddressingMode, compare_with: u8) {
        let addr = self.get_read_operand_addressing(mode);
        let data = self.mem_read(addr);
        if data <= compare_with {
            self.status.insert(CpuFlags::CARRY);
        } else {
            self.status.remove(CpuFlags::CARRY);
        }

        self.update_zero_and_negative_flags(compare_with.wrapping_sub(data));
    }

    fn branch(&mut self, condition: bool) {
        // The offset is fetched on cycle 2 whether or not the branch is taken;
        // interrupts are polled at the end of that cycle for every branch. That
        // makes a not-taken branch poll on its last cycle and a taken
        // page-crossing branch poll two cycles before its end -- neither of
        // which is the generic penultimate-cycle poll.
        let old_pc = self.program_counter.wrapping_add(1);
        let jump: i8 = self.mem_read(self.program_counter) as i8;
        self.branch_poll = Some((self.nmi_pending, self.irq_line));

        if condition {
            let jump_addr = old_pc.wrapping_add(jump as u16);
            self.program_counter = jump_addr;
            self.additional_cycles += 1;
            if (old_pc & 0xff00) != (jump_addr & 0xff00) {
                self.additional_cycles += 1;
            }
        }
    }

    // The seven-cycle interrupt sequence shared by BRK, IRQ, and NMI. Every
    // access ticks a real cycle, so the PPU/APU advance through the pushes and
    // vector fetch. The vector is chosen *after* the status push (the hardware's
    // cycle-5 poll point): a pending NMI at that moment steals the vector even
    // when the sequence began as a BRK or an IRQ, which is the NMI-hijacking the
    // `cpu_interrupts_v2` tests check. `brk` selects the B flag on the pushed
    // status and the two-byte return address past the signature byte.
    fn service_interrupt(&mut self, brk: bool) {
        if brk {
            // Dummy fetch of the signature byte; the return address skips it.
            let _ = self.mem_read(self.program_counter);
            self.program_counter = self.program_counter.wrapping_add(1);
        } else {
            // Two dummy reads of the preempted instruction's opcode.
            let _ = self.mem_read(self.program_counter);
            let _ = self.mem_read(self.program_counter);
        }

        self.stack_push_u16(self.program_counter);

        // Late vector decision (the hardware latches the interrupt source around
        // the cycle that pushes PCL): a latched NMI wins and is consumed here,
        // stealing the vector even from a BRK or IRQ that started the sequence.
        let vector = if self.nmi_pending {
            self.nmi_pending = false;
            self.nmi_pending_delayed = false;
            0xfffa
        } else {
            0xfffe
        };

        let mut flag = self.status.clone();
        flag.set(CpuFlags::BREAK, brk);
        flag.set(CpuFlags::BREAK2, true);
        self.stack_push(flag.bits());
        self.status.insert(CpuFlags::INTERRUPT_DISABLE);

        let lo = self.mem_read(vector) as u16;
        let hi = self.mem_read(vector + 1) as u16;
        self.program_counter = (hi << 8) | lo;
    }

    pub fn run(&mut self) {
        self.run_with_callback(|_| {});
    }

    pub fn run_with_callback<F>(&mut self, mut callback: F)
    where
        F: FnMut(&mut CPU),
    {
        self.run_until(|cpu| {
            callback(cpu);
            false
        });
    }

    /// Run to the next host-facing vblank boundary. The bus raises this event
    /// independently of NMI enable; execution stops before any NMI-handler
    /// instruction can poll the controller.
    pub fn run_until_frame_ready(&mut self) {
        self.run_until(|cpu| cpu.bus.take_host_frame_ready());
    }

    /// Run until `callback` asks the CPU to stop. This is primarily useful for
    /// deterministic automation (for example, stopping a ROM probe at an exact
    /// video frame) without terminating the whole process from a bus callback.
    pub fn run_until<F>(&mut self, mut callback: F)
    where
        F: FnMut(&mut CPU) -> bool,
    {
        // we borrow the opscode data structure without taking ownership
        let ref opcodes: HashMap<u8, &'static opcodes::OpCode> = *opcodes::OPCODES_MAP;

        loop {
            // The trace formatter and any stop condition run with `executing`
            // clear so their memory reads do not advance the PPU/APU.
            self.executing = false;
            if callback(self) {
                return;
            }

            // Honor the BRK halt sentinel before spending a cycle on the fetch:
            // the sentinel is an automation stop marker, not a real executed
            // instruction, so it must not advance the machine. The peek reads
            // the opcode from code space (never a side-effecting register) and
            // does not tick.
            if self.halt_on_brk && self.bus.mem_read(self.program_counter) == 0x00 {
                // Match the historical halt convention: PC stops one past the
                // sentinel byte, but no cycle is consumed.
                self.program_counter = self.program_counter.wrapping_add(1);
                self.executing = false;
                return;
            }

            // Each real bus access from here on ticks one CPU cycle and counts
            // toward the instruction's access total; `finish_instruction` ticks
            // whatever internal cycles remain in the opcode's budget.
            self.executing = true;
            self.instr_accesses = 0;
            let code = self.mem_read(self.program_counter);
            self.program_counter += 1;
            let program_counter_state = self.program_counter;
            let opcode = opcodes.get(&code).unwrap();
            self.additional_cycles = 0;
            // The maskable IRQ is recognized using the interrupt-disable flag as
            // it stood *before* this instruction runs, so CLI, SEI, and PLP take
            // effect one instruction late -- the 6502 polls interrupts on an
            // instruction's penultimate cycle, before such a flag write lands.
            let irq_disabled_before = self.status.contains(CpuFlags::INTERRUPT_DISABLE);

            // BRK as a real software interrupt (test ROMs clear `halt_on_brk`).
            // The seven-cycle sequence runs entirely as ticking accesses and may
            // have its vector hijacked by a pending NMI. The interrupt handler's
            // first instruction runs before interrupts are polled again, so no
            // poll happens here.
            if code == 0x00 {
                self.service_interrupt(true);
                continue;
            }

            match code {
                // LDA
                0xa9 | 0xa5 | 0xb5 | 0xad | 0xbd | 0xb9 | 0xa1 | 0xb1 => {
                    self.lda(&opcode.mode);
                }

                // STA
                0x85 | 0x95 | 0x8d | 0x9d | 0x99 | 0x81 | 0x91 => {
                    self.sta(&opcode.mode);
                }

                0xaa => self.tax(),
                0xe8 => self.inx(),

                /* CLD */ 0xd8 => self.status.remove(CpuFlags::DECIMAL_MODE),

                /* CLI */ 0x58 => self.status.remove(CpuFlags::INTERRUPT_DISABLE),

                /* CLV */ 0xb8 => self.status.remove(CpuFlags::OVERFLOW),

                /* CLC */ 0x18 => self.clear_carry_flag(),

                /* SEC */ 0x38 => self.set_carry_flag(),

                /* SEI */ 0x78 => self.status.insert(CpuFlags::INTERRUPT_DISABLE),

                /* SED */ 0xf8 => self.status.insert(CpuFlags::DECIMAL_MODE),

                /* PHA */ 0x48 => self.stack_push(self.register_a),

                /* PLA */
                0x68 => {
                    self.pla();
                }

                /* PHP */
                0x08 => {
                    self.php();
                }

                /* PLP */
                0x28 => {
                    self.plp();
                }

                /* ADC */
                0x69 | 0x65 | 0x75 | 0x6d | 0x7d | 0x79 | 0x61 | 0x71 => {
                    self.adc(&opcode.mode);
                }

                /* SBC */
                0xe9 | 0xe5 | 0xf5 | 0xed | 0xfd | 0xf9 | 0xe1 | 0xf1 => {
                    self.sbc(&opcode.mode);
                }

                /* AND */
                0x29 | 0x25 | 0x35 | 0x2d | 0x3d | 0x39 | 0x21 | 0x31 => {
                    self.and(&opcode.mode);
                }

                /* EOR */
                0x49 | 0x45 | 0x55 | 0x4d | 0x5d | 0x59 | 0x41 | 0x51 => {
                    self.eor(&opcode.mode);
                }

                /* ORA */
                0x09 | 0x05 | 0x15 | 0x0d | 0x1d | 0x19 | 0x01 | 0x11 => {
                    self.ora(&opcode.mode);
                }

                /* LSR */ 0x4a => self.lsr_accumulator(),

                /* LSR */
                0x46 | 0x56 | 0x4e | 0x5e => {
                    self.lsr(&opcode.mode);
                }

                /*ASL*/ 0x0a => self.asl_accumulator(),

                /* ASL */
                0x06 | 0x16 | 0x0e | 0x1e => {
                    self.asl(&opcode.mode);
                }

                /*ROL*/ 0x2a => self.rol_accumulator(),

                /* ROL */
                0x26 | 0x36 | 0x2e | 0x3e => {
                    self.rol(&opcode.mode);
                }

                /* ROR */ 0x6a => self.ror_accumulator(),

                /* ROR */
                0x66 | 0x76 | 0x6e | 0x7e => {
                    self.ror(&opcode.mode);
                }

                /* INC */
                0xe6 | 0xf6 | 0xee | 0xfe => {
                    self.inc(&opcode.mode);
                }

                /* INY */
                0xc8 => self.iny(),

                /* DEC */
                0xc6 | 0xd6 | 0xce | 0xde => {
                    self.dec(&opcode.mode);
                }

                /* DEX */
                0xca => {
                    self.dex();
                }

                /* DEY */
                0x88 => {
                    self.dey();
                }

                /* CMP */
                0xc9 | 0xc5 | 0xd5 | 0xcd | 0xdd | 0xd9 | 0xc1 | 0xd1 => {
                    self.compare(&opcode.mode, self.register_a);
                }

                /* CPY */
                0xc0 | 0xc4 | 0xcc => {
                    self.compare(&opcode.mode, self.register_y);
                }

                /* CPX */
                0xe0 | 0xe4 | 0xec => self.compare(&opcode.mode, self.register_x),

                /* JMP Absolute */
                0x4c => {
                    let mem_address = self.mem_read_u16(self.program_counter);
                    self.program_counter = mem_address;
                }

                /* JMP Indirect */
                0x6c => {
                    let mem_address = self.mem_read_u16(self.program_counter);

                    let indirect_ref = if mem_address & 0x00FF == 0x00FF {
                        let lo = self.mem_read(mem_address);
                        let hi = self.mem_read(mem_address & 0xFF00);
                        (hi as u16) << 8 | (lo as u16)
                    } else {
                        self.mem_read_u16(mem_address)
                    };

                    self.program_counter = indirect_ref;
                }

                /* JSR */
                0x20 => {
                    self.stack_push_u16(self.program_counter + 2 - 1);
                    let target_address = self.mem_read_u16(self.program_counter);
                    self.program_counter = target_address
                }

                /* RTS */
                0x60 => {
                    self.program_counter = self.stack_pop_u16() + 1;
                }

                /* RTI */
                0x40 => {
                    self.status.bits = self.stack_pop();
                    self.status.remove(CpuFlags::BREAK);
                    self.status.insert(CpuFlags::BREAK2);

                    self.program_counter = self.stack_pop_u16();
                }

                /* BNE */
                0xd0 => {
                    self.branch(!self.status.contains(CpuFlags::ZERO));
                }

                /* BVS */
                0x70 => {
                    self.branch(self.status.contains(CpuFlags::OVERFLOW));
                }

                /* BVC */
                0x50 => {
                    self.branch(!self.status.contains(CpuFlags::OVERFLOW));
                }

                /* BPL */
                0x10 => {
                    self.branch(!self.status.contains(CpuFlags::NEGATIV));
                }

                /* BMI */
                0x30 => {
                    self.branch(self.status.contains(CpuFlags::NEGATIV));
                }

                /* BEQ */
                0xf0 => {
                    self.branch(self.status.contains(CpuFlags::ZERO));
                }

                /* BCS */
                0xb0 => {
                    self.branch(self.status.contains(CpuFlags::CARRY));
                }

                /* BCC */
                0x90 => {
                    self.branch(!self.status.contains(CpuFlags::CARRY));
                }

                /* BIT */
                0x24 | 0x2c => {
                    self.bit(&opcode.mode);
                }

                /* STX */
                0x86 | 0x96 | 0x8e => {
                    let addr = self.get_operand_addressing(&opcode.mode);
                    self.mem_write(addr, self.register_x);
                }

                /* STY */
                0x84 | 0x94 | 0x8c => {
                    let addr = self.get_operand_addressing(&opcode.mode);
                    self.mem_write(addr, self.register_y);
                }

                /* LDX */
                0xa2 | 0xa6 | 0xb6 | 0xae | 0xbe => {
                    self.ldx(&opcode.mode);
                }

                /* LDY */
                0xa0 | 0xa4 | 0xb4 | 0xac | 0xbc => {
                    self.ldy(&opcode.mode);
                }

                /* NOP */
                0xea => {
                    //do nothing
                }

                /* TAY */
                0xa8 => {
                    self.register_y = self.register_a;
                    self.update_zero_and_negative_flags(self.register_y);
                }

                /* TSX */
                0xba => {
                    self.register_x = self.stack_pointer;
                    self.update_zero_and_negative_flags(self.register_x);
                }

                /* TXA */
                0x8a => {
                    self.register_a = self.register_x;
                    self.update_zero_and_negative_flags(self.register_a);
                }

                /* TXS */
                0x9a => {
                    self.stack_pointer = self.register_x;
                }

                /* TYA */
                0x98 => {
                    self.register_a = self.register_y;
                    self.update_zero_and_negative_flags(self.register_a);
                }

                // ---- Undocumented opcodes -------------------------------

                /* NOP (implied, undocumented) */
                0x1a | 0x3a | 0x5a | 0x7a | 0xda | 0xfa => {}

                /* NOP (immediate / read, undocumented) */
                0x80 | 0x82 | 0x89 | 0xc2 | 0xe2 | 0x04 | 0x44 | 0x64 | 0x14 | 0x34 | 0x54
                | 0x74 | 0xd4 | 0xf4 | 0x0c | 0x1c | 0x3c | 0x5c | 0x7c | 0xdc | 0xfc => {
                    self.nop_read(&opcode.mode);
                }

                /* LAX */
                0xa3 | 0xa7 | 0xaf | 0xb3 | 0xb7 | 0xbf => self.lax(&opcode.mode),

                /* SAX */
                0x83 | 0x87 | 0x8f | 0x97 => self.sax(&opcode.mode),

                /* SBC (undocumented) */
                0xeb => self.sbc(&opcode.mode),

                /* DCP */
                0xc3 | 0xc7 | 0xcf | 0xd3 | 0xd7 | 0xdb | 0xdf => self.dcp(&opcode.mode),

                /* ISB / ISC */
                0xe3 | 0xe7 | 0xef | 0xf3 | 0xf7 | 0xfb | 0xff => self.isb(&opcode.mode),

                /* SLO */
                0x03 | 0x07 | 0x0f | 0x13 | 0x17 | 0x1b | 0x1f => self.slo(&opcode.mode),

                /* RLA */
                0x23 | 0x27 | 0x2f | 0x33 | 0x37 | 0x3b | 0x3f => self.rla(&opcode.mode),

                /* SRE */
                0x43 | 0x47 | 0x4f | 0x53 | 0x57 | 0x5b | 0x5f => self.sre(&opcode.mode),

                /* RRA */
                0x63 | 0x67 | 0x6f | 0x73 | 0x77 | 0x7b | 0x7f => self.rra(&opcode.mode),

                /* ANC */
                0x0b | 0x2b => self.anc(),
                /* ALR (ASR) */
                0x4b => self.alr(),
                /* ARR */
                0x6b => self.arr(),
                /* AXS (SBX) */
                0xcb => self.axs(),
                /* ANE (XAA), unstable */
                0x8b => self.ane(),
                /* LXA, unstable */
                0xab => self.lxa(),

                /* SHY, unstable */
                0x9c => self.sh_store(&opcode.mode, |cpu, high| cpu.register_y & high),
                /* SHX, unstable */
                0x9e => self.sh_store(&opcode.mode, |cpu, high| cpu.register_x & high),
                /* SHA (AHX), unstable */
                0x93 | 0x9f => {
                    self.sh_store(&opcode.mode, |cpu, high| {
                        cpu.register_a & cpu.register_x & high
                    })
                }
                /* TAS (SHS), unstable */
                0x9b => {
                    self.stack_pointer = self.register_a & self.register_x;
                    self.sh_store(&opcode.mode, |cpu, high| {
                        cpu.register_a & cpu.register_x & high
                    });
                }
                /* LAS, unstable */
                0xbb => self.las(&opcode.mode),

                /* JAM / KIL: the processor locks up, refetching this opcode
                 * forever, exactly as the real 6502 does. */
                0x02 | 0x12 | 0x22 | 0x32 | 0x42 | 0x52 | 0x62 | 0x72 | 0x92 | 0xb2 | 0xd2
                | 0xf2 => {
                    self.program_counter = self.program_counter.wrapping_sub(1);
                }

                0x00 => unreachable!(),
            }
            // Real bus accesses have already ticked; tick the internal cycles
            // that remain in the budget, including taken-branch and indexed-read
            // page-cross penalties discovered while executing.
            self.finish_instruction(opcode.cycles);

            if program_counter_state == self.program_counter {
                self.program_counter += (opcode.len - 1) as u16;
            }

            // RTI restores the I flag from the stack immediately, so it does not
            // get the one-instruction delay that CLI/SEI/PLP do; its IRQ poll
            // uses the post-instruction I flag.
            self.poll_interrupts(irq_disabled_before, code == 0x40);
        }
    }

    // Recognize an interrupt latched as of the penultimate cycle (the 6502 poll
    // point). NMI wins over IRQ. IRQ honors the interrupt-disable flag: for most
    // instructions that is the value sampled *before* the instruction, so a
    // CLI/SEI/PLP that changes it is delayed one instruction; RTI's change is
    // immediate, so it uses the post-instruction flag.
    fn poll_interrupts(&mut self, irq_disabled_before: bool, i_flag_immediate: bool) {
        // Branches carry their own fixed poll point; every other instruction is
        // recognized from the penultimate-cycle (`_delayed`) sample.
        let (nmi, irq) = self
            .branch_poll
            .take()
            .unwrap_or((self.nmi_pending_delayed, self.irq_line_delayed));
        if nmi {
            self.service_interrupt(false);
            return;
        }
        let i_flag = if i_flag_immediate {
            self.status.contains(CpuFlags::INTERRUPT_DISABLE)
        } else {
            irq_disabled_before
        };
        if irq && !i_flag {
            self.service_interrupt(false);
        }
    }

    fn update_zero_and_negative_flags(&mut self, result: u8) {
        if result == 0 {
            self.status.insert(CpuFlags::ZERO);
        } else {
            self.status.remove(CpuFlags::ZERO);
        }

        if result >> 7 == 1 {
            self.status.insert(CpuFlags::NEGATIV);
        } else {
            self.status.remove(CpuFlags::NEGATIV);
        }
    }

}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::test;

    #[test]
    fn dmc_rdy_repeats_the_following_core_read_slot() {
        fn controller_shifts(with_dmc: bool) -> u8 {
            let bus = Bus::new(test::test_rom(vec![]), |_, _, _| {});
            let mut cpu = CPU::new(bus);
            cpu.bus
                .joypad_mut()
                .set_button_pressed_status(crate::joypad::JoypadButton::BUTTON_A, true);
            cpu.bus.mem_write(0x4016, 1);
            cpu.bus.mem_write(0x4016, 0);
            if with_dmc {
                cpu.bus.mem_write(0x4012, 0);
                cpu.bus.mem_write(0x4013, 0);
                cpu.bus.mem_write(0x4015, 0x10);
            }

            cpu.executing = true;
            let _ = cpu.mem_read(0x0000);
            assert_eq!(cpu.dmc_held_read.is_some(), with_dmc);
            let _ = cpu.mem_read(0x4016);
            assert!(cpu.dmc_held_read.is_none());
            cpu.executing = false;

            let mut shifts = 1;
            while cpu.bus.mem_read(0x4016) != 1 {
                shifts += 1;
            }
            shifts
        }

        assert_eq!(controller_shifts(true), controller_shifts(false) - 1);
    }

    #[test]
    fn irq_samples_advance_through_oam_dma_stall() {
        let bus = Bus::new(test::test_rom(vec![]), |_, _, _| {});
        let mut cpu = CPU::new(bus);

        // Put the APU one cycle before its frame IRQ, then let the $4014 bus
        // cycle enter OAM DMA. The IRQ asserts at the beginning of the stall;
        // sampling every physical DMA cycle must advance it into both stages
        // of the CPU's interrupt-recognition pipeline.
        for _ in 0..29_827 {
            let _ = cpu.bus.tick(1);
        }
        cpu.executing = true;
        cpu.mem_write(0x4014, 0x00);
        cpu.executing = false;

        assert!(cpu.irq_line);
        assert!(!cpu.irq_line_delayed);

        cpu.executing = true;
        let _ = cpu.mem_read(0x0000);
        cpu.executing = false;
        assert!(cpu.irq_line_delayed);
    }

    #[test]
    fn test_0xa9_lda_immediate_load_data() {
        let bus = Bus::new(test::test_rom(vec![0xa9, 0x05, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);

        cpu.run();

        assert_eq!(cpu.register_a, 5);
        assert!(cpu.status.bits() & 0b0000_0010 == 0b00);
        assert!(cpu.status.bits() & 0b1000_0000 == 0);
    }

    #[test]
    fn test_0xaa_tax_move_a_to_x() {
        let bus = Bus::new(test::test_rom(vec![0xaa, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.register_a = 10;

        cpu.run();

        assert_eq!(cpu.register_x, 10)
    }

    #[test]
    fn test_5_ops_working_together() {
        let bus = Bus::new(test::test_rom(vec![0xa9, 0xc0, 0xaa, 0xe8, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);

        cpu.run();

        assert_eq!(cpu.register_x, 0xc1)
    }

    #[test]
    fn test_inx_overflow() {
        let bus = Bus::new(test::test_rom(vec![0xe8, 0xe8, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.register_x = 0xff;

        cpu.run();

        assert_eq!(cpu.register_x, 1)
    }

    #[test]
    fn test_lda_from_memory() {
        let bus = Bus::new(test::test_rom(vec![0xa5, 0x10, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.mem_write(0x10, 0x55);

        cpu.run();

        assert_eq!(cpu.register_a, 0x55);
    }

    #[test]
    fn test_irq_serviced_after_cli() {
        // CLI enables interrupts; a pending APU IRQ (DMC flag forced here)
        // then fires before the next instruction, vectoring through $FFFE
        // (0x0000 in the zero-filled test ROM, where BRK ends run()).
        let bus = Bus::new(test::test_rom(vec![0x58, 0xea, 0xea, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.bus.apu.dmc.irq_flag = true;

        cpu.run();

        // The IRQ pushed PC and status (3 bytes) and jumped to 0x0000.
        assert_eq!(cpu.stack_pointer, STACK_RESET - 3);
        assert_eq!(cpu.program_counter, 0x0001);
        assert!(cpu.status.contains(CpuFlags::INTERRUPT_DISABLE));
    }

    #[test]
    fn test_irq_masked_by_interrupt_disable() {
        // The power-up status has I set, so the pending IRQ must not fire.
        let bus = Bus::new(test::test_rom(vec![0xea, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.bus.apu.dmc.irq_flag = true;

        cpu.run();

        assert_eq!(cpu.stack_pointer, STACK_RESET);
    }

    #[test]
    fn cli_delays_irq_by_exactly_one_instruction() {
        // CLI, NOP, NOP with a pending IRQ. The interrupt-disable clear is
        // delayed one instruction, so the first NOP executes before the IRQ is
        // taken: the pushed return address is $8002, not $8001.
        let bus = Bus::new(test::test_rom(vec![0x58, 0xea, 0xea, 0xea]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.bus.apu.dmc.irq_flag = true;

        cpu.run();

        let return_lo = cpu.mem_read(0x01fc) as u16;
        let return_hi = cpu.mem_read(0x01fd) as u16;
        assert_eq!(return_hi << 8 | return_lo, 0x8002);
        assert!(cpu.status.contains(CpuFlags::INTERRUPT_DISABLE));
    }

    #[test]
    fn brk_vectors_through_fffe_when_not_halting() {
        // With halt-on-BRK cleared, BRK is a real software interrupt: it pushes
        // three bytes, sets I, and jumps to the $FFFE vector (0x0000 in the
        // zero-filled test ROM). We stop the run right after it vectors, before
        // the $00 at $0000 would loop back into another BRK.
        let bus = Bus::new(test::test_rom(vec![0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.set_halt_on_brk(false);

        let mut steps = 0;
        cpu.run_until(|_| {
            steps += 1;
            steps == 2
        });

        assert_eq!(cpu.program_counter, 0x0000);
        assert_eq!(cpu.stack_pointer, STACK_RESET - 3);
        assert!(cpu.status.contains(CpuFlags::INTERRUPT_DISABLE));
        // BRK pushes the address after its signature byte: $8000 + 2.
        let return_lo = cpu.mem_read(0x01fc) as u16;
        let return_hi = cpu.mem_read(0x01fd) as u16;
        assert_eq!(return_hi << 8 | return_lo, 0x8002);
    }

    #[test]
    fn lax_loads_both_a_and_x() {
        // A7: LAX $10 loads the same value into A and X.
        let bus = Bus::new(test::test_rom(vec![0xa7, 0x10, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.mem_write(0x10, 0x55);

        cpu.run();

        assert_eq!(cpu.register_a, 0x55);
        assert_eq!(cpu.register_x, 0x55);
    }

    #[test]
    fn sax_stores_a_and_x() {
        // 87: SAX $10 stores A & X without touching any flag.
        let bus = Bus::new(test::test_rom(vec![0x87, 0x10, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.register_a = 0xf0;
        cpu.register_x = 0x3c;

        cpu.run();

        assert_eq!(cpu.mem_read(0x10), 0x30);
    }

    #[test]
    fn slo_shifts_memory_then_ors_into_a() {
        // 07: SLO $10 = ASL memory, then ORA into A.
        let bus = Bus::new(test::test_rom(vec![0x07, 0x10, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.register_a = 0x01;
        cpu.mem_write(0x10, 0x40);

        cpu.run();

        assert_eq!(cpu.mem_read(0x10), 0x80); // 0x40 << 1
        assert_eq!(cpu.register_a, 0x81); // 0x01 | 0x80
    }

    #[test]
    fn rol_memory_updates_the_zero_flag() {
        // 26: ROL $10 on 0x80 with carry clear yields 0x00; Z must be set.
        let bus = Bus::new(test::test_rom(vec![0x26, 0x10, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.mem_write(0x10, 0x80);

        cpu.run();

        assert_eq!(cpu.mem_read(0x10), 0x00);
        assert!(cpu.status.contains(CpuFlags::ZERO));
        assert!(cpu.status.contains(CpuFlags::CARRY));
    }

    #[test]
    fn undocumented_nop_consumes_its_operand_and_continues() {
        // 04 (NOP zp) is a two-byte no-op: execution proceeds to the next
        // instruction, which loads A.
        let bus = Bus::new(test::test_rom(vec![0x04, 0x10, 0xa9, 0x42, 0x00]), |_, _, _| {});
        let mut cpu = CPU::new(bus);

        cpu.run();

        assert_eq!(cpu.register_a, 0x42);
    }

    #[test]
    fn taken_branch_adds_cycle_and_page_cross_adds_another() {
        let bus = Bus::new(
            test::test_rom(vec![0xa2, 0x02, 0xca, 0xd0, 0xfd, 0x00]),
            |_, _, _| {},
        );
        let mut cpu = CPU::new(bus);

        cpu.run();

        // LDX (2), DEX (2), taken BNE (3), DEX (2), untaken BNE (2).
        assert_eq!(cpu.bus.cpu_cycles(), 11);
    }

    #[test]
    fn indexed_read_adds_cycle_when_effective_address_crosses_page() {
        let bus = Bus::new(
            test::test_rom(vec![0xa2, 0x01, 0xbd, 0xff, 0x00, 0x00]),
            |_, _, _| {},
        );
        let mut cpu = CPU::new(bus);
        cpu.mem_write(0x0100, 0x55);

        cpu.run();

        assert_eq!(cpu.register_a, 0x55);
        assert_eq!(cpu.bus.cpu_cycles(), 7); // LDX (2) + crossing LDA abs,X (5)
    }

    #[test]
    fn snapshot_restore_replays_a_frame_deterministically() {
        // JMP $8000 keeps the CPU alive without changing machine state beyond
        // timing. Interrupts remain masked by the power-on status flags.
        let bus = Bus::new(test::test_rom(vec![0x4c, 0x00, 0x80]), |_, _, _| {});
        let mut cpu = CPU::new(bus);
        cpu.run_until_frame_ready();
        cpu.mem_write(0x6000, 0x12);
        let snapshot = cpu.snapshot();

        cpu.run_until_frame_ready();
        let expected_frame = cpu.bus.ppu().frame().data.clone();
        let expected_cycles = cpu.bus.cpu_cycles();
        cpu.mem_write(0x6000, 0x34);

        cpu.restore(snapshot);
        assert_eq!(cpu.mem_read(0x6000), 0x12);
        cpu.run_until_frame_ready();
        assert_eq!(cpu.bus.ppu().frame().data, expected_frame);
        assert_eq!(cpu.bus.cpu_cycles(), expected_cycles);
    }

    #[test]
    fn speculative_frames_do_not_reach_the_audio_callback() {
        let delivered = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let callback_delivered = delivered.clone();
        let bus = Bus::new_with_audio(
            test::test_rom(vec![0x4c, 0x00, 0x80]),
            |_, _, _| {},
            256,
            move |samples| callback_delivered.set(callback_delivered.get() + samples.len()),
        );
        let mut cpu = CPU::new(bus);
        cpu.bus.apu.set_sample_rate(crate::audio::SAMPLE_RATE);
        cpu.run_until_frame_ready();
        cpu.bus.apu.drain_samples();
        delivered.set(0);

        let snapshot = cpu.snapshot();
        cpu.bus.set_audio_delivery_enabled(false);
        cpu.run_until_frame_ready();
        cpu.run_until_frame_ready();
        assert_eq!(delivered.get(), 0);

        cpu.restore(snapshot);
        cpu.run_until_frame_ready();
        assert!(delivered.get() >= 512);
    }
}
