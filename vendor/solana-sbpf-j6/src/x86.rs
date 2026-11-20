//! Machinecode emission for x86-64

#![allow(clippy::arithmetic_side_effects)]
use crate::{
    jit::{JitCompiler, OperandSize},
    vm::ContextObject,
};

macro_rules! exclude_operand_sizes {
    ($size:expr, $($to_exclude:path)|+ $(,)?) => {
        debug_assert!(match $size {
            $($to_exclude)|+ => false,
            _ => true,
        });
    }
}

/// Symbolic names for x86-64 registers
#[allow(dead_code, clippy::upper_case_acronyms)]
#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum X86Register {
    /// eax / rax
    RAX = 0,
    /// ecx / rcx
    RCX = 1,
    /// edx / rdx
    RDX = 2,
    /// ebx / rbx
    RBX = 3,
    /// esp / rsp
    RSP = 4,
    /// ebp / rbp
    RBP = 5,
    /// esi / rsi
    RSI = 6,
    /// edi / rdi
    RDI = 7,
    /// r8d / r8
    R8 = 8,
    /// r9d / r9
    R9 = 9,
    /// r10d / r10
    R10 = 10,
    /// r11d / r11
    R11 = 11,
    /// r12d / r12
    R12 = 12,
    /// r13d / r13
    R13 = 13,
    /// r14d / r14
    R14 = 14,
    /// r15d / r15
    R15 = 15,
    /// mm0
    MM0 = 16,
    /// mm1
    MM1 = 17,
    /// mm2
    MM2 = 18,
    /// mm3
    MM3 = 19,
    /// mm4
    MM4 = 20,
    /// mm5
    MM5 = 21,
    /// mm6
    MM6 = 22,
    /// mm7
    MM7 = 23,
}
use X86Register::*;

// System V AMD64 ABI
// Works on: Linux, macOS, BSD and Solaris but not on Windows

/// Registers which can pass arguments at function calls
pub const ARGUMENT_REGISTERS: [X86Register; 6] = [RDI, RSI, RDX, RCX, R8, R9];
/// Registers which have to be saved by the caller
pub const CALLER_SAVED_REGISTERS: [X86Register; 9] = [RAX, RCX, RDX, RSI, RDI, R8, R9, R10, R11];
/// Registers which have to be saved by the callee
pub const CALLEE_SAVED_REGISTERS: [X86Register; 6] = [RBP, RBX, R12, R13, R14, R15];

struct X86Rex {
    w: bool,
    r: bool,
    x: bool,
    b: bool,
}

struct X86ModRm {
    mode: u8,
    r: u8,
    m: u8,
}

struct X86Sib {
    scale: u8,
    index: u8,
    base: u8,
}

/// Scale, index, base (SIB)
#[derive(Copy, Clone)]
pub enum X86IndirectAccess {
    /// [second_operand + offset]
    Offset(i32),
    /// [second_operand + offset + index << shift]
    OffsetIndexShift(i32, X86Register, u8),
}

impl X86IndirectAccess {
    /// Encodes `[base + offset]`, automatically using SIB when `base` is `RSP` or `R12`.
    #[inline(always)]
    pub const fn offset(base: X86Register, offset: i32) -> Self {
        if (base as u8 & 0b111) == 4 {
            Self::OffsetIndexShift(offset, X86Register::RSP, 0)
        } else {
            Self::Offset(offset)
        }
    }
}

#[allow(dead_code)]
#[derive(Copy, Clone)]
pub(crate) enum FenceType {
    /// lfence
    Load = 5,
    /// mfence
    All = 6,
    /// sfence
    Store = 7,
}

/// Generic x86 instruction frame
#[derive(Copy, Clone)]
pub struct X86Instruction {
    size: OperandSize,
    opcode_escape_sequence: u8,
    opcode: u8,
    modrm: bool,
    indirect: Option<X86IndirectAccess>,
    first_operand: u8,
    second_operand: u8,
    immediate_size: OperandSize,
    immediate: i64,
}

