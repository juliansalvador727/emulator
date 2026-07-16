#!/usr/bin/env python3
"""Build the repository's source-available MMC1 validation ROMs.

The tiny assembler below deliberately supports only the instructions used by
the fixtures.  This keeps the mapper suite hermetic: no cc65 installation and
no redistributable third-party ROM are required.
"""

import hashlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "test-roms" / "generated"


class Asm:
    def __init__(self, origin=0xC000):
        self.origin = origin
        self.code = bytearray()
        self.labels = {}
        self.fixups = []

    @property
    def pc(self):
        return self.origin + len(self.code)

    def emit(self, *values):
        self.code.extend(values)

    def label(self, name):
        self.labels[name] = self.pc

    def imm(self, opcode, value):
        self.emit(opcode, value)

    def absolute(self, opcode, address):
        self.emit(opcode, address & 0xFF, address >> 8)

    def branch(self, opcode, label):
        self.emit(opcode, 0)
        self.fixups.append((len(self.code) - 1, label, "rel"))

    def jump(self, label):
        self.emit(0x4C, 0, 0)
        self.fixups.append((len(self.code) - 2, label, "abs"))

    def finish(self):
        for offset, label, kind in self.fixups:
            target = self.labels[label]
            if kind == "abs":
                self.code[offset:offset + 2] = bytes((target & 0xFF, target >> 8))
            else:
                delta = target - (self.origin + offset + 1)
                if not -128 <= delta <= 127:
                    raise ValueError(f"branch to {label} is out of range")
                self.code[offset] = delta & 0xFF
        return bytes(self.code)


def lda_i(a, value):
    a.imm(0xA9, value)


def lda(a, address):
    a.absolute(0xAD, address)


def sta(a, address):
    a.absolute(0x8D, address)


def cmp_i(a, value):
    a.imm(0xC9, value)


def mmc1_load(a, address, value):
    """Serially load a five-bit MMC1 register, least-significant bit first."""
    for bit in range(5):
        lda_i(a, (value >> bit) & 1)
        sta(a, address)


def expect(a, address, value, failure):
    lda(a, address)
    cmp_i(a, value)
    success = f"expect_ok_{len(a.fixups)}"
    a.branch(0xF0, success)  # BEQ over the long failure jump
    a.jump(failure)
    a.label(success)


def make_program():
    a = Asm()
    a.label("reset")
    a.emit(0x78, 0xD8)             # SEI; CLD
    a.imm(0xA2, 0xFF)              # LDX #$FF
    a.emit(0x9A)                    # TXS
    lda(a, 0x6101)
    cmp_i(a, 1)
    a.branch(0xD0, "first_boot")  # BNE over the long reset-path jump
    a.jump("after_reset")
    a.label("first_boot")

    # Start the blargg protocol in PRG-RAM bank zero.
    lda_i(a, 0x80)
    sta(a, 0x6000)
    for address, value in ((0x6001, 0xDE), (0x6002, 0xB0), (0x6003, 0x61)):
        lda_i(a, value)
        sta(a, address)

    # SUROM: CHR bank bit 4 selects the upper 256 KiB PRG region.  Code is
    # mirrored in fixed banks 15 and 31 so it remains executable at the switch.
    mmc1_load(a, 0xE000, 2)
    expect(a, 0x8000, 2, "fail_prg_low")
    mmc1_load(a, 0xA000, 0x10)
    expect(a, 0x8000, 18, "fail_prg_high")
    mmc1_load(a, 0xA000, 0)

    # SXROM: CHR bank bits 2-3 select four independent 8 KiB RAM banks.
    for bank, value in enumerate((0x11, 0x22, 0x33, 0x44)):
        mmc1_load(a, 0xA000, bank << 2)
        lda_i(a, value)
        sta(a, 0x6100)
    for bank, value in enumerate((0x11, 0x22, 0x33, 0x44)):
        mmc1_load(a, 0xA000, bank << 2)
        expect(a, 0x6100, value, f"fail_ram_{bank}")
    mmc1_load(a, 0xA000, 0)

    # PRG register bit 4 disables RAM. Reads become zero and writes are ignored.
    mmc1_load(a, 0xE000, 0x12)
    expect(a, 0x6100, 0, "fail_ram_disable")
    lda_i(a, 0x99)
    sta(a, 0x6100)
    mmc1_load(a, 0xE000, 2)
    expect(a, 0x6100, 0x11, "fail_ram_disable_write")

    # Ask the headless harness for a front-panel reset while MMC1 is in 32 KiB
    # PRG mode. The reset must restore fixed-last-bank mode without clearing RAM
    # or the completed PRG register.
    lda_i(a, 1)
    sta(a, 0x6101)
    mmc1_load(a, 0x8000, 0)
    lda_i(a, 0x81)
    sta(a, 0x6000)
    a.label("wait_reset")
    a.jump("wait_reset")

    a.label("after_reset")
    expect(a, 0x8000, 2, "fail_reset_mode")
    expect(a, 0x6100, 0x11, "fail_reset_ram")
    lda_i(a, 0)
    sta(a, 0x6000)
    a.label("passed")
    a.jump("passed")

    failures = ["prg_low", "prg_high"] + [f"ram_{i}" for i in range(4)] + [
        "ram_disable", "ram_disable_write", "reset_mode", "reset_ram"
    ]
    for status, name in enumerate(failures, 1):
        a.label(f"fail_{name}")
        # Restore RAM bank zero before publishing the result, except when RAM is
        # disabled; restoring the PRG register handles that case too.
        mmc1_load(a, 0xE000, 2)
        mmc1_load(a, 0xA000, 0)
        lda_i(a, status)
        sta(a, 0x6000)
        a.label(f"halt_{name}")
        a.jump(f"halt_{name}")
    return a.finish(), a.labels["reset"]


def build():
    code, reset = make_program()
    if len(code) > 0x3FFA:
        raise ValueError("fixture code overlaps vectors")

    banks = []
    for number in range(32):
        bank = bytearray([number] * 0x4000)
        # Bank 3 executes while the test deliberately selects 32 KiB mode;
        # banks 15/31 are the fixed banks in the two SUROM outer regions.
        if number in (3, 15, 31):
            bank[:len(code)] = code
            bank[0x3FFA:0x4000] = bytes((reset & 0xFF, reset >> 8)) * 3
        banks.append(bank)

    header = bytearray(b"NES\x1a")
    header.extend((32, 0, 0x10, 0, 4, 0, 0, 0, 0, 0, 0, 0))
    image = bytes(header) + b"".join(banks)
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / "mmc1_sxrom.nes"
    path.write_bytes(image)
    digest = hashlib.sha256(image).hexdigest()
    print(f"{path.relative_to(ROOT)} {digest}")


if __name__ == "__main__":
    build()
