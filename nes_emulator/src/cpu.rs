/*
note: NES CPU uses little endian addressing
Real Address	                0x8000
Address packed in big-endian	80 00
Address packed in little-endian	00 80
e.g. read data from 0x8000 into A reg:
LDA $8000      <=>    ad 00 80
*/

use crate::bus::{Bus, BusSnapshot};
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
}

pub struct CpuSnapshot {
    register_a: u8,
    register_x: u8,
    register_y: u8,
    status: CpuFlags,
    program_counter: u16,
    stack_pointer: u8,
    additional_cycles: u8,
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
        self.bus.mem_read(addr)
    }

    fn mem_write(&mut self, addr: u16, data: u8) {
        self.bus.mem_write(addr, data)
    }
    fn mem_read_u16(&mut self, pos: u16) -> u16 {
        self.bus.mem_read_u16(pos)
    }

    fn mem_write_u16(&mut self, pos: u16, data: u16) {
        self.bus.mem_write_u16(pos, data)
    }
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
        }
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

    // Indexed reads take one additional cycle when their effective address
    // crosses a page. Indexed stores and read-modify-write instructions have
    // fixed timings, so only value-reading operations use this helper.
    fn get_read_operand_addressing(&mut self, mode: &AddressingMode) -> u16 {
        match mode {
            AddressingMode::Absolute_X => {
                let base = self.mem_read_u16(self.program_counter);
                let addr = base.wrapping_add(self.register_x as u16);
                self.additional_cycles += u8::from((base & 0xff00) != (addr & 0xff00));
                addr
            }
            AddressingMode::Absolute_Y => {
                let base = self.mem_read_u16(self.program_counter);
                let addr = base.wrapping_add(self.register_y as u16);
                self.additional_cycles += u8::from((base & 0xff00) != (addr & 0xff00));
                addr
            }
            AddressingMode::Indirect_Y => {
                let base = self.mem_read(self.program_counter);
                let lo = self.mem_read(base as u16);
                let hi = self.mem_read(base.wrapping_add(1) as u16);
                let pointer = (hi as u16) << 8 | lo as u16;
                let addr = pointer.wrapping_add(self.register_y as u16);
                self.additional_cycles += u8::from((pointer & 0xff00) != (addr & 0xff00));
                addr
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
        let addr = self.get_operand_addressing(mode);
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
        let addr = self.get_operand_addressing(mode);
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
        let addr = self.get_operand_addressing(mode);
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
        let addr = self.get_operand_addressing(mode);
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
        self.update_negative_flags(data);
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
        let addr = self.get_operand_addressing(mode);
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
        self.update_negative_flags(data);
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
        let addr = self.get_operand_addressing(mode);
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
        let addr = self.get_operand_addressing(mode);
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
        if condition {
            let old_pc = self.program_counter.wrapping_add(1);
            let jump: i8 = self.mem_read(self.program_counter) as i8;
            let jump_addr = old_pc.wrapping_add(jump as u16);

            self.program_counter = jump_addr;
            self.additional_cycles += 1;
            if (old_pc & 0xff00) != (jump_addr & 0xff00) {
                self.additional_cycles += 1;
            }
        }
    }

    fn interrupt_nmi(&mut self) {
        self.stack_push_u16(self.program_counter);
        let mut flag = self.status.clone();
        flag.set(CpuFlags::BREAK, false);
        flag.set(CpuFlags::BREAK2, true);

        self.stack_push(flag.bits);
        self.status.insert(CpuFlags::INTERRUPT_DISABLE);

        self.bus.tick(2);
        self.program_counter = self.mem_read_u16(0xfffA);
    }

    // Maskable interrupt (the APU frame counter and DMC assert it); same as
    // NMI but through the $FFFE vector. The line is level-triggered, so the
    // handler must acknowledge the source (e.g. read $4015) or it will fire
    // again after RTI.
    fn interrupt_irq(&mut self) {
        self.stack_push_u16(self.program_counter);
        let mut flag = self.status.clone();
        flag.set(CpuFlags::BREAK, false);
        flag.set(CpuFlags::BREAK2, true);

        self.stack_push(flag.bits);
        self.status.insert(CpuFlags::INTERRUPT_DISABLE);

        self.bus.tick(2);
        self.program_counter = self.mem_read_u16(0xfffE);
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
            if let Some(_nmi) = self.bus.poll_nmi_status() {
                self.interrupt_nmi();
            }

            if self.bus.poll_irq_status() && !self.status.contains(CpuFlags::INTERRUPT_DISABLE) {
                self.interrupt_irq();
            }

            if callback(self) {
                return;
            }

            let code = self.mem_read(self.program_counter);
            self.program_counter += 1;
            let program_counter_state = self.program_counter;
            let opcode = opcodes.get(&code).unwrap();
            self.additional_cycles = 0;

            // The CPU core executes an instruction atomically, but PPU/APU
            // register accesses occur on its final bus cycle. Advance the
            // fixed portion first so those externally visible accesses are
            // placed at the end of the instruction rather than its start.
            // BRK remains the test/program halt sentinel used by this core.
            if code == 0x00 {
                return;
            }
            self.bus.tick(opcode.cycles);

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

                0x00 => unreachable!(),
                _ => todo!(),
            }
            // Taken-branch and indexed-read penalties are discovered while
            // executing the instruction.
            self.bus.tick(self.additional_cycles);

            if program_counter_state == self.program_counter {
                self.program_counter += (opcode.len - 1) as u16;
            }
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

    fn update_negative_flags(&mut self, result: u8) {
        if result >> 7 == 1 {
            self.status.insert(CpuFlags::NEGATIV)
        } else {
            self.status.remove(CpuFlags::NEGATIV)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cartridge::test;

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