impl X86Instruction {
    /// Can be used to fill out unspecified fields in the constructor
    pub const DEFAULT: X86Instruction = X86Instruction {
        size: OperandSize::S0,
        opcode_escape_sequence: 0,
        opcode: 0,
        modrm: true,
        indirect: None,
        first_operand: 0,
        second_operand: 0,
        immediate_size: OperandSize::S0,
        immediate: 0,
    };

    /// Encodes this instruction into a 16-byte buffer and returns `(bytes, len)`
    pub fn encode(&self) -> ([u8; 16], usize) {
        let mut buf = [0u8; 16];
        let len = unsafe { self.encode_into(buf.as_mut_ptr()) };
        (buf, len)
    }

    /// Encodes this instruction at `ptr` and returns its length.
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for writes of 16 bytes. Bytes past the returned length are clobbered.
    #[inline(always)]
    unsafe fn encode_into(&self, ptr: *mut u8) -> usize {
        debug_assert!(!matches!(self.size, OperandSize::S0));
        if self.opcode_escape_sequence == 4 {
            let vex_byte1 = ((!(self.first_operand >> 3) & 1) << 7)
                | (1 << 6)
                | ((!(self.second_operand >> 3) & 1) << 5)
                | 0x02;
            let vex_byte2 =
                ((matches!(self.size, OperandSize::S64) as u8) << 7) | (self.immediate as u8);
            let modrm_byte = 0xc0 | ((self.first_operand & 7) << 3) | (self.second_operand & 7);
            *ptr = 0xc4;
            *ptr.add(1) = vex_byte1;
            *ptr.add(2) = vex_byte2;
            *ptr.add(3) = self.opcode;
            *ptr.add(4) = modrm_byte;
            return 5;
        }
        let mut rex = X86Rex {
            w: matches!(self.size, OperandSize::S64),
            r: self.first_operand & 0b1000 != 0,
            x: false,
            b: self.second_operand & 0b1000 != 0,
        };
        let mut modrm = X86ModRm {
            mode: 0,
            r: self.first_operand & 0b111,
            m: self.second_operand & 0b111,
        };
        let mut sib = X86Sib {
            scale: 0,
            index: 0,
            base: 0,
        };
        let mut has_sib = false;
        let mut displacement_len = 0usize;
        let mut displacement = 0i32;
        if self.modrm {
            match self.indirect {
                Some(X86IndirectAccess::Offset(offset)) => {
                    displacement = offset;
                    debug_assert_ne!(self.second_operand & 0b111, 4); // Reserved for SIB addressing
                    if (-128..=127).contains(&displacement)
                        || (displacement == 0 && self.second_operand & 0b111 == 5)
                    {
                        displacement_len = 1;
                        modrm.mode = 1;
                    } else {
                        displacement_len = 4;
                        modrm.mode = 2;
                    }
                }
                Some(X86IndirectAccess::OffsetIndexShift(offset, index, shift)) => {
                    displacement = offset;
                    if (-128..=127).contains(&displacement) {
                        displacement_len = 1;
                        modrm.mode = 1;
                    } else {
                        displacement_len = 4;
                        modrm.mode = 2;
                    }
                    modrm.m = 4;
                    rex.x = (index as u8) & 0b1000 != 0;
                    sib.scale = shift & 0b11;
                    sib.index = (index as u8) & 0b111;
                    sib.base = self.second_operand & 0b111;
                    has_sib = true;
                }
                None => {
                    modrm.mode = 3;
                }
            }
        }
        let force_rex = matches!(self.size, OperandSize::S8)
            && ((self.opcode_escape_sequence == 0
                && matches!(self.opcode, 0x88 | 0x84 | 0x38)
                && (4..=7).contains(&self.first_operand))
                || (self.modrm
                    && self.indirect.is_none()
                    && (4..=7).contains(&self.second_operand)));
        let mut len = 0usize;
        {
            *ptr = 0x66;
            len += matches!(self.size, OperandSize::S16) as usize;
            let rex =
                ((rex.w as u8) << 3) | ((rex.r as u8) << 2) | ((rex.x as u8) << 1) | (rex.b as u8);
            *ptr.add(len) = 0x40 | rex;
            len += (rex != 0 || force_rex) as usize;
            match self.opcode_escape_sequence {
                1 => {
                    *ptr.add(len) = 0x0f;
                    len += 1;
                }
                2 => {
                    std::ptr::write_unaligned(ptr.add(len).cast::<u16>(), 0x0f38);
                    len += 2;
                }
                3 => {
                    std::ptr::write_unaligned(ptr.add(len).cast::<u16>(), 0x0f3a);
                    len += 2;
                }
                _ => {}
            }
            *ptr.add(len) = self.opcode;
            len += 1;
            if self.modrm {
                *ptr.add(len) = (modrm.mode << 6) | (modrm.r << 3) | modrm.m;
                len += 1;
                if has_sib {
                    *ptr.add(len) = (sib.scale << 6) | (sib.index << 3) | sib.base;
                    len += 1;
                }
                if displacement_len != 0 {
                    std::ptr::write_unaligned(ptr.add(len).cast::<i32>(), displacement);
                    len += displacement_len;
                }
            }
            match self.immediate_size {
                OperandSize::S0 => {}
                OperandSize::S8 => {
                    *ptr.add(len) = self.immediate as u8;
                    len += 1;
                }
                OperandSize::S16 => {
                    std::ptr::write_unaligned(ptr.add(len).cast::<u16>(), self.immediate as u16);
                    len += 2;
                }
                OperandSize::S32 => {
                    std::ptr::write_unaligned(ptr.add(len).cast::<u32>(), self.immediate as u32);
                    len += 4;
                }
                OperandSize::S64 => {
                    debug_assert!(len + 8 <= 16);
                    std::ptr::write_unaligned(ptr.add(len).cast::<u64>(), self.immediate as u64);
                    len += 8;
                }
            }
        }
        debug_assert!(len <= 15);
        len
    }

    /// Emits this instruction into the text section
    #[inline(always)]
    pub fn emit<C: ContextObject>(&self, jit: &mut JitCompiler<C>) {
        if let Some(ptr) = jit.encode_slot() {
            let len = unsafe { self.encode_into(ptr) };
            jit.advance_encoded(len);
        } else {
            let (buf, len) = self.encode();
            jit.emit_encoded_tail(&buf, len);
        }
    }

    /// Arithmetic or logic
    #[inline(always)]
    pub const fn alu_escaped(
        size: OperandSize,
        opcode_escape_sequence: u8,
        opcode: u8,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        Self {
            size,
            opcode_escape_sequence,
            opcode,
            first_operand: source as u8,
            second_operand: destination as u8,
            indirect,
            ..X86Instruction::DEFAULT
        }
    }

    /// Arithmetic or logic
    #[inline(always)]
    pub const fn alu_immediate_escaped(
        size: OperandSize,
        opcode_escape_sequence: u8,
        opcode: u8,
        opcode_extension: u8,
        destination: X86Register,
        immediate: i64,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        let opcode = if opcode_escape_sequence == 0
            && opcode == 0x81
            && matches!(size, OperandSize::S32 | OperandSize::S64)
            && immediate >= i8::MIN as i64
            && immediate <= i8::MAX as i64
        {
            0x83
        } else {
            opcode
        };
        Self {
            size,
            opcode_escape_sequence,
            opcode,
            first_operand: opcode_extension,
            second_operand: destination as u8,
            immediate_size: match opcode {
                0x83 | 0xc1 => OperandSize::S8,
                0x81 => OperandSize::S32,
                0xf7 if opcode_extension == 0 => OperandSize::S32,
                _ => OperandSize::S0,
            },
            immediate,
            indirect,
            ..X86Instruction::DEFAULT
        }
    }

    /// Signed multiply source by immediate into destination (3-operand IMUL)
    #[inline(always)]
    pub const fn imul_immediate(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        immediate: i64,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        let compact = immediate >= i8::MIN as i64 && immediate <= i8::MAX as i64;
        Self {
            size,
            opcode: if compact { 0x6b } else { 0x69 },
            first_operand: destination as u8,
            second_operand: source as u8,
            immediate_size: if compact {
                OperandSize::S8
            } else {
                OperandSize::S32
            },
            immediate,
            ..Self::DEFAULT
        }
    }

    /// BMI2 flagless variable bit shift (`SHLX`, `SHRX`, `SARX`)
    #[inline(always)]
    pub const fn bmi2_shift(
        size: OperandSize,
        opcode_extension: u8,
        destination: X86Register,
        source_value: X86Register,
        shift_count: X86Register,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        let pp = match opcode_extension {
            4 => 0x01, // 66: SHLX
            5 => 0x03, // F2: SHRX
            7 => 0x02, // F3: SARX
            _ => unreachable!(),
        };
        Self {
            size,
            opcode_escape_sequence: 4,
            opcode: 0xf7,
            first_operand: destination as u8,
            second_operand: source_value as u8,
            immediate: (((!(shift_count as u8) & 0x0f) << 3) | pp) as i64,
            ..Self::DEFAULT
        }
    }

    /// Arithmetic or logic
    #[inline(always)]
    pub const fn alu(
        size: OperandSize,
        opcode: u8,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        Self::alu_escaped(size, 0, opcode, source, destination, indirect)
    }

    /// Arithmetic or logic
    #[inline(always)]
    pub const fn alu_immediate(
        size: OperandSize,
        opcode: u8,
        opcode_extension: u8,
        destination: X86Register,
        immediate: i64,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        Self::alu_immediate_escaped(
            size,
            0,
            opcode,
            opcode_extension,
            destination,
            immediate,
            indirect,
        )
    }

    /// Move source to destination
    #[inline(always)]
    pub const fn mov(size: OperandSize, source: X86Register, destination: X86Register) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        Self {
            size,
            opcode: 0x89,
            first_operand: source as u8,
            second_operand: destination as u8,
            ..Self::DEFAULT
        }
    }

    /// Move source to destination
    #[inline(always)]
    pub const fn mov_with_sign_extension(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        Self {
            size,
            opcode: 0x63,
            first_operand: destination as u8,
            second_operand: source as u8,
            ..Self::DEFAULT
        }
    }

    /// Move to / from / between MMX (float mantissa)
    #[allow(dead_code)]
    #[inline(always)]
    pub const fn mov_mmx(size: OperandSize, source: X86Register, destination: X86Register) -> Self {
        exclude_operand_sizes!(
            size,
            OperandSize::S0 | OperandSize::S8 | OperandSize::S16 | OperandSize::S32
        );
        if (destination as u8) & 16 != 0 {
            // If the destination is a MMX register
            Self {
                size,
                opcode_escape_sequence: 1,
                opcode: if (source as u8) & 16 != 0 { 0x6F } else { 0x6E },
                first_operand: (destination as u8) & 0xF,
                second_operand: (source as u8) & 0xF,
                ..Self::DEFAULT
            }
        } else {
            Self {
                size,
                opcode_escape_sequence: 1,
                opcode: 0x7E,
                first_operand: (source as u8) & 0xF,
                second_operand: (destination as u8) & 0xF,
                ..Self::DEFAULT
            }
        }
    }

    /// Conditionally move source to destination
    #[inline(always)]
    pub const fn cmov(
        size: OperandSize,
        condition: u8,
        source: X86Register,
        destination: X86Register,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        Self {
            size,
            opcode_escape_sequence: 1,
            opcode: condition,
            first_operand: destination as u8,
            second_operand: source as u8,
            ..Self::DEFAULT
        }
    }

    /// Swap source and destination
    #[inline(always)]
    pub const fn xchg(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(
            size,
            OperandSize::S0 | OperandSize::S8 | OperandSize::S16 | OperandSize::S32,
        );
        Self {
            size,
            opcode: 0x87,
            first_operand: source as u8,
            second_operand: destination as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Swap byte order of destination
    #[inline(always)]
    pub const fn bswap(size: OperandSize, destination: X86Register) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8);
        match size {
            OperandSize::S16 => Self {
                size,
                opcode: 0xc1,
                second_operand: destination as u8,
                immediate_size: OperandSize::S8,
                immediate: 8,
                ..Self::DEFAULT
            },
            OperandSize::S32 | OperandSize::S64 => Self {
                size,
                opcode_escape_sequence: 1,
                opcode: 0xc8 | ((destination as u8) & 0b111),
                modrm: false,
                second_operand: destination as u8,
                ..Self::DEFAULT
            },
            _ => unimplemented!(),
        }
    }

    /// Test source and destination
    #[inline(always)]
    pub const fn test(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size,
            opcode: if let OperandSize::S8 = size {
                0x84
            } else {
                0x85
            },
            first_operand: source as u8,
            second_operand: destination as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Test immediate and destination
    #[inline(always)]
    pub const fn test_immediate(
        size: OperandSize,
        destination: X86Register,
        immediate: i64,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size,
            opcode: if let OperandSize::S8 = size {
                0xf6
            } else {
                0xf7
            },
            first_operand: 0,
            second_operand: destination as u8,
            immediate_size: if let OperandSize::S64 = size {
                OperandSize::S32
            } else {
                size
            },
            immediate,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Compare source and destination
    #[inline(always)]
    pub const fn cmp(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size,
            opcode: if let OperandSize::S8 = size {
                0x38
            } else {
                0x39
            },
            first_operand: source as u8,
            second_operand: destination as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Compare immediate and destination
    #[inline(always)]
    pub const fn cmp_immediate(
        size: OperandSize,
        destination: X86Register,
        immediate: i64,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        let compact = matches!(size, OperandSize::S16 | OperandSize::S32 | OperandSize::S64)
            && immediate >= i8::MIN as i64
            && immediate <= i8::MAX as i64;
        Self {
            size,
            opcode: if let OperandSize::S8 = size {
                0x80
            } else if compact {
                0x83
            } else {
                0x81
            },
            first_operand: 7,
            second_operand: destination as u8,
            immediate_size: if compact {
                OperandSize::S8
            } else if let OperandSize::S64 = size {
                OperandSize::S32
            } else {
                size
            },
            immediate,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Load effective address of source into destination
    #[inline(always)]
    pub const fn lea(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: Option<X86IndirectAccess>,
    ) -> Self {
        exclude_operand_sizes!(
            size,
            OperandSize::S0 | OperandSize::S8 | OperandSize::S16 | OperandSize::S32,
        );
        Self {
            size,
            opcode: 0x8d,
            first_operand: destination as u8,
            second_operand: source as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Convert word to doubleword or doubleword to quadword
    #[inline(always)]
    pub const fn sign_extend_rax_rdx(size: OperandSize) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S8 | OperandSize::S16);
        Self {
            size,
            opcode: 0x99,
            modrm: false,
            ..X86Instruction::DEFAULT
        }
    }

    /// Load destination from [source + offset]
    #[inline(always)]
    pub const fn load(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: X86IndirectAccess,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size: if let OperandSize::S64 = size {
                OperandSize::S64
            } else {
                OperandSize::S32
            },
            opcode_escape_sequence: match size {
                OperandSize::S8 | OperandSize::S16 => 1,
                _ => 0,
            },
            opcode: match size {
                OperandSize::S8 => 0xb6,
                OperandSize::S16 => 0xb7,
                _ => 0x8b,
            },
            first_operand: destination as u8,
            second_operand: source as u8,
            indirect: Some(indirect),
            ..Self::DEFAULT
        }
    }

    /// Store source in [destination + offset]
    #[inline(always)]
    pub const fn store(
        size: OperandSize,
        source: X86Register,
        destination: X86Register,
        indirect: X86IndirectAccess,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size,
            opcode: match size {
                OperandSize::S8 => 0x88,
                _ => 0x89,
            },
            first_operand: source as u8,
            second_operand: destination as u8,
            indirect: Some(indirect),
            ..Self::DEFAULT
        }
    }

    /// Load destination from immediate
    #[inline(always)]
    pub const fn load_immediate(destination: X86Register, immediate: i64) -> Self {
        if immediate == 0 {
            Self {
                size: OperandSize::S32,
                opcode: 0x31,
                first_operand: destination as u8,
                second_operand: destination as u8,
                ..Self::DEFAULT
            }
        } else {
            Self::load_immediate_preserve_flags(destination, immediate)
        }
    }

    /// Load destination from immediate without modifying status flags
    #[inline(always)]
    pub const fn load_immediate_preserve_flags(destination: X86Register, immediate: i64) -> Self {
        let mut size = OperandSize::S64;
        if immediate >= 0 {
            if immediate <= u32::MAX as i64 {
                // Zero extend u32 imm to u64 reg
                size = OperandSize::S32;
            }
        } else if immediate >= i32::MIN as i64 {
            // Sign extend i32 imm to i64 reg
            return Self {
                size: OperandSize::S64,
                opcode: 0xc7,
                second_operand: destination as u8,
                immediate_size: OperandSize::S32,
                immediate,
                ..Self::DEFAULT
            };
        }
        // Load full u64 imm into u64 reg
        Self {
            size,
            opcode: (0xb8 | ((destination as u8) & 0b111)),
            modrm: false,
            second_operand: destination as u8,
            immediate_size: size,
            immediate,
            ..Self::DEFAULT
        }
    }

    /// Store sign-extended immediate in destination
    #[inline(always)]
    pub const fn store_immediate(
        size: OperandSize,
        destination: X86Register,
        indirect: X86IndirectAccess,
        immediate: i64,
    ) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        Self {
            size,
            opcode: match size {
                OperandSize::S8 => 0xc6,
                _ => 0xc7,
            },
            second_operand: destination as u8,
            indirect: Some(indirect),
            immediate_size: if let OperandSize::S64 = size {
                OperandSize::S32
            } else {
                size
            },
            immediate,
            ..Self::DEFAULT
        }
    }

    /// Push source onto the stack
    #[allow(dead_code)]
    #[inline(always)]
    pub const fn push_immediate(size: OperandSize, immediate: i32) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0 | OperandSize::S16);
        Self {
            size,
            opcode: match size {
                OperandSize::S8 => 0x6A,
                _ => 0x68,
            },
            modrm: false,
            immediate_size: if let OperandSize::S64 = size {
                OperandSize::S32
            } else {
                size
            },
            immediate: immediate as i64,
            ..Self::DEFAULT
        }
    }

    /// Push source onto the stack
    #[inline(always)]
    pub const fn push(source: X86Register, indirect: Option<X86IndirectAccess>) -> Self {
        if indirect.is_none() {
            Self {
                size: OperandSize::S32,
                opcode: 0x50 | ((source as u8) & 0b111),
                modrm: false,
                second_operand: source as u8,
                ..Self::DEFAULT
            }
        } else {
            Self {
                size: OperandSize::S64,
                opcode: 0xFF,
                modrm: true,
                first_operand: 6,
                second_operand: source as u8,
                indirect,
                ..Self::DEFAULT
            }
        }
    }

    /// Pop from the stack into destination
    #[inline(always)]
    pub const fn pop(destination: X86Register) -> Self {
        Self {
            size: OperandSize::S32,
            opcode: 0x58 | ((destination as u8) & 0b111),
            modrm: false,
            second_operand: destination as u8,
            ..Self::DEFAULT
        }
    }

    /// Jump to short relative destination on condition
    #[inline(always)]
    pub const fn conditional_jump_short(opcode_7x: u8, relative_destination: i8) -> Self {
        debug_assert!(opcode_7x >= 0x70 && opcode_7x <= 0x7f);
        Self {
            size: OperandSize::S32,
            opcode: opcode_7x,
            modrm: false,
            immediate_size: OperandSize::S8,
            immediate: relative_destination as i64,
            ..Self::DEFAULT
        }
    }

    /// Jump to relative destination on condition
    #[inline(always)]
    pub const fn conditional_jump_immediate(opcode: u8, relative_destination: i32) -> Self {
        Self {
            size: OperandSize::S32,
            opcode_escape_sequence: 1,
            opcode,
            modrm: false,
            immediate_size: OperandSize::S32,
            immediate: relative_destination as i64,
            ..Self::DEFAULT
        }
    }

    /// Jump to relative destination
    #[inline(always)]
    pub const fn jump_immediate(relative_destination: i32) -> Self {
        Self {
            size: OperandSize::S32,
            opcode: 0xe9,
            modrm: false,
            immediate_size: OperandSize::S32,
            immediate: relative_destination as i64,
            ..Self::DEFAULT
        }
    }

    /// Jump to absolute destination
    #[allow(dead_code)]
    #[inline(always)]
    pub const fn jump_reg(destination: X86Register, indirect: Option<X86IndirectAccess>) -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0xff,
            first_operand: 4,
            second_operand: destination as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Push RIP and jump to relative destination
    #[inline(always)]
    pub const fn call_immediate(relative_destination: i32) -> Self {
        Self {
            size: OperandSize::S32,
            opcode: 0xe8,
            modrm: false,
            immediate_size: OperandSize::S32,
            immediate: relative_destination as i64,
            ..Self::DEFAULT
        }
    }

    /// Push RIP and jump to absolute destination
    #[inline(always)]
    pub const fn call_reg(destination: X86Register, indirect: Option<X86IndirectAccess>) -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0xff,
            first_operand: 2,
            second_operand: destination as u8,
            indirect,
            ..Self::DEFAULT
        }
    }

    /// Pop RIP
    #[inline(always)]
    pub const fn return_near() -> Self {
        Self {
            size: OperandSize::S32,
            opcode: 0xc3,
            modrm: false,
            ..Self::DEFAULT
        }
    }

    /// No operation
    #[allow(dead_code)]
    #[inline(always)]
    pub const fn noop() -> Self {
        Self {
            size: OperandSize::S32,
            opcode: 0x90,
            modrm: false,
            ..Self::DEFAULT
        }
    }

    /// Trap / software interrupt
    #[allow(dead_code)]
    #[inline(always)]
    pub const fn interrupt(immediate: u8) -> Self {
        if immediate == 3 {
            Self {
                size: OperandSize::S32,
                opcode: 0xcc,
                modrm: false,
                ..Self::DEFAULT
            }
        } else {
            Self {
                size: OperandSize::S32,
                opcode: 0xcd,
                modrm: false,
                immediate_size: OperandSize::S8,
                immediate: immediate as i64,
                ..Self::DEFAULT
            }
        }
    }

    /// rdtsc
    #[inline(always)]
    pub const fn cycle_count() -> Self {
        Self {
            size: OperandSize::S32,
            opcode_escape_sequence: 1,
            opcode: 0x31,
            modrm: false,
            ..Self::DEFAULT
        }
    }

    /// lfence / sfence / mfence
    #[allow(dead_code)]
    pub(crate) const fn fence(fence_type: FenceType) -> Self {
        Self {
            size: OperandSize::S32,
            opcode_escape_sequence: 1,
            opcode: 0xae,
            first_operand: fence_type as u8,
            ..Self::DEFAULT
        }
    }
}
