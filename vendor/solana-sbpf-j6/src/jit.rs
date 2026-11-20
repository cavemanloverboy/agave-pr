//! Just-in-time compiler (Linux x86, macOS x86)

// Derived from uBPF <https://github.com/iovisor/ubpf>
// Copyright 2015 Big Switch Networks, Inc
//      (uBPF: JIT algorithm, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, MetaBuff addition)
// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![allow(clippy::arithmetic_side_effects)]

#[cfg(not(feature = "shuttle-test"))]
use rand::{thread_rng, Rng};
#[cfg(feature = "shuttle-test")]
use shuttle::rand::{thread_rng, Rng};

use rand::{
    distributions::{Distribution, Uniform},
    rngs::SmallRng,
    SeedableRng,
};
use std::{convert::TryFrom, fmt::Debug, mem, ptr};

#[cfg(not(feature = "shuttle-test"))]
thread_local! {
    static JIT_DIVERSIFICATION_RNG: std::cell::RefCell<SmallRng> =
        std::cell::RefCell::new(SmallRng::from_rng(thread_rng()).expect("rng"));
}

use crate::{
    ebpf::{self, FIRST_SCRATCH_REG, FRAME_PTR_REG, INSN_SIZE, SCRATCH_REGS},
    elf::Executable,
    error::{EbpfError, ProgramResult},
    memory_region::{MappingCacheEntry, MemoryMapping, MemoryRegion},
    program::{BuiltinFunction, JitProgram, MemoryPcTable, SBPFVersion, UnsealedJitProgram},
    vm::{get_runtime_environment_key, Config, ContextObject, EbpfVm, RuntimeEnvironmentSlot},
    x86::{
        FenceType, X86IndirectAccess, X86Instruction,
        X86Register::{self, *},
        ARGUMENT_REGISTERS, CALLEE_SAVED_REGISTERS, CALLER_SAVED_REGISTERS,
    },
};

/// The maximum machine code length in bytes of a program with no guest instructions
pub const MAX_EMPTY_PROGRAM_MACHINE_CODE_LENGTH: usize = 4096;
/// The maximum machine code length in bytes of a single guest instruction
#[cfg(not(feature = "tracer"))]
pub const MAX_MACHINE_CODE_LENGTH_PER_INSTRUCTION: usize = 110;
/// The maximum machine code length in bytes of a single guest instruction
#[cfg(feature = "tracer")]
pub const MAX_MACHINE_CODE_LENGTH_PER_INSTRUCTION: usize = 128;
/// The maximum machine code length in bytes of an instruction meter checkpoint
pub const MACHINE_CODE_PER_INSTRUCTION_METER_CHECKPOINT: usize = 24;
/// The maximum machine code length of the randomized padding
pub const MAX_START_PADDING_LENGTH: usize = 256;

impl<C: ContextObject> Executable<C> {
    /// JIT compile the executable
    ///
    /// This function does not ensure fully sequentially consistent execution ordering between calls
    /// to it and related calls such as [`Self::get_compiled_program`] or
    /// [`Self::take_compiled_program`].
    ///
    /// This means that there can be some non-trivial interactions in ordering between calls to this
    /// function and a `get_compiled_program`: concurrent calls to `get_compiled_program` will
    /// return the previous compiled program or `None` for the duration of the compilation process
    /// and is only guaranteed to start returning the newly compiled `JitProgram` after this
    /// function returns.
    pub fn jit_compile(&self) -> Result<(), crate::error::EbpfError> {
        let jit = JitCompiler::<C>::new(self)?;
        let compiled = std::sync::Arc::new(jit.compile()?);
        let mut guard = self
            .compiled_program
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(compiled);
        Ok(())
    }
}

impl JitProgram {
    pub(crate) fn try_invoke<C: ContextObject>(
        &self,
        config: &Config,
        sbpf_version: SBPFVersion,
        vm: &mut EbpfVm<C>,
    ) -> bool {
        if self.inline_fast_mem() && !vm.memory().matches_jit_fast_path(config, sbpf_version) {
            return false;
        }
        let stack_host_delta = if self.inline_v3_stack() {
            if vm.call_depth != 0
                || vm.registers[FRAME_PTR_REG]
                    != ebpf::MM_STACK_START.saturating_add(config.stack_frame_size as u64)
            {
                return false;
            }
            let Some(stack_size) = config.stack_frame_size.checked_mul(config.max_call_depth)
            else {
                return false;
            };
            let Some(stack_host_addr) = vm.memory().fast_stack_host_addr(stack_size as u64) else {
                return false;
            };
            stack_host_addr.wrapping_sub(ebpf::MM_STACK_START)
        } else {
            0
        };
        unsafe {
            let entry_pc = vm.registers[11];
            let instruction_meter =
                (vm.previous_instruction_meter as i64).wrapping_add(entry_pc as i64);
            let entrypoint = &self.text_section()
                [self.pc_section()[entry_pc as usize] as usize & (i32::MAX as u32 as usize)]
                as *const u8;
            let host_stack_pointer = &raw mut vm.host_stack_pointer;
            let registers_ptr = &raw const vm.registers;
            let vm = vm.encrypted_host_address();
            macro_rules! stmt_expr_attribute_asm {
                ($($prologue:literal,)+ cfg(not(feature = $feature:literal)), $guarded:tt, $($epilogue:tt)+) => {
                    #[cfg(feature = $feature)]
                    std::arch::asm!($($prologue,)+ $($epilogue)+);
                    #[cfg(not(feature = $feature))]
                    std::arch::asm!($($prologue,)+ $guarded, $($epilogue)+);
                }
            }
            stmt_expr_attribute_asm!(
                // RBP and RBX must be saved and restored manually in the current version of rustc and llvm.
                "push rbx",
                "push rbp",
                "mov [{host_stack_pointer}], rsp",
                "add QWORD PTR [{host_stack_pointer}], -8",
                // RBP holds stack_host_delta when inline_v3_stack is active, or 0 otherwise so caller RBP is never exposed.
                cfg(not(feature = "jit-enable-host-stack-frames")),
                "mov rbp, rsi",
                "mov [rsp-8], rax",
                "mov rax, [r11 + 0x00]",
                "mov rsi, [r11 + 0x08]",
                "mov rdx, [r11 + 0x10]",
                "mov rcx, [r11 + 0x18]",
                "mov r8,  [r11 + 0x20]",
                "mov r9,  [r11 + 0x28]",
                "mov rbx, [r11 + 0x30]",
                "mov r12, [r11 + 0x38]",
                "mov r13, [r11 + 0x40]",
                "mov r14, [r11 + 0x48]",
                "mov r15, [r11 + 0x50]",
                "mov r11, [r11 + 0x58]",
                "call [rsp-8]",
                "pop rbp",
                "pop rbx",
                host_stack_pointer = in(reg) host_stack_pointer,
                inlateout("rdi") vm.0 => _,
                inlateout("rsi") stack_host_delta => _,
                inlateout("r10") instruction_meter => _,
                inlateout("rax") entrypoint => _,
                inlateout("r11") registers_ptr => _,
                lateout("rdx") _, lateout("rcx") _, lateout("r8") _,
                lateout("r9") _, lateout("r12") _, lateout("r13") _, lateout("r14") _, lateout("r15") _,
                // lateout("rbp") _, lateout("rbx") _,
            );
        }
        true
    }
}

#[cold]
#[inline(never)]
extern "C" fn lookup_memory_pc(table: *const MemoryPcTable, return_rip: usize) -> u64 {
    let table = unsafe { &*table };
    let offset = return_rip.wrapping_sub(table.text_section_base) as u32;
    let idx = table
        .entries
        .binary_search_by_key(&offset, |&(ret_off, _)| ret_off)
        .expect("valid JIT memory callsite return RIP");
    table.entries[idx].1 as u64
}

#[inline]
fn insn_writes_dst(opc: u8, sbpf_version: SBPFVersion) -> bool {
    if (opc & 0x07) == ebpf::BPF_JMP64 {
        return false;
    }
    if sbpf_version.enable_jmp32()
        && matches!(
            opc,
            ebpf::JEQ32_IMM
                | ebpf::JEQ32_REG
                | ebpf::JGT32_IMM
                | ebpf::JGT32_REG
                | ebpf::JGE32_IMM
                | ebpf::JGE32_REG
                | ebpf::JLT32_IMM
                | ebpf::JLT32_REG
                | ebpf::JLE32_IMM
                | ebpf::JLE32_REG
                | ebpf::JSET32_IMM
                | ebpf::JSET32_REG
                | ebpf::JNE32_IMM
                | ebpf::JNE32_REG
                | ebpf::JSGT32_IMM
                | ebpf::JSGT32_REG
                | ebpf::JSGE32_IMM
                | ebpf::JSGE32_REG
                | ebpf::JSLT32_IMM
                | ebpf::JSLT32_REG
                | ebpf::JSLE32_IMM
                | ebpf::JSLE32_REG
        )
    {
        return false;
    }
    if sbpf_version.move_memory_instruction_classes() {
        !matches!(
            opc,
            ebpf::ST_1B_IMM
                | ebpf::ST_2B_IMM
                | ebpf::ST_4B_IMM
                | ebpf::ST_8B_IMM
                | ebpf::ST_1B_REG
                | ebpf::ST_2B_REG
                | ebpf::ST_4B_REG
                | ebpf::ST_8B_REG
        )
    } else {
        !matches!(
            opc,
            ebpf::ST_B_IMM
                | ebpf::ST_H_IMM
                | ebpf::ST_W_IMM
                | ebpf::ST_DW_IMM
                | ebpf::ST_B_REG
                | ebpf::ST_H_REG
                | ebpf::ST_W_REG
                | ebpf::ST_DW_REG
        )
    }
}

// Used to define subroutines and then call them
// See JitCompiler::set_anchor() and JitCompiler::relative_to_anchor()
#[cfg(feature = "tracer")]
const ANCHOR_TRACE: usize = 0;
const ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS: usize = 1;
const ANCHOR_EPILOGUE: usize = 2;
const ANCHOR_THROW_EXCEPTION_UNCHECKED: usize = 3;
const ANCHOR_EXIT: usize = 4;
const ANCHOR_THROW_EXCEPTION: usize = 5;
const ANCHOR_CALL_DEPTH_EXCEEDED: usize = 6;
const ANCHOR_CALL_REG_OUTSIDE_TEXT_SEGMENT: usize = 7;
const ANCHOR_DIV_BY_ZERO: usize = 8;
const ANCHOR_DIV_OVERFLOW: usize = 9;
const ANCHOR_CALL_REG_UNSUPPORTED_INSTRUCTION: usize = 10;
const ANCHOR_CALL_UNSUPPORTED_INSTRUCTION: usize = 11;
const ANCHOR_EXTERNAL_FUNCTION_CALL: usize = 12;
const ANCHOR_CALL_REG_DEPTH_EXCEEDED: usize = 13;
const ANCHOR_INTERNAL_FUNCTION_CALL_REG: usize = 14;
const ANCHOR_FIRST_ATTEMPT_SUCCESS: usize = 15;
const ANCHOR_RECOVER_MEMORY_PC: usize = 16;
const ANCHOR_CALL_REG_LDDW_UNSUPPORTED: usize = 17;
const ANCHOR_TRANSLATE_MEMORY_ADDRESS: usize = 23;
const ANCHOR_COUNT: usize = 35; // Update me when adding or removing anchors

/// Mapping of guest to host registers
pub const REGISTER_MAP: [X86Register; 11] = [
    CALLER_SAVED_REGISTERS[0], // RAX
    ARGUMENT_REGISTERS[1],     // RSI
    ARGUMENT_REGISTERS[2],     // RDX
    ARGUMENT_REGISTERS[3],     // RCX
    ARGUMENT_REGISTERS[4],     // R8
    ARGUMENT_REGISTERS[5],     // R9
    CALLEE_SAVED_REGISTERS[1], // RBX
    CALLEE_SAVED_REGISTERS[2], // R12
    CALLEE_SAVED_REGISTERS[3], // R13
    CALLEE_SAVED_REGISTERS[4], // R14
    CALLEE_SAVED_REGISTERS[5], // R15
];

/// RDI: Used together with slot_in_vm()
pub const REGISTER_PTR_TO_VM: X86Register = ARGUMENT_REGISTERS[0];
/// R10: Program counter limit
pub const REGISTER_INSTRUCTION_METER: X86Register = CALLER_SAVED_REGISTERS[7];
/// R11: Scratch register
pub const REGISTER_SCRATCH: X86Register = CALLER_SAVED_REGISTERS[8];

/// Bit width of an instruction operand
#[derive(Copy, Clone, Debug)]
pub enum OperandSize {
    /// Empty
    S0 = 0,
    /// 8 bit
    S8 = 8,
    /// 16 bit
    S16 = 16,
    /// 32 bit
    S32 = 32,
    /// 64 bit
    S64 = 64,
}

/// Instruction operand
///
/// Last boolean on variants flags user provided values.
pub enum Value {
    /// Register value used as is
    Register(X86Register),
    /// Register dereferenced via memory load
    RegisterIndirect(X86Register, i32, bool),
    /// Register value plus a i32
    RegisterPlusConstant32(X86Register, i32, bool),
    /// Register value plus a i64
    RegisterPlusConstant64(X86Register, i64, bool),
    /// Constant i64
    Constant64(i64, bool),
}

/// Function call argument
pub struct Argument {
    /// Position in the function signature
    pub index: usize,
    /// Value to pass in that position
    pub value: Value,
}

#[derive(Debug)]
struct Jump {
    location: *const u8,
    target_pc: usize,
}

#[derive(Copy, Clone, Debug)]
struct InternalCallInfo {
    clobber_mask: u8,
    save_frame_ptr: bool,
    bump_frame_ptr: bool,
}

impl InternalCallInfo {
    const fn conservative(manual_stack_frame_bump: bool) -> Self {
        Self {
            clobber_mask: 0b1111,
            save_frame_ptr: true,
            bump_frame_ptr: !manual_stack_frame_bump,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum ColdStubKind {
    CallDepthExceeded,
    RootExit,
}

#[derive(Copy, Clone, Debug)]
struct ColdStub {
    jcc_end_offset: usize,
    pc: usize,
    kind: ColdStubKind,
}

#[derive(Clone, Copy, Debug)]
struct ForwardBranchStub {
    jcc_imm_offset: u32,
    pc: u32,
    target_pc: u32,
}

/* Explanation of the Instruction Meter

    The instruction meter serves two purposes: First, measure how many BPF instructions are
    executed (profiling) and second, limit this number by stopping the program with an exception
    once a given threshold is reached (validation). One approach would be to increment and
    validate the instruction meter before each instruction. However, this would heavily impact
    performance. Thus, we only profile and validate the instruction meter at branches.

    For this, we implicitly sum up all the instructions between two branches.
    It is easy to know the end of such a slice of instructions, but how do we know where it
    started? There could be multiple ways to jump onto a path which all lead to the same final
    branch. This is, where the integral technique comes in. The program is basically a sequence
    of instructions with the x-axis being the program counter (short "pc"). The cost function is
    a constant function which returns one for every point on the x axis. Now, the instruction
    meter needs to calculate the definite integral of the cost function between the start and the
    end of the current slice of instructions. For that we need the indefinite integral of the cost
    function. Fortunately, the derivative of the pc is the cost function (it increases by one for
    every instruction), thus the pc is an antiderivative of the the cost function and a valid
    indefinite integral. So, to calculate an definite integral of the cost function, we just need
    to subtract the start pc from the end pc of the slice. This difference can then be subtracted
    from the remaining instruction counter until it goes below zero at which point it reaches
    the instruction meter limit. Ok, but how do we know the start of the slice at the end?

    The trick is: We do not need to know. As subtraction and addition are associative operations,
    we can reorder them, even beyond the current branch. Thus, we can simply account for the
    amount the start will subtract at the next branch by already adding that to the remaining
    instruction counter at the current branch. So, every branch just subtracts its current pc
    (the end of the slice) and adds the target pc (the start of the next slice) to the remaining
    instruction counter. This way, no branch needs to know the pc of the last branch explicitly.
    Another way to think about this trick is as follows: The remaining instruction counter now
    measures what the maximum pc is, that we can reach with the remaining budget after the last
    branch.

    For conditional branches, forward branches emit only the comparison and conditional jump inline
    so untaken fallthroughs pay zero metering overhead; taken forward branches jump to an
    out-of-line stub that validates and updates the meter before jumping to target_pc. Backward
    branches validate inline and invert the branch condition into a short jump around the meter
    subtraction and backward jump, avoiding speculative meter updates and exit undo operations.

    Another special case to keep in mind are return instructions. They would require us to know
    the return address (target pc), but in the JIT we already converted that to be a host address.
    Of course, one could also save the BPF return address on the stack, but an even simpler
    solution exists: Just count as if you were jumping to an specific target pc before the exit,
    and then after returning use the undo profiling. The trick is, that the undo profiling now
    has the current pc which is the BPF return address. The virtual target pc we count towards
    and undo again can be anything, so we just set it to zero.
*/

/// Temporary object which stores the compilation context
pub struct JitCompiler<'a, C: ContextObject> {
    result: UnsealedJitProgram,
    text_ptr: *mut u8,
    text_len: usize,
    pc_section_ptr: *mut u32,
    pc_section_len: usize,
    text_section_jumps: Vec<Jump>,
    memory_pc_entries: Vec<(u32, u32)>,
    forward_branch_stubs: Vec<ForwardBranchStub>,
    anchors: [*const u8; ANCHOR_COUNT],
    offset_in_text_section: usize,
    executable: &'a Executable<C>,
    program: &'a [u8],
    program_vm_addr: u64,
    config: &'a Config,
    pc: usize,
    last_instruction_meter_validation_pc: usize,
    next_noop_insertion: u32,
    noop_range: Option<Uniform<u32>>,
    runtime_environment_key: i32,
    immediate_value_key: i64,
    diversification_rng: SmallRng,
    stopwatch_is_active: bool,
    inline_v3_stack: bool,
    inline_fast_mem: bool,
    internal_call_info: Vec<InternalCallInfo>,
    /// For each pc, its index into `internal_call_info`, or `u32::MAX` if it is not a call target
    call_target_index: Vec<u32>,
    is_lddw_second_slot: Vec<bool>,
    cold_stubs: Vec<ColdStub>,
    has_bmi2: bool,
}

#[rustfmt::skip]
impl<'a, C: ContextObject> JitCompiler<'a, C> {
    fn analyze_internal_functions(
        executable: &'a Executable<C>,
        program: &'a [u8],
        mut target_pcs: Vec<usize>,
        is_lddw_second_slot: Vec<bool>,
        inline_stack_frame_size: Option<i64>,
    ) -> (Vec<InternalCallInfo>, Vec<u32>, Vec<bool>) {
        // In-frame `[r10 + off]` accesses compiled by `emit_load` / `emit_store_*` never reach host code.
        let is_inline_stack_access = |base_reg: u8, off: i16, len: i64| match inline_stack_frame_size {
            Some(frame_size) => {
                let off = off as i64;
                base_reg == FRAME_PTR_REG as u8 && -frame_size <= off && off + len <= 0
            }
            None => false,
        };
        let legacy_mem_access_len = |opc: u8| -> i64 {
            match opc {
                ebpf::LD_B_REG | ebpf::ST_B_IMM | ebpf::ST_B_REG => 1,
                ebpf::LD_H_REG | ebpf::ST_H_IMM | ebpf::ST_H_REG => 2,
                ebpf::LD_W_REG | ebpf::ST_W_IMM | ebpf::ST_W_REG => 4,
                _ => 8,
            }
        };
        let sbpf_version = executable.get_sbpf_version();
        let num_insns = program.len() / ebpf::INSN_SIZE;
        let is_lddw_slot = |pc: usize, slots: &[bool]| slots.get(pc).copied().unwrap_or(false);
        target_pcs.retain(|&t_pc| {
            t_pc < num_insns
                && !is_lddw_slot(t_pc, &is_lddw_second_slot)
                && ebpf::get_insn_unchecked(program, t_pc).opc != 0
        });
        if target_pcs.is_empty() {
            return (Vec::new(), Vec::new(), is_lddw_second_slot);
        }
        target_pcs.sort_unstable();
        target_pcs.dedup();
        let mut target_index = vec![u32::MAX; num_insns];
        for (idx, &target_pc) in target_pcs.iter().enumerate() {
            target_index[target_pc] = idx as u32;
        }

        struct FuncSummary {
            clobber_mask: u8,
            uses_r10: bool,
            has_nested_call: bool,
            reaches_host_call: bool,
            reaches_call_reg: bool,
            intra_edges: Vec<usize>,
            call_edges: Vec<usize>,
        }

        let mut summaries = Vec::with_capacity(target_pcs.len());
        let mut visited_gen = vec![0u32; num_insns];
        let mut worklist: Vec<usize> = Vec::new();
        let mut gen = 0u32;
        let mut steps_used = 0usize;
        let max_steps = num_insns.saturating_mul(8).max(4096);

        for &entry_pc in &target_pcs {
            let mut summary = FuncSummary {
                clobber_mask: 0,
                uses_r10: false,
                has_nested_call: false,
                reaches_host_call: cfg!(feature = "tracer"),
                reaches_call_reg: false,
                intra_edges: Vec::new(),
                call_edges: Vec::new(),
            };
            if steps_used >= max_steps {
                summary.reaches_call_reg = true;
                summary.reaches_host_call = true;
                summaries.push(summary);
                continue;
            }
            gen = gen.wrapping_add(1);
            if gen == 0 {
                visited_gen.fill(0);
                gen = 1;
            }
            visited_gen[entry_pc] = gen;
            worklist.clear();
            worklist.push(entry_pc);

            macro_rules! push_succ {
                ($succ_pc:expr) => {{
                    let succ = $succ_pc;
                    if succ < num_insns {
                        if is_lddw_slot(succ, &is_lddw_second_slot) {
                            summary.reaches_call_reg = true;
                        } else if succ != entry_pc {
                            let other_idx = target_index[succ] as usize;
                            if other_idx != u32::MAX as usize {
                                if !summary.intra_edges.contains(&other_idx) {
                                    summary.intra_edges.push(other_idx);
                                }
                            } else if visited_gen[succ] != gen {
                                visited_gen[succ] = gen;
                                worklist.push(succ);
                            }
                        } else if visited_gen[succ] != gen {
                            visited_gen[succ] = gen;
                            worklist.push(succ);
                        }
                    }
                }};
            }

            while let Some(pc) = worklist.pop() {
                steps_used += 1;
                if steps_used > max_steps {
                    summary.reaches_call_reg = true;
                    worklist.clear();
                    break;
                }
                let insn = ebpf::get_insn_unchecked(program, pc);
                if insn.opc != ebpf::CALL_IMM && (insn.dst == 10 || insn.src == 10) {
                    summary.uses_r10 = true;
                }
                let j_target = (pc as isize).wrapping_add(insn.off as isize).wrapping_add(1) as usize;
                match insn.opc {
                    ebpf::EXIT => {}
                    ebpf::JA => {
                        push_succ!(j_target);
                    }
                    ebpf::JEQ64_IMM
                    | ebpf::JEQ64_REG
                    | ebpf::JGT64_IMM
                    | ebpf::JGT64_REG
                    | ebpf::JGE64_IMM
                    | ebpf::JGE64_REG
                    | ebpf::JLT64_IMM
                    | ebpf::JLT64_REG
                    | ebpf::JLE64_IMM
                    | ebpf::JLE64_REG
                    | ebpf::JSET64_IMM
                    | ebpf::JSET64_REG
                    | ebpf::JNE64_IMM
                    | ebpf::JNE64_REG
                    | ebpf::JSGT64_IMM
                    | ebpf::JSGT64_REG
                    | ebpf::JSGE64_IMM
                    | ebpf::JSGE64_REG
                    | ebpf::JSLT64_IMM
                    | ebpf::JSLT64_REG
                    | ebpf::JSLE64_IMM
                    | ebpf::JSLE64_REG => {
                        push_succ!(pc + 1);
                        push_succ!(j_target);
                    }
                    ebpf::JEQ32_IMM
                    | ebpf::JEQ32_REG
                    | ebpf::JGT32_IMM
                    | ebpf::JGT32_REG
                    | ebpf::JGE32_IMM
                    | ebpf::JGE32_REG
                    | ebpf::JLT32_IMM
                    | ebpf::JLT32_REG
                    | ebpf::JLE32_IMM
                    | ebpf::JLE32_REG
                    | ebpf::JSET32_IMM
                    | ebpf::JSET32_REG
                    | ebpf::JNE32_IMM
                    | ebpf::JNE32_REG
                    | ebpf::JSGT32_IMM
                    | ebpf::JSGT32_REG
                    | ebpf::JSGE32_IMM
                    | ebpf::JSGE32_REG
                    | ebpf::JSLT32_IMM
                    | ebpf::JSLT32_REG
                    | ebpf::JSLE32_IMM
                    | ebpf::JSLE32_REG
                        if sbpf_version.enable_jmp32() =>
                    {
                        push_succ!(pc + 1);
                        push_succ!(j_target);
                    }
                    ebpf::CALL_REG => {
                        summary.reaches_call_reg = true;
                        summary.reaches_host_call = true;
                        push_succ!(pc + 1);
                    }
                    ebpf::CALL_IMM => {
                        let mut resolved_external = false;
                        let mut resolved_internal: Option<usize> = None;
                        if (!sbpf_version.static_syscalls() || insn.src == 0)
                            && executable
                                .get_loader()
                                .get_function_registry()
                                .lookup_by_key(insn.imm as u32)
                                .is_some()
                        {
                            resolved_external = true;
                            summary.reaches_host_call = true;
                        }
                        if sbpf_version.static_syscalls() {
                            if insn.src == 1 {
                                let target = (pc as i64).saturating_add(insn.imm).saturating_add(1);
                                if target >= 0 && ebpf::is_pc_in_program(program, target as usize) {
                                    resolved_internal = Some(target as usize);
                                }
                            }
                        } else if let Some((_name, t_pc)) =
                            executable.get_function_registry().lookup_by_key(insn.imm as u32)
                        {
                            if ebpf::is_pc_in_program(program, t_pc) {
                                resolved_internal = Some(t_pc);
                            } else {
                                summary.reaches_call_reg = true;
                            }
                        }
                        if let Some(callee_pc) = resolved_internal {
                            summary.has_nested_call = true;
                            let callee_idx = target_index.get(callee_pc).copied().unwrap_or(u32::MAX) as usize;
                            if callee_idx != u32::MAX as usize {
                                if !summary.call_edges.contains(&callee_idx) {
                                    summary.call_edges.push(callee_idx);
                                }
                            } else {
                                summary.reaches_call_reg = true;
                            }
                            push_succ!(pc + 1);
                        } else if resolved_external {
                            push_succ!(pc + 1);
                        } else {
                            summary.reaches_call_reg = true;
                        }
                    }
                    ebpf::LD_DW_IMM if !sbpf_version.disable_lddw() => {
                        if (6..=9).contains(&insn.dst) {
                            summary.clobber_mask |= 1 << (insn.dst - 6);
                        }
                        push_succ!(pc + 2);
                    }
                    ebpf::ST_B_IMM
                    | ebpf::ST_H_IMM
                    | ebpf::ST_W_IMM
                    | ebpf::ST_DW_IMM
                    | ebpf::ST_B_REG
                    | ebpf::ST_H_REG
                    | ebpf::ST_W_REG
                    | ebpf::ST_DW_REG
                        if !sbpf_version.move_memory_instruction_classes() =>
                    {
                        if !is_inline_stack_access(insn.dst, insn.off, legacy_mem_access_len(insn.opc)) {
                            summary.reaches_host_call = true;
                        }
                        push_succ!(pc + 1);
                    }
                    ebpf::ST_1B_IMM
                    | ebpf::ST_2B_IMM
                    | ebpf::ST_4B_IMM
                    | ebpf::ST_8B_IMM
                    | ebpf::ST_1B_REG
                    | ebpf::ST_2B_REG
                    | ebpf::ST_4B_REG
                    | ebpf::ST_8B_REG
                        if sbpf_version.move_memory_instruction_classes() =>
                    {
                        summary.reaches_host_call = true;
                        push_succ!(pc + 1);
                    }
                    ebpf::LD_B_REG
                    | ebpf::LD_H_REG
                    | ebpf::LD_W_REG
                    | ebpf::LD_DW_REG
                        if !sbpf_version.move_memory_instruction_classes() =>
                    {
                        if (6..=9).contains(&insn.dst) {
                            summary.clobber_mask |= 1 << (insn.dst - 6);
                        }
                        if !is_inline_stack_access(insn.src, insn.off, legacy_mem_access_len(insn.opc)) {
                            summary.reaches_host_call = true;
                        }
                        push_succ!(pc + 1);
                    }
                    ebpf::LD_1B_REG
                    | ebpf::LD_2B_REG
                    | ebpf::LD_4B_REG
                    | ebpf::LD_8B_REG
                        if sbpf_version.move_memory_instruction_classes() =>
                    {
                        if (6..=9).contains(&insn.dst) {
                            summary.clobber_mask |= 1 << (insn.dst - 6);
                        }
                        summary.reaches_host_call = true;
                        push_succ!(pc + 1);
                    }
                    _ => {
                        if (6..=9).contains(&insn.dst) {
                            summary.clobber_mask |= 1 << (insn.dst - 6);
                        }
                        push_succ!(pc + 1);
                    }
                }
            }
            if summary.reaches_call_reg {
                summary.clobber_mask = 0b1111;
                summary.reaches_host_call = true;
            }
            summaries.push(summary);
        }

        loop {
            let mut changed = false;
            for i in 0..summaries.len() {
                let mut clobber = summaries[i].clobber_mask;
                let mut uses_r10 = summaries[i].uses_r10;
                let mut has_nested_call = summaries[i].has_nested_call;
                let mut reaches_host_call = summaries[i].reaches_host_call;
                let mut reaches_call_reg = summaries[i].reaches_call_reg;

                for k in 0..summaries[i].intra_edges.len() {
                    let j = summaries[i].intra_edges[k];
                    clobber |= summaries[j].clobber_mask;
                    uses_r10 |= summaries[j].uses_r10;
                    has_nested_call |= summaries[j].has_nested_call;
                    reaches_host_call |= summaries[j].reaches_host_call;
                    reaches_call_reg |= summaries[j].reaches_call_reg;
                }
                for k in 0..summaries[i].call_edges.len() {
                    let j = summaries[i].call_edges[k];
                    clobber |= summaries[j].clobber_mask;
                    reaches_host_call |= summaries[j].reaches_host_call;
                }
                if reaches_call_reg {
                    clobber = 0b1111;
                    reaches_host_call = true;
                }
                if clobber != summaries[i].clobber_mask
                    || uses_r10 != summaries[i].uses_r10
                    || has_nested_call != summaries[i].has_nested_call
                    || reaches_host_call != summaries[i].reaches_host_call
                    || reaches_call_reg != summaries[i].reaches_call_reg
                {
                    summaries[i].clobber_mask = clobber;
                    summaries[i].uses_r10 = uses_r10;
                    summaries[i].has_nested_call = has_nested_call;
                    summaries[i].reaches_host_call = reaches_host_call;
                    summaries[i].reaches_call_reg = reaches_call_reg;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let manual_bump = sbpf_version.manual_stack_frame_bump();
        let mut out = Vec::with_capacity(summaries.len());
        for s in &summaries {
            let needs_frame_bump = s.reaches_call_reg
                || s.uses_r10
                || s.has_nested_call
                || manual_bump
                || cfg!(feature = "tracer");
            let mut clobber_mask = if s.reaches_call_reg { 0b1111 } else { s.clobber_mask };
            let mut save_frame_ptr = needs_frame_bump;
            let bump_frame_ptr = needs_frame_bump && !manual_bump;

            if s.reaches_host_call {
                let num_pushed = clobber_mask.count_ones() + u32::from(save_frame_ptr);
                if num_pushed.is_multiple_of(2) {
                    if clobber_mask != 0b1111 {
                        clobber_mask |= 1 << (!clobber_mask).trailing_zeros();
                    } else {
                        save_frame_ptr = true;
                    }
                }
            }

            out.push(InternalCallInfo {
                clobber_mask,
                save_frame_ptr,
                bump_frame_ptr,
            });
        }

        (out, target_index, is_lddw_second_slot)
    }

    /// Constructs a new compiler and allocates memory for the compilation output
    pub fn new(executable: &'a Executable<C>) -> Result<Self, EbpfError> {
        let config = executable.get_config();
        let sbpf_version = executable.get_sbpf_version();
        let (program_vm_addr, program) = executable.get_text_bytes();

        // Scan through program to find actual number of instructions and check whether r10 is ever written
        let mut pc = 0;
        let mut r10_never_written = true;
        let mut call_target_pcs: Vec<usize> = Vec::new();
        let mut is_lddw_second_slot: Vec<bool> = Vec::new();
        let disable_lddw = sbpf_version.disable_lddw();
        let num_insns = program.len() / ebpf::INSN_SIZE;
        while (pc + 1) * ebpf::INSN_SIZE <= program.len() {
            let insn = ebpf::get_insn_unchecked(program, pc);
            if insn.dst == FRAME_PTR_REG as u8 && insn_writes_dst(insn.opc, sbpf_version) {
                r10_never_written = false;
            }
            match insn.opc {
                ebpf::LD_DW_IMM if !disable_lddw => {
                    if pc + 1 < num_insns {
                        if is_lddw_second_slot.is_empty() {
                            is_lddw_second_slot.resize(num_insns, false);
                        }
                        is_lddw_second_slot[pc + 1] = true;
                    }
                    pc += 2;
                    continue;
                }
                ebpf::CALL_IMM => {
                    let target_pc = if sbpf_version.static_syscalls() {
                        if insn.src == 1 {
                            let target = (pc as i64).saturating_add(insn.imm).saturating_add(1);
                            (target >= 0).then_some(target as usize)
                        } else {
                            None
                        }
                    } else {
                        executable
                            .get_function_registry()
                            .lookup_by_key(insn.imm as u32)
                            .map(|(_name, target_pc)| target_pc)
                    };
                    call_target_pcs.extend(target_pc);
                }
                _ => {}
            }
            pc += 1;
        }

        let inline_v3_stack = cfg!(not(feature = "jit-enable-host-stack-frames"))
            && sbpf_version == SBPFVersion::V3
            && r10_never_written
            && config.max_call_depth > 0
            && config.stack_frame_size > 0
            && config.stack_frame_size.checked_mul(config.max_call_depth).is_some();
        let inline_fast_mem = !sbpf_version.stack_frame_gaps() || !config.enable_stack_frame_gaps;
        let (internal_call_info, call_target_index, is_lddw_second_slot) = if call_target_pcs.is_empty() {
            (Vec::new(), Vec::new(), is_lddw_second_slot)
        } else {
            let inline_stack_frame_size = inline_v3_stack.then_some(config.stack_frame_size as i64);
            Self::analyze_internal_functions(executable, program, call_target_pcs, is_lddw_second_slot, inline_stack_frame_size)
        };

        let mut code_length_estimate = MAX_EMPTY_PROGRAM_MACHINE_CODE_LENGTH + MAX_START_PADDING_LENGTH + MAX_MACHINE_CODE_LENGTH_PER_INSTRUCTION * pc;
        if config.noop_instruction_rate != 0 {
            code_length_estimate += code_length_estimate / config.noop_instruction_rate as usize;
        }
        if let Some(q) = pc.checked_div(config.instruction_meter_checkpoint_distance) {
            code_length_estimate += q * MACHINE_CODE_PER_INSTRUCTION_METER_CHECKPOINT;
        }
        // Relative jump destinations limit the maximum output size
        debug_assert!(code_length_estimate < (i32::MAX as usize));

        let runtime_environment_key = get_runtime_environment_key();
        #[cfg(not(feature = "shuttle-test"))]
        let (mut diversification_rng, immediate_value_key) = if config.noop_instruction_rate == 0 {
            let key = if config.sanitize_user_provided_values {
                JIT_DIVERSIFICATION_RNG.with(|rng| rng.borrow_mut().gen::<i64>())
            } else {
                0
            };
            (SmallRng::seed_from_u64(0), key)
        } else {
            let seed = JIT_DIVERSIFICATION_RNG.with(|rng| rng.borrow_mut().gen::<u64>());
            let mut rng = SmallRng::seed_from_u64(seed);
            let key = if config.sanitize_user_provided_values {
                rng.gen::<i64>()
            } else {
                0
            };
            (rng, key)
        };
        #[cfg(feature = "shuttle-test")]
        let (mut diversification_rng, immediate_value_key) = {
            let mut rng = SmallRng::from_rng(thread_rng()).map_err(|_| EbpfError::JitNotCompiled)?;
            let key = rng.gen::<i64>();
            (rng, key)
        };

        let (next_noop_insertion, noop_range) = if config.noop_instruction_rate == 0 {
            (u32::MAX, None)
        } else {
            (
                diversification_rng.gen_range(0..config.noop_instruction_rate * 2),
                Some(Uniform::new_inclusive(0, config.noop_instruction_rate * 2)),
            )
        };

        let mut result = UnsealedJitProgram::new(pc, code_length_estimate);
        result.set_fast_path_flags(inline_v3_stack, inline_fast_mem);
        let text_len = result.text_section().len();
        let text_ptr = result.text_section_mut().as_mut_ptr();
        let pc_section_len = result.pc_section().len();
        let pc_section_ptr = result.pc_section_mut().as_mut_ptr();

        Ok(Self {
            result,
            text_ptr,
            text_len,
            pc_section_ptr,
            pc_section_len,
            text_section_jumps: vec![],
            memory_pc_entries: Vec::new(),
            forward_branch_stubs: vec![],
            anchors: [std::ptr::null(); ANCHOR_COUNT],
            offset_in_text_section: 0,
            executable,
            program_vm_addr,
            program,
            config,
            pc: 0,
            last_instruction_meter_validation_pc: 0,
            next_noop_insertion,
            noop_range,
            runtime_environment_key,
            immediate_value_key,
            diversification_rng,
            stopwatch_is_active: false,
            inline_v3_stack,
            inline_fast_mem,
            internal_call_info,
            call_target_index,
            is_lddw_second_slot,
            cold_stubs: Vec::new(),
            has_bmi2: std::arch::is_x86_feature_detected!("bmi2"),
        })
    }

    /// Compiles the given executable, consuming the compiler
    pub fn compile(mut self) -> Result<JitProgram, EbpfError> {
        self.text_ptr = self.result.text_section_mut().as_mut_ptr();
        self.pc_section_ptr = self.result.pc_section_mut().as_mut_ptr();

        // Randomized padding at the start before random intervals begin
        if self.config.noop_instruction_rate != 0 {
            for _ in 0..self.diversification_rng.gen_range(0..MAX_START_PADDING_LENGTH) {
                // X86Instruction::noop().emit(self)?;
                self.emit::<u8>(0x90);
            }
        }

        self.emit_subroutines();

        let sbpf_version = self.executable.get_sbpf_version();
        let checkpoint_distance = self.config.instruction_meter_checkpoint_distance;
        let program_len = self.program.len();

        while (self.pc + 1) * ebpf::INSN_SIZE <= program_len {
            if self.offset_in_text_section + self.cold_stubs.len() * 32 + self.forward_branch_stubs.len() * 64 + MAX_MACHINE_CODE_LENGTH_PER_INSTRUCTION * 2 >= self.text_len {
                return Err(EbpfError::ExhaustedTextSegment(self.pc));
            }
            let mut insn = ebpf::get_insn_unchecked(self.program, self.pc);
            debug_assert!(self.pc < self.pc_section_len);
            unsafe {
                *self.pc_section_ptr.add(self.pc) = self.offset_in_text_section as u32;
            }

            // Regular instruction meter checkpoints to prevent long linear runs from exceeding their budget
            if self.last_instruction_meter_validation_pc + checkpoint_distance <= self.pc {
                self.emit_validate_instruction_count(Some(self.pc));
            }

            #[cfg(feature = "tracer")]
            {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.pc as i64));
                self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_TRACE, 5)));
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, 0));
            }

            let dst = REGISTER_MAP[insn.dst as usize];
            let src = REGISTER_MAP[insn.src as usize];
            let target_pc = (self.pc as isize + insn.off as isize + 1) as usize;

            match insn.opc {
                ebpf::LD_DW_IMM if !sbpf_version.disable_lddw() => {
                    self.emit_validate_and_profile_instruction_count(self.pc + 2);
                    self.pc += 1;
                    let unsupported_offset = unsafe { self.anchors[ANCHOR_CALL_UNSUPPORTED_INSTRUCTION].offset_from(self.text_ptr) as u32 };
                    debug_assert!(self.pc < self.pc_section_len);
                    unsafe {
                        *self.pc_section_ptr.add(self.pc) = unsupported_offset;
                    }
                    ebpf::augment_lddw_unchecked(self.program, &mut insn);
                    if self.should_sanitize_constant(insn.imm) {
                        self.emit_sanitized_load_immediate(dst, insn.imm);
                    } else {
                        self.emit_ins(X86Instruction::load_immediate(dst, insn.imm));
                    }
                },

                // BPF_LDX class
                ebpf::LD_B_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_load(OperandSize::S8, 1, insn.src, dst, src, insn.off);
                },
                ebpf::LD_H_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_load(OperandSize::S16, 2, insn.src, dst, src, insn.off);
                },
                ebpf::LD_W_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_load(OperandSize::S32, 4, insn.src, dst, src, insn.off);
                },
                ebpf::LD_DW_REG if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_load(OperandSize::S64, 8, insn.src, dst, src, insn.off);
                },

                // BPF_ST class
                ebpf::ST_B_IMM  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_imm(OperandSize::S8, 1, insn.dst, dst, insn.off, insn.imm);
                },
                ebpf::ST_H_IMM  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_imm(OperandSize::S16, 2, insn.dst, dst, insn.off, insn.imm);
                },
                ebpf::ST_W_IMM  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_imm(OperandSize::S32, 4, insn.dst, dst, insn.off, insn.imm);
                },
                ebpf::ST_DW_IMM if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_imm(OperandSize::S64, 8, insn.dst, dst, insn.off, insn.imm);
                },

                // BPF_STX class
                ebpf::ST_B_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_reg(OperandSize::S8, 1, insn.dst, dst, src, insn.off);
                },
                ebpf::ST_H_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_reg(OperandSize::S16, 2, insn.dst, dst, src, insn.off);
                },
                ebpf::ST_W_REG  if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_reg(OperandSize::S32, 4, insn.dst, dst, src, insn.off);
                },
                ebpf::ST_DW_REG if !sbpf_version.move_memory_instruction_classes() => {
                    self.emit_store_reg(OperandSize::S64, 8, insn.dst, dst, src, insn.off);
                },

                // BPF_ALU32_LOAD class
                ebpf::ADD32_IMM  => {
                    self.emit_sanitized_alu(OperandSize::S32, 0x01, 0, dst, insn.imm);
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::ADD32_REG  => {
                    self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x01, src, dst, None));
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::SUB32_IMM  => {
                    if sbpf_version.swap_sub_reg_imm_operands() {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0xf7, 3, dst, 0, None));
                        if insn.imm != 0 {
                            self.emit_sanitized_alu(OperandSize::S32, 0x01, 0, dst, insn.imm);
                        }
                    } else {
                        self.emit_sanitized_alu(OperandSize::S32, 0x29, 5, dst, insn.imm);
                    }
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::SUB32_REG  => {
                    self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x29, src, dst, None));
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::MUL32_IMM if !sbpf_version.enable_pqr() => {
                    if !self.should_sanitize_constant(insn.imm) && i32::try_from(insn.imm).is_ok() {
                        self.emit_ins(X86Instruction::imul_immediate(OperandSize::S32, dst, dst, insn.imm));
                    } else {
                        if self.should_sanitize_constant(insn.imm) {
                            self.emit_sanitized_load_immediate(REGISTER_SCRATCH, insn.imm);
                        } else {
                            self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, insn.imm));
                        }
                        self.emit_ins(X86Instruction::alu_escaped(OperandSize::S32, 1, 0xaf, dst, REGISTER_SCRATCH, None));
                    }
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::DIV32_IMM | ebpf::MOD32_IMM if !sbpf_version.enable_pqr() =>
                    self.emit_product_quotient_remainder(
                        OperandSize::S32,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MOD,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) != ebpf::BPF_MUL,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MUL,
                        dst, dst, Some(insn.imm),
                    ),
                ebpf::LD_1B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(Some(dst), Value::RegisterPlusConstant64(src, insn.off as i64, true), 1, None);
                },
                ebpf::MUL32_REG if !sbpf_version.enable_pqr() => {
                    self.emit_ins(X86Instruction::alu_escaped(OperandSize::S32, 1, 0xaf, dst, src, None));
                    if !sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                },
                ebpf::DIV32_REG | ebpf::MOD32_REG if !sbpf_version.enable_pqr() =>
                    self.emit_product_quotient_remainder(
                        OperandSize::S32,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MOD,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) != ebpf::BPF_MUL,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MUL,
                        src, dst, None,
                    ),
                ebpf::LD_2B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(Some(dst), Value::RegisterPlusConstant64(src, insn.off as i64, true), 2, None);
                },
                ebpf::OR32_IMM   => self.emit_sanitized_alu(OperandSize::S32, 0x09, 1, dst, insn.imm),
                ebpf::OR32_REG   => self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x09, src, dst, None)),
                ebpf::AND32_IMM  => self.emit_sanitized_alu(OperandSize::S32, 0x21, 4, dst, insn.imm),
                ebpf::AND32_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x21, src, dst, None)),
                ebpf::LSH32_IMM  => self.emit_shift(OperandSize::S32, 4, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::LSH32_REG  => self.emit_shift(OperandSize::S32, 4, src, dst, None),
                ebpf::RSH32_IMM  => self.emit_shift(OperandSize::S32, 5, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::RSH32_REG  => self.emit_shift(OperandSize::S32, 5, src, dst, None),
                ebpf::NEG32      if !sbpf_version.disable_neg() => self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0xf7, 3, dst, 0, None)),
                ebpf::LD_4B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(Some(dst), Value::RegisterPlusConstant64(src, insn.off as i64, true), 4, None);
                },
                ebpf::LD_8B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(Some(dst), Value::RegisterPlusConstant64(src, insn.off as i64, true), 8, None);
                },
                ebpf::XOR32_IMM  => self.emit_sanitized_alu(OperandSize::S32, 0x31, 6, dst, insn.imm),
                ebpf::XOR32_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x31, src, dst, None)),
                ebpf::MOV32_IMM  => {
                    if self.should_sanitize_constant(insn.imm) {
                        self.emit_sanitized_load_immediate(dst, insn.imm as u32 as u64 as i64);
                    } else {
                        self.emit_ins(X86Instruction::load_immediate(dst, insn.imm as u32 as u64 as i64));
                    }
                }
                ebpf::MOV32_REG  => {
                    if sbpf_version.explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::mov_with_sign_extension(OperandSize::S64, src, dst));
                    } else {
                        self.emit_ins(X86Instruction::mov(OperandSize::S32, src, dst));
                    }
                }
                ebpf::ARSH32_IMM => self.emit_shift(OperandSize::S32, 7, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::ARSH32_REG => self.emit_shift(OperandSize::S32, 7, src, dst, None),
                ebpf::LE if !sbpf_version.disable_le() => {
                    match insn.imm {
                        16 => {
                            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0x81, 4, dst, 0xffff, None)); // Mask to 16 bit
                        }
                        32 => {
                            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0x81, 4, dst, -1, None)); // Mask to 32 bit
                        }
                        64 => {}
                        _ => {
                            return Err(EbpfError::InvalidInstruction);
                        }
                    }
                },
                ebpf::BE         => {
                    match insn.imm {
                        16 => {
                            self.emit_ins(X86Instruction::bswap(OperandSize::S16, dst));
                            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0x81, 4, dst, 0xffff, None)); // Mask to 16 bit
                        }
                        32 => self.emit_ins(X86Instruction::bswap(OperandSize::S32, dst)),
                        64 => self.emit_ins(X86Instruction::bswap(OperandSize::S64, dst)),
                        _ => {
                            return Err(EbpfError::InvalidInstruction);
                        }
                    }
                },

                // BPF_ALU64_STORE class
                ebpf::ADD64_IMM  => self.emit_sanitized_alu(OperandSize::S64, 0x01, 0, dst, insn.imm),
                ebpf::ADD64_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, src, dst, None)),
                ebpf::SUB64_IMM  => {
                    if sbpf_version.swap_sub_reg_imm_operands() {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 3, dst, 0, None));
                        if insn.imm != 0 {
                            self.emit_sanitized_alu(OperandSize::S64, 0x01, 0, dst, insn.imm);
                        }
                    } else {
                        self.emit_sanitized_alu(OperandSize::S64, 0x29, 5, dst, insn.imm);
                    }
                }
                ebpf::SUB64_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, src, dst, None)),
                ebpf::MUL64_IMM if !sbpf_version.enable_pqr() => {
                    if !self.should_sanitize_constant(insn.imm) && i32::try_from(insn.imm).is_ok() {
                        self.emit_ins(X86Instruction::imul_immediate(OperandSize::S64, dst, dst, insn.imm));
                    } else {
                        if self.should_sanitize_constant(insn.imm) {
                            self.emit_sanitized_load_immediate(REGISTER_SCRATCH, insn.imm);
                        } else {
                            self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, insn.imm));
                        }
                        self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, dst, REGISTER_SCRATCH, None));
                    }
                },
                ebpf::DIV64_IMM | ebpf::MOD64_IMM if !sbpf_version.enable_pqr() =>
                    self.emit_product_quotient_remainder(
                        OperandSize::S64,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MOD,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) != ebpf::BPF_MUL,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MUL,
                        dst, dst, Some(insn.imm),
                    ),
                ebpf::ST_1B_IMM  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 1, Some(Value::Constant64(insn.imm, true)));
                },
                ebpf::ST_2B_IMM  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 2, Some(Value::Constant64(insn.imm, true)));
                },
                ebpf::MUL64_REG if !sbpf_version.enable_pqr() => {
                    self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, dst, src, None));
                },
                ebpf::DIV64_REG | ebpf::MOD64_REG if !sbpf_version.enable_pqr() =>
                    self.emit_product_quotient_remainder(
                        OperandSize::S64,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MOD,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) != ebpf::BPF_MUL,
                        (insn.opc & ebpf::BPF_ALU_OP_MASK) == ebpf::BPF_MUL,
                        src, dst, None,
                    ),
                ebpf::ST_1B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 1, Some(Value::Register(src)));
                },
                ebpf::ST_2B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 2, Some(Value::Register(src)));
                },
                ebpf::OR64_IMM   => self.emit_sanitized_alu(OperandSize::S64, 0x09, 1, dst, insn.imm),
                ebpf::OR64_REG   => self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x09, src, dst, None)),
                ebpf::AND64_IMM  => self.emit_sanitized_alu(OperandSize::S64, 0x21, 4, dst, insn.imm),
                ebpf::AND64_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x21, src, dst, None)),
                ebpf::LSH64_IMM  => self.emit_shift(OperandSize::S64, 4, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::LSH64_REG  => self.emit_shift(OperandSize::S64, 4, src, dst, None),
                ebpf::RSH64_IMM  => self.emit_shift(OperandSize::S64, 5, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::RSH64_REG  => self.emit_shift(OperandSize::S64, 5, src, dst, None),
                ebpf::ST_4B_IMM  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 4, Some(Value::Constant64(insn.imm, true)));
                },
                ebpf::NEG64      if !sbpf_version.disable_neg() => self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 3, dst, 0, None)),
                ebpf::ST_4B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 4, Some(Value::Register(src)));
                },
                ebpf::ST_8B_IMM  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 8, Some(Value::Constant64(insn.imm, true)));
                },
                ebpf::ST_8B_REG  if sbpf_version.move_memory_instruction_classes() => {
                    self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, insn.off as i64, true), 8, Some(Value::Register(src)));
                },
                ebpf::XOR64_IMM  => self.emit_sanitized_alu(OperandSize::S64, 0x31, 6, dst, insn.imm),
                ebpf::XOR64_REG  => self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x31, src, dst, None)),
                ebpf::MOV64_IMM  => {
                    if self.should_sanitize_constant(insn.imm) {
                        self.emit_sanitized_load_immediate(dst, insn.imm);
                    } else {
                        self.emit_ins(X86Instruction::load_immediate(dst, insn.imm));
                    }
                }
                ebpf::MOV64_REG  => {
                    if src != dst {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, src, dst));
                    }
                },
                ebpf::ARSH64_IMM => self.emit_shift(OperandSize::S64, 7, REGISTER_SCRATCH, dst, Some(insn.imm)),
                ebpf::ARSH64_REG => self.emit_shift(OperandSize::S64, 7, src, dst, None),
                ebpf::HOR64_IMM if sbpf_version.disable_lddw() => {
                    self.emit_sanitized_alu(OperandSize::S64, 0x09, 1, dst, (insn.imm as u64).wrapping_shl(32) as i64);
                }

                // BPF_PQR class
                ebpf::LMUL32_IMM | ebpf::LMUL64_IMM | ebpf::UHMUL64_IMM | ebpf::SHMUL64_IMM |
                ebpf::UDIV32_IMM | ebpf::UDIV64_IMM | ebpf::UREM32_IMM | ebpf::UREM64_IMM |
                ebpf::SDIV32_IMM | ebpf::SDIV64_IMM | ebpf::SREM32_IMM | ebpf::SREM64_IMM
                if sbpf_version.enable_pqr() => {
                    let signed = insn.opc & (1 << 7) != 0;
                    let mut imm = insn.imm;
                    if !signed {
                        imm &= u32::MAX as i64;
                    }
                    self.emit_product_quotient_remainder(
                        if insn.opc & (1 << 4) != 0 { OperandSize::S64 } else { OperandSize::S32 },
                        insn.opc & (1 << 5) != 0,
                        insn.opc & (1 << 6) != 0,
                        signed,
                        dst, dst, Some(imm),
                    )
                }
                ebpf::LMUL32_REG | ebpf::LMUL64_REG | ebpf::UHMUL64_REG | ebpf::SHMUL64_REG |
                ebpf::UDIV32_REG | ebpf::UDIV64_REG | ebpf::UREM32_REG | ebpf::UREM64_REG |
                ebpf::SDIV32_REG | ebpf::SDIV64_REG | ebpf::SREM32_REG | ebpf::SREM64_REG
                if sbpf_version.enable_pqr() =>
                    self.emit_product_quotient_remainder(
                        if insn.opc & (1 << 4) != 0 { OperandSize::S64 } else { OperandSize::S32 },
                        insn.opc & (1 << 5) != 0,
                        insn.opc & (1 << 6) != 0,
                        insn.opc & (1 << 7) != 0,
                        src, dst, None,
                    ),

                // BPF_JMP32 class
                ebpf::JEQ32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x84, false, insn.imm, dst, target_pc),
                ebpf::JEQ32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x84, false, src, dst, target_pc),
                ebpf::JGT32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x87, false, insn.imm, dst, target_pc),
                ebpf::JGT32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x87, false, src, dst, target_pc),
                ebpf::JGE32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x83, false, insn.imm, dst, target_pc),
                ebpf::JGE32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x83, false, src, dst, target_pc),
                ebpf::JLT32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x82, false, insn.imm, dst, target_pc),
                ebpf::JLT32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x82, false, src, dst, target_pc),
                ebpf::JLE32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x86, false, insn.imm, dst, target_pc),
                ebpf::JLE32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x86, false, src, dst, target_pc),
                ebpf::JSET32_IMM  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x85, true, insn.imm, dst, target_pc),
                ebpf::JSET32_REG  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x85, true, src, dst, target_pc),
                ebpf::JNE32_IMM   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x85, false, insn.imm, dst, target_pc),
                ebpf::JNE32_REG   if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x85, false, src, dst, target_pc),
                ebpf::JSGT32_IMM  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x8f, false, insn.imm, dst, target_pc),
                ebpf::JSGT32_REG  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x8f, false, src, dst, target_pc),
                ebpf::JSGE32_IMM  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x8d, false, insn.imm, dst, target_pc),
                ebpf::JSGE32_REG  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x8d, false, src, dst, target_pc),
                ebpf::JSLT32_IMM  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x8c, false, insn.imm, dst, target_pc),
                ebpf::JSLT32_REG  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x8c, false, src, dst, target_pc),
                ebpf::JSLE32_IMM  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_imm(OperandSize::S32, 0x8e, false, insn.imm, dst, target_pc),
                ebpf::JSLE32_REG  if sbpf_version.enable_jmp32() => self.emit_conditional_branch_reg(OperandSize::S32, 0x8e, false, src, dst, target_pc),

                // BPF_JMP64 class
                ebpf::JA         => {
                    self.emit_validate_and_profile_instruction_count(target_pc);
                    let jump_offset = self.relative_to_target_pc(target_pc, 5);
                    self.emit_ins(X86Instruction::jump_immediate(jump_offset));
                },
                ebpf::JEQ64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x84, false, insn.imm, dst, target_pc),
                ebpf::JEQ64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x84, false, src, dst, target_pc),
                ebpf::JGT64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x87, false, insn.imm, dst, target_pc),
                ebpf::JGT64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x87, false, src, dst, target_pc),
                ebpf::JGE64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x83, false, insn.imm, dst, target_pc),
                ebpf::JGE64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x83, false, src, dst, target_pc),
                ebpf::JLT64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x82, false, insn.imm, dst, target_pc),
                ebpf::JLT64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x82, false, src, dst, target_pc),
                ebpf::JLE64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x86, false, insn.imm, dst, target_pc),
                ebpf::JLE64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x86, false, src, dst, target_pc),
                ebpf::JSET64_IMM   => self.emit_conditional_branch_imm(OperandSize::S64, 0x85, true, insn.imm, dst, target_pc),
                ebpf::JSET64_REG   => self.emit_conditional_branch_reg(OperandSize::S64, 0x85, true, src, dst, target_pc),
                ebpf::JNE64_IMM    => self.emit_conditional_branch_imm(OperandSize::S64, 0x85, false, insn.imm, dst, target_pc),
                ebpf::JNE64_REG    => self.emit_conditional_branch_reg(OperandSize::S64, 0x85, false, src, dst, target_pc),
                ebpf::JSGT64_IMM   => self.emit_conditional_branch_imm(OperandSize::S64, 0x8f, false, insn.imm, dst, target_pc),
                ebpf::JSGT64_REG   => self.emit_conditional_branch_reg(OperandSize::S64, 0x8f, false, src, dst, target_pc),
                ebpf::JSGE64_IMM   => self.emit_conditional_branch_imm(OperandSize::S64, 0x8d, false, insn.imm, dst, target_pc),
                ebpf::JSGE64_REG   => self.emit_conditional_branch_reg(OperandSize::S64, 0x8d, false, src, dst, target_pc),
                ebpf::JSLT64_IMM   => self.emit_conditional_branch_imm(OperandSize::S64, 0x8c, false, insn.imm, dst, target_pc),
                ebpf::JSLT64_REG   => self.emit_conditional_branch_reg(OperandSize::S64, 0x8c, false, src, dst, target_pc),
                ebpf::JSLE64_IMM   => self.emit_conditional_branch_imm(OperandSize::S64, 0x8e, false, insn.imm, dst, target_pc),
                ebpf::JSLE64_REG   => self.emit_conditional_branch_reg(OperandSize::S64, 0x8e, false, src, dst, target_pc),
                ebpf::CALL_IMM     => {
                    let mut resolved = false;
                    // External syscall
                    if !sbpf_version.static_syscalls() || insn.src == 0 {
                        if let Some((_, (_, callback))) = self.executable.get_loader().get_function_registry().lookup_by_key(insn.imm as u32) {
                            callback(&mut self);
                            resolved = true;
                        }
                    }
                    // Internal call
                    if sbpf_version.static_syscalls() {
                        let target_pc = (self.pc as i64).saturating_add(insn.imm).saturating_add(1);
                        if ebpf::is_pc_in_program(self.program, target_pc as usize) && insn.src == 1 {
                            self.emit_internal_call(Value::Constant64(target_pc, true));
                            resolved = true;
                        }
                    } else if let Some((_function_name, target_pc)) =
                        self.executable
                            .get_function_registry()
                            .lookup_by_key(insn.imm as u32) {
                        self.emit_internal_call(Value::Constant64(target_pc as i64, true));
                        resolved = true;
                    }
                    if !resolved {
                        self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.pc as i64));
                        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_CALL_UNSUPPORTED_INSTRUCTION, 5)));
                    }
                },
                ebpf::CALL_REG  => {
                    let target_pc = if sbpf_version.callx_uses_src_reg() {
                        src
                    } else if sbpf_version.callx_uses_dst_reg() {
                        dst
                    } else {
                        REGISTER_MAP[insn.imm as usize]
                    };
                    self.emit_internal_call(Value::Register(target_pc));
                },
                ebpf::EXIT      => {
                    self.last_instruction_meter_validation_pc = self.pc;
                    self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, REGISTER_INSTRUCTION_METER, self.pc as i64, None));
                    self.emit_ins(X86Instruction::conditional_jump_immediate(0x86, self.relative_to_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS, 6)));

                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, self.pc as i64 + 1, None));

                    let call_depth_access = X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::CallDepth));
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_PTR_TO_VM, 1, Some(call_depth_access))); // env.call_depth -= 1;
                    self.cold_stubs.push(ColdStub {
                        jcc_end_offset: self.offset_in_text_section + 6,
                        pc: self.pc,
                        kind: ColdStubKind::RootExit,
                    });
                    self.emit_ins(X86Instruction::conditional_jump_immediate(0x82, 0));

                    self.emit_ins(X86Instruction::return_near());
                },

                _               => return Err(EbpfError::UnsupportedInstruction),
            }

            self.pc += 1;
        }

        // Bumper in case there was no final exit
        if self.offset_in_text_section + self.cold_stubs.len() * 32 + self.forward_branch_stubs.len() * 64 + MAX_MACHINE_CODE_LENGTH_PER_INSTRUCTION * 2 >= self.text_len {
            return Err(EbpfError::ExhaustedTextSegment(self.pc));
        }
        self.emit_validate_and_profile_instruction_count(self.pc + 1);
        self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.pc as i64)); // Save pc
        self.emit_set_exception_kind(EbpfError::ExecutionOverrun);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        let cold_stubs = std::mem::take(&mut self.cold_stubs);
        let call_depth_access = X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::CallDepth));
        for stub in cold_stubs {
            if self.offset_in_text_section + 32 >= self.text_len {
                return Err(EbpfError::ExhaustedTextSegment(stub.pc));
            }
            let stub_offset = self.offset_in_text_section;
            let rel32 = (stub_offset as isize - stub.jcc_end_offset as isize) as i32;
            unsafe {
                let patch_ptr = self.result.text_section_mut().as_mut_ptr().add(stub.jcc_end_offset - 4).cast::<i32>();
                ptr::write_unaligned(patch_ptr, rel32);
            }
            match stub.kind {
                ColdStubKind::CallDepthExceeded => {
                    self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, stub.pc as i64));
                    self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_CALL_DEPTH_EXCEEDED, 5)));
                }
                ColdStubKind::RootExit => {
                    self.emit_ins(X86Instruction::store_immediate(OperandSize::S64, REGISTER_PTR_TO_VM, call_depth_access, 0));
                    self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, stub.pc as i64));
                    self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_EXIT, 5)));
                }
            }
        }

        let forward_branch_stubs = mem::take(&mut self.forward_branch_stubs);
        for stub in forward_branch_stubs {
            if self.offset_in_text_section + 64 >= self.text_len {
                return Err(EbpfError::ExhaustedTextSegment(stub.pc as usize));
            }
            let stub_offset = self.offset_in_text_section;
            let jcc_end = stub.jcc_imm_offset as usize + 4;
            let rel = (stub_offset as isize - jcc_end as isize) as i32;
            unsafe {
                let imm_ptr = self.result.text_section_mut().as_mut_ptr().add(stub.jcc_imm_offset as usize).cast::<i32>();
                ptr::write_unaligned(imm_ptr, rel);
            }
            self.emit_validate_instruction_count(Some(stub.pc as usize));
            self.emit_add_instruction_meter(stub.target_pc as i64 - stub.pc as i64 - 1);
            let jump_offset = self.relative_to_target_pc(stub.target_pc as usize, 5);
            self.emit_ins(X86Instruction::jump_immediate(jump_offset));
        }

        self.resolve_jumps();
        debug_assert!(self.memory_pc_entries.windows(2).all(|w| w[0].0 < w[1].0));
        self.result
            .set_memory_pc_entries(self.memory_pc_entries.into_boxed_slice());
        self.result.seal(self.offset_in_text_section)
    }

    /// Some user provided constants can skip sanitization, returns `false` for those
    #[inline]
    pub fn should_sanitize_constant(&self, value: i64) -> bool {
        if !self.config.sanitize_user_provided_values {
            return false;
        }

        match value as u64 {
            0xFFFF
            | 0xFFFFFF
            | 0xFFFFFFFF
            | 0xFFFFFFFFFF
            | 0xFFFFFFFFFFFF
            | 0xFFFFFFFFFFFFFF
            | 0xFFFFFFFFFFFFFFFF => false,
            v if v <= 0xFF => false,
            v if !v <= 0xFF => false,
            _ => true
        }
    }

    /// Decrypts and offsets the vm structure for `X86IndirectAccess::Offset()`
    #[inline]
    pub fn slot_in_vm(&self, slot: RuntimeEnvironmentSlot) -> i32 {
        (slot as i32) - self.runtime_environment_key
    }

    /// Emits a fixed length machinecode sequence into the text section
    #[inline(always)]
    pub fn emit<T>(&mut self, data: T) {
        debug_assert!(self.offset_in_text_section + mem::size_of::<T>() <= self.text_len);
        unsafe {
            let ptr = self.text_ptr.add(self.offset_in_text_section);
            #[allow(clippy::cast_ptr_alignment)]
            ptr::write_unaligned(ptr.cast::<T>(), data);
        }
        self.offset_in_text_section += mem::size_of::<T>();
    }

    /// The next 16 writable bytes of the text section, if that many remain.
    ///
    /// Instructions are encoded in place so that no stack buffer has to be reloaded.
    #[inline(always)]
    pub(crate) fn encode_slot(&mut self) -> Option<*mut u8> {
        if self.offset_in_text_section + 16 <= self.text_len {
            Some(unsafe { self.text_ptr.add(self.offset_in_text_section) })
        } else {
            None
        }
    }

    /// Commits `len` bytes encoded at [`Self::encode_slot`].
    #[inline(always)]
    pub(crate) fn advance_encoded(&mut self, len: usize) {
        self.offset_in_text_section += len;
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn emit_encoded_tail(&mut self, buf: &[u8; 16], len: usize) {
        assert!(self.offset_in_text_section + len <= self.text_len);
        unsafe {
            ptr::copy_nonoverlapping(
                buf.as_ptr(),
                self.text_ptr.add(self.offset_in_text_section),
                len,
            );
        }
        self.offset_in_text_section += len;
    }

    /// Emits a variable length machinecode sequence into the text section
    #[inline]
    pub fn emit_variable_length(&mut self, size: OperandSize, data: u64) {
        match size {
            OperandSize::S0 => {},
            OperandSize::S8 => self.emit::<u8>(data as u8),
            OperandSize::S16 => self.emit::<u16>(data as u16),
            OperandSize::S32 => self.emit::<u32>(data as u32),
            OperandSize::S64 => self.emit::<u64>(data),
        }
    }

    /// Emits a [X86Instruction] into the text section
    // This function helps the optimizer to inline the machinecode emission while avoiding stack allocations
    #[inline(always)]
    pub fn emit_ins(&mut self, instruction: X86Instruction) {
        instruction.emit(self);
        if self.next_noop_insertion > 0 {
            self.next_noop_insertion -= 1;
        } else {
            self.insert_noop();
        }
    }

    #[cold]
    #[inline(never)]
    fn insert_noop(&mut self) {
        if let Some(noop_range) = &self.noop_range {
            self.next_noop_insertion = noop_range.sample(&mut self.diversification_rng);
            // X86Instruction::noop().emit(self)?;
            self.emit::<u8>(0x90);
        } else {
            self.next_noop_insertion = u32::MAX;
        }
    }

    /// Loads a user provided constant into the destination with sanitization
    #[inline]
    pub fn emit_sanitized_load_immediate(&mut self, destination: X86Register, value: i64) {
        let lower_key = self.immediate_value_key as i32 as i64;
        if value >= i32::MIN as i64 && value <= i32::MAX as i64 {
            self.emit_ins(X86Instruction::load_immediate(destination, value.wrapping_sub(lower_key)));
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, destination, lower_key, None)); // wrapping_add(lower_key)
        } else if value as u64 & u32::MAX as u64 == 0 {
            self.emit_ins(X86Instruction::load_immediate(destination, value.rotate_right(32).wrapping_sub(lower_key)));
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, destination, lower_key, None)); // wrapping_add(lower_key)
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 4, destination, 32, None)); // shift_left(32)
        } else if destination != REGISTER_SCRATCH {
            self.emit_ins(X86Instruction::load_immediate(destination, value.wrapping_sub(self.immediate_value_key)));
            self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.immediate_value_key));
            self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_SCRATCH, destination, None)); // wrapping_add(immediate_value_key)
        } else {
            let upper_key = (self.immediate_value_key >> 32) as i32 as i64;
            self.emit_ins(X86Instruction::load_immediate(destination, value.wrapping_sub(lower_key).rotate_right(32).wrapping_sub(upper_key)));
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, destination, upper_key, None)); // wrapping_add(upper_key)
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 1, destination, 32, None)); // rotate_right(32)
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, destination, lower_key, None)); // wrapping_add(lower_key)
        }
    }

    /// Performs arithmetic on a user provided immediate value with sanitization
    #[inline]
    pub fn emit_sanitized_alu(&mut self, size: OperandSize, opcode: u8, opcode_extension: u8, destination: X86Register, immediate: i64) {
        if self.should_sanitize_constant(immediate) {
            self.emit_sanitized_load_immediate(REGISTER_SCRATCH, immediate);
            self.emit_ins(X86Instruction::alu(size, opcode, REGISTER_SCRATCH, destination, None));
        } else if immediate >= i32::MIN as i64 && immediate <= i32::MAX as i64 {
            self.emit_ins(X86Instruction::alu_immediate(size, 0x81, opcode_extension, destination, immediate, None));
        } else {
            self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, immediate));
            self.emit_ins(X86Instruction::alu(size, opcode, REGISTER_SCRATCH, destination, None));
        }
    }

    #[allow(dead_code)]
    fn emit_stopwatch(&mut self, begin: bool) {
        self.stopwatch_is_active = true;
        self.emit_ins(X86Instruction::push(RDX, None));
        self.emit_ins(X86Instruction::push(RAX, None));
        self.emit_ins(X86Instruction::fence(FenceType::Load)); // lfence
        self.emit_ins(X86Instruction::cycle_count()); // rdtsc
        self.emit_ins(X86Instruction::fence(FenceType::Load)); // lfence
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 4, RDX, 32, None)); // RDX <<= 32;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x09, RDX, RAX, None)); // RAX |= RDX;
        if begin {
            self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RAX, REGISTER_PTR_TO_VM, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::StopwatchNumerator))))); // *numerator -= RAX;
        } else {
            self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, RAX, REGISTER_PTR_TO_VM, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::StopwatchNumerator))))); // *numerator += RAX;
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_PTR_TO_VM, 1, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::StopwatchDenominator))))); // *denominator += 1;
        }
        self.emit_ins(X86Instruction::pop(RAX));
        self.emit_ins(X86Instruction::pop(RDX));
    }

    /// Emits a instruction meter check (without any updates thereof)
    #[inline]
    pub fn emit_validate_instruction_count(&mut self, pc: Option<usize>) {
        // Update `MACHINE_CODE_PER_INSTRUCTION_METER_CHECKPOINT` if you change the code generation here
        if let Some(pc) = pc {
            self.last_instruction_meter_validation_pc = pc;
            if pc <= i32::MAX as usize {
                self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, REGISTER_INSTRUCTION_METER, pc as i64, None));
                self.emit_ins(X86Instruction::conditional_jump_immediate(0x86, self.relative_to_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS, 6)));
                return;
            }
            self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, pc as i64));
        }
        // If instruction_meter <= pc, throw ExceededMaxInstructions
        self.emit_ins(X86Instruction::cmp(OperandSize::S64, REGISTER_SCRATCH, REGISTER_INSTRUCTION_METER, None));
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x86, self.relative_to_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS, 6)));
    }

    #[inline]
    fn emit_add_instruction_meter(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        if delta > 0 {
            if i32::try_from(delta).is_ok() {
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, delta, None));
            } else {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, delta));
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_SCRATCH, REGISTER_INSTRUCTION_METER, None));
            }
        } else {
            let neg_delta = delta.wrapping_neg();
            if neg_delta > 0 && i32::try_from(neg_delta).is_ok() {
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, neg_delta, None));
            } else if i32::try_from(delta).is_ok() {
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, delta, None));
            } else {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, delta));
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_SCRATCH, REGISTER_INSTRUCTION_METER, None));
            }
        }
    }

    /// Charges the instruction meter (without any check thereof)
    #[inline]
    pub fn emit_profile_instruction_count(&mut self, target_pc: usize) {
        self.emit_add_instruction_meter(target_pc as i64 - self.pc as i64 - 1); // instruction_meter += target_pc - (self.pc + 1);
    }

    /// Refunds the instruction meter (without any check thereof)
    #[inline]
    pub fn emit_undo_profile_instruction_count(&mut self, target_pc: usize) {
        self.emit_add_instruction_meter(self.pc as i64 + 1 - target_pc as i64); // instruction_meter += (self.pc + 1) - target_pc;
    }

    /// Checks and charges the instruction meter
    #[inline]
    pub fn emit_validate_and_profile_instruction_count(&mut self, target_pc: usize) {
        self.emit_validate_instruction_count(Some(self.pc));
        self.emit_profile_instruction_count(target_pc);
    }

    /// Hands over the instruction meter to the VM
    ///
    /// with self-pc in REGISTER_SCRATCH.
    pub fn emit_instruction_meter_to_vm(&mut self, pc: Value) {
        // The code below does: self.emit_profile_instruction_count(0);
        // Which comes down to: instruction_meter -= self.pc + 1;
        match pc {
            Value::Register(reg) => {
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x2b, REGISTER_INSTRUCTION_METER, reg, None));
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, 1, None));
            },
            Value::RegisterIndirect(reg, offset, _user_provided) => {
                let indirect_access = if reg == RSP {
                    X86IndirectAccess::OffsetIndexShift(offset, RSP, 0)
                } else {
                    X86IndirectAccess::Offset(offset)
                };
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x2b, REGISTER_INSTRUCTION_METER, reg, Some(indirect_access)));
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, 1, None));
            },
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            }
        }
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x2b, REGISTER_INSTRUCTION_METER, REGISTER_PTR_TO_VM, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::PreviousInstructionMeter))))); // instruction_meter -= *PreviousInstructionMeter;
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 3, REGISTER_INSTRUCTION_METER, 0, None)); // REGISTER_INSTRUCTION_METER = -REGISTER_INSTRUCTION_METER;
        self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_INSTRUCTION_METER, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::DueInsnCount)))); // *DueInsnCount = REGISTER_INSTRUCTION_METER;
    }

    /// Takes back the instruction meter from the VM
    pub fn emit_instruction_meter_from_vm(&mut self, pc: Value) {
        self.emit_ins(X86Instruction::load(OperandSize::S64, REGISTER_PTR_TO_VM, REGISTER_INSTRUCTION_METER, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::PreviousInstructionMeter)))); // REGISTER_INSTRUCTION_METER = *PreviousInstructionMeter;
        // The code below does: self.emit_undo_profile_instruction_count(0);
        // Which comes down to: instruction_meter += self.pc + 1;
        match pc {
            Value::Register(reg) => {
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, reg, REGISTER_INSTRUCTION_METER, None));
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, 1, None));
            },
            Value::RegisterIndirect(reg, offset, _user_provided) => {
                let indirect_access = if reg == RSP {
                    X86IndirectAccess::OffsetIndexShift(offset, RSP, 0)
                } else {
                    X86IndirectAccess::Offset(offset)
                };
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x03, REGISTER_INSTRUCTION_METER, reg, Some(indirect_access)));
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, 1, None));
            },
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            }
        }
    }

    /// Calls an external Rust function
    ///
    /// For calls that return values in registers, the returned value is moved into `result_regs`.
    pub fn emit_rust_call(&mut self, target: Value, arguments: &[Argument], result_regs: &[X86Register]) {
        debug_assert!(result_regs.len() <= 2);

        // Save caller-saved registers that are not overwritten by return values
        let mut num_saved = 0usize;
        for &reg in CALLER_SAVED_REGISTERS.iter() {
            if !result_regs.contains(&reg) {
                self.emit_ins(X86Instruction::push(reg, None));
                num_saved += 1;
            }
        }

        let stack_arguments = arguments.len().saturating_sub(ARGUMENT_REGISTERS.len()) as i64;
        let need_align_pad = (num_saved + stack_arguments as usize).is_multiple_of(2);
        if need_align_pad {
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, RSP, 8, None));
        }

        // Pass arguments
        for argument in arguments {
            let is_stack_argument = argument.index >= ARGUMENT_REGISTERS.len();
            let dst = if is_stack_argument {
                RSP // Never used
            } else {
                ARGUMENT_REGISTERS[argument.index]
            };
            match argument.value {
                Value::Register(reg) => {
                    if is_stack_argument {
                        self.emit_ins(X86Instruction::push(reg, None));
                    } else if reg != dst {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, reg, dst));
                    }
                },
                Value::RegisterIndirect(reg, offset, user_provided) => {
                    debug_assert!(!user_provided);
                    if is_stack_argument {
                        debug_assert!(reg != RSP);
                        self.emit_ins(X86Instruction::push(reg, Some(X86IndirectAccess::Offset(offset))));
                    } else if reg == RSP {
                        self.emit_ins(X86Instruction::load(OperandSize::S64, RSP, dst, X86IndirectAccess::OffsetIndexShift(offset, RSP, 0)));
                    } else {
                        self.emit_ins(X86Instruction::load(OperandSize::S64, reg, dst, X86IndirectAccess::Offset(offset)));
                    }
                },
                Value::RegisterPlusConstant32(reg, offset, user_provided) => {
                    debug_assert!(!user_provided);
                    if is_stack_argument {
                        self.emit_ins(X86Instruction::push(reg, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, offset as i64, Some(X86IndirectAccess::OffsetIndexShift(0, RSP, 0))));
                    } else if reg == RSP {
                        self.emit_ins(X86Instruction::lea(OperandSize::S64, RSP, dst, Some(X86IndirectAccess::OffsetIndexShift(offset, RSP, 0))));
                    } else {
                        self.emit_ins(X86Instruction::lea(OperandSize::S64, reg, dst, Some(X86IndirectAccess::Offset(offset))));
                    }
                },
                Value::RegisterPlusConstant64(reg, offset, user_provided) => {
                    debug_assert!(!user_provided);
                    if is_stack_argument {
                        self.emit_ins(X86Instruction::push(reg, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, offset, Some(X86IndirectAccess::OffsetIndexShift(0, RSP, 0))));
                    } else {
                        self.emit_ins(X86Instruction::load_immediate(dst, offset));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, reg, dst, None));
                    }
                },
                Value::Constant64(value, user_provided) => {
                    debug_assert!(!user_provided && !is_stack_argument);
                    self.emit_ins(X86Instruction::load_immediate(dst, value));
                },
            }
        }

        match target {
            Value::Register(reg) => {
                self.emit_ins(X86Instruction::call_reg(reg, None));
            },
            Value::RegisterIndirect(reg, offset, user_provided) => {
                debug_assert!(!user_provided);
                if reg == RSP {
                    self.emit_ins(X86Instruction::call_reg(RSP, Some(X86IndirectAccess::OffsetIndexShift(offset, RSP, 0))));
                } else {
                    self.emit_ins(X86Instruction::call_reg(reg, Some(X86IndirectAccess::Offset(offset))));
                }
            },
            Value::Constant64(value, user_provided) => {
                debug_assert!(!user_provided);
                self.emit_ins(X86Instruction::load_immediate(RAX, value));
                self.emit_ins(X86Instruction::call_reg(RAX, None));
            },
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            }
        }

        // Clean up stack arguments and alignment padding
        let stack_cleanup_words = stack_arguments + i64::from(need_align_pad);
        if stack_cleanup_words != 0 {
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, stack_cleanup_words * 8, None));
        }

        // Move return values directly into result_regs before restoring saved registers
        match result_regs {
            [] => {}
            [dst0] => {
                if *dst0 != RAX {
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, RAX, *dst0));
                }
            }
            [dst0, dst1] => {
                debug_assert!(*dst0 != *dst1);
                if *dst0 == RDX && *dst1 == RAX {
                    self.emit_ins(X86Instruction::xchg(OperandSize::S64, RAX, RDX, None));
                } else if *dst0 == RDX {
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, *dst1));
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, RAX, *dst0));
                } else {
                    if *dst0 != RAX {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, RAX, *dst0));
                    }
                    if *dst1 != RDX {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, *dst1));
                    }
                }
            }
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            }
        }

        for &reg in CALLER_SAVED_REGISTERS.iter().rev() {
            if !result_regs.contains(&reg) {
                self.emit_ins(X86Instruction::pop(reg));
            }
        }
    }

    /// Calls an internal function within the same program
    pub fn emit_internal_call(&mut self, dst: Value) {
        self.last_instruction_meter_validation_pc = self.pc;
        match dst {
            Value::Register(reg) => {
                for bit in 0..SCRATCH_REGS {
                    self.emit_ins(X86Instruction::push(REGISTER_MAP[FIRST_SCRATCH_REG + bit], None));
                }
                self.emit_ins(X86Instruction::push(REGISTER_MAP[FRAME_PTR_REG], None));
                // Store guest current pc at [RSP - 24] for meter profiling and error reporting
                self.emit_ins(X86Instruction::store_immediate(OperandSize::S64, RSP, X86IndirectAccess::OffsetIndexShift(-24, RSP, 0), self.pc as i64));
                // Move guest_target_address into REGISTER_SCRATCH
                self.emit_ins(X86Instruction::mov(OperandSize::S64, reg, REGISTER_SCRATCH));
                self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_INTERNAL_FUNCTION_CALL_REG, 5)));
                self.emit_ins(X86Instruction::call_reg(REGISTER_SCRATCH, None));

                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, self.pc as i64 + 1, None));
                self.emit_ins(X86Instruction::pop(REGISTER_MAP[FRAME_PTR_REG]));
                for bit in (0..SCRATCH_REGS).rev() {
                    self.emit_ins(X86Instruction::pop(REGISTER_MAP[FIRST_SCRATCH_REG + bit]));
                }
            },
            Value::Constant64(target_pc, user_provided) => {
                debug_assert!(user_provided);
                let target_pc_usize = target_pc as usize;
                let manual_bump = self.executable.get_sbpf_version().manual_stack_frame_bump();
                let is_unsupported_target = !ebpf::is_pc_in_program(self.program, target_pc_usize)
                    || self.is_lddw_second_slot.get(target_pc_usize).copied().unwrap_or(false)
                    || ebpf::get_insn_unchecked(self.program, target_pc_usize).opc == 0;
                let info = if is_unsupported_target {
                    InternalCallInfo::conservative(manual_bump)
                } else {
                    match self.call_target_index.get(target_pc_usize) {
                        Some(&idx) if idx != u32::MAX => self.internal_call_info[idx as usize],
                        _ => InternalCallInfo::conservative(manual_bump),
                    }
                };

                self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, REGISTER_INSTRUCTION_METER, self.pc as i64, None));
                self.emit_ins(X86Instruction::conditional_jump_immediate(0x86, self.relative_to_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS, 6)));

                let call_depth_access = X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::CallDepth));
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_PTR_TO_VM, 1, Some(call_depth_access)));
                self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S32, REGISTER_PTR_TO_VM, self.config.max_call_depth as i64, Some(call_depth_access)));
                self.cold_stubs.push(ColdStub {
                    jcc_end_offset: self.offset_in_text_section + 6,
                    pc: self.pc,
                    kind: ColdStubKind::CallDepthExceeded,
                });
                self.emit_ins(X86Instruction::conditional_jump_immediate(0x83, 0));

                for bit in 0..SCRATCH_REGS {
                    if info.clobber_mask & (1 << bit) != 0 {
                        self.emit_ins(X86Instruction::push(REGISTER_MAP[FIRST_SCRATCH_REG + bit], None));
                    }
                }
                if info.save_frame_ptr {
                    self.emit_ins(X86Instruction::push(REGISTER_MAP[FRAME_PTR_REG], None));
                    if info.bump_frame_ptr {
                        let num_frames = if self.executable.get_sbpf_version().stack_frame_gaps()
                            && self.config.enable_stack_frame_gaps {
                            2
                        } else {
                            1
                        };
                        let stack_frame_size = self.config.stack_frame_size as i64 * num_frames;
                        if stack_frame_size != 0 {
                            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_MAP[FRAME_PTR_REG], stack_frame_size, None));
                        }
                    }
                }

                let delta = target_pc - self.pc as i64 - 1;
                if delta > 0 {
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, delta, None));
                } else if delta < 0 {
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, -delta, None));
                }

                if is_unsupported_target {
                    self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, target_pc));
                }

                let jump_offset = self.relative_to_target_pc(target_pc_usize, 5);
                self.emit_ins(X86Instruction::call_immediate(jump_offset));

                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, self.pc as i64 + 1, None));
                if info.save_frame_ptr {
                    self.emit_ins(X86Instruction::pop(REGISTER_MAP[FRAME_PTR_REG]));
                }
                for bit in (0..SCRATCH_REGS).rev() {
                    if info.clobber_mask & (1 << bit) != 0 {
                        self.emit_ins(X86Instruction::pop(REGISTER_MAP[FIRST_SCRATCH_REG + bit]));
                    }
                }
            },
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            }
        }
    }

    /// Calls an external function from the syscall registry
    pub fn emit_external_call(&mut self, function: BuiltinFunction<C>) {
        self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, function as usize as i64));
        self.emit_ins(X86Instruction::push(REGISTER_SCRATCH, None));
        self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.pc as i64)); // Save pc
        self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_EXTERNAL_FUNCTION_CALL, 5)));
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, 8, None)); // RSP += 8;
    }

    #[inline(always)]
    fn can_inline_v3_stack(&self, base_reg_idx: u8, off: i16, len: i64) -> bool {
        if !self.inline_v3_stack || base_reg_idx != FRAME_PTR_REG as u8 {
            return false;
        }
        let off = off as i64;
        let frame_size = self.config.stack_frame_size as i64;
        -frame_size <= off && off + len <= 0
    }

    #[inline(always)]
    fn emit_load(&mut self, op_size: OperandSize, len: i64, src_idx: u8, dst: X86Register, src: X86Register, off: i16) {
        if self.can_inline_v3_stack(src_idx, off, len) {
            self.emit_ins(X86Instruction::load(
                op_size,
                R15,
                dst,
                X86IndirectAccess::OffsetIndexShift(off as i32, RBP, 0),
            ));
        } else {
            self.emit_address_translation(Some(dst), Value::RegisterPlusConstant64(src, off as i64, true), len as u64, None);
        }
    }

    #[inline(always)]
    fn emit_store_reg(&mut self, op_size: OperandSize, len: i64, dst_idx: u8, dst: X86Register, src: X86Register, off: i16) {
        if self.can_inline_v3_stack(dst_idx, off, len) {
            self.emit_ins(X86Instruction::store(
                op_size,
                src,
                R15,
                X86IndirectAccess::OffsetIndexShift(off as i32, RBP, 0),
            ));
        } else {
            self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, off as i64, true), len as u64, Some(Value::Register(src)));
        }
    }

    #[inline(always)]
    fn emit_store_imm(&mut self, op_size: OperandSize, len: i64, dst_idx: u8, dst: X86Register, off: i16, raw_imm: i64) {
        if self.can_inline_v3_stack(dst_idx, off, len) {
            let imm = match op_size {
                OperandSize::S8 => raw_imm as i8 as i64,
                OperandSize::S16 => raw_imm as i16 as i64,
                OperandSize::S32 => raw_imm as i32 as i64,
                OperandSize::S64 => raw_imm as i32 as i64,
                OperandSize::S0 => unreachable!(),
            };
            let access = X86IndirectAccess::OffsetIndexShift(off as i32, RBP, 0);
            if self.should_sanitize_constant(imm) {
                self.emit_sanitized_load_immediate(REGISTER_SCRATCH, imm);
                self.emit_ins(X86Instruction::store(op_size, REGISTER_SCRATCH, R15, access));
            } else {
                self.emit_ins(X86Instruction::store_immediate(op_size, R15, access, imm));
            }
        } else {
            self.emit_address_translation(None, Value::RegisterPlusConstant64(dst, off as i64, true), len as u64, Some(Value::Constant64(raw_imm, true)));
        }
    }

    /// Helper for load and store instructions
    pub fn emit_address_translation(&mut self, dst: Option<X86Register>, vm_addr: Value, len: u64, value: Option<Value>) {
        debug_assert_ne!(dst.is_some(), value.is_some());
        let value_stack_slot = X86IndirectAccess::OffsetIndexShift(-32, RSP, 0);

        let anchor_base = match value {
            Some(Value::Register(reg)) => {
                self.emit_ins(X86Instruction::store(OperandSize::S64, reg, RSP, value_stack_slot));
                4
            }
            Some(Value::Constant64(constant, user_provided)) => {
                debug_assert!(user_provided);
                if !(user_provided && self.should_sanitize_constant(constant))
                    && (i32::MIN as i64..=i32::MAX as i64).contains(&constant)
                {
                    self.emit_ins(X86Instruction::store_immediate(OperandSize::S64, RSP, value_stack_slot, constant));
                    4
                } else {
                    // First half of emit_sanitized_load_immediate(stack_slot_of_value_to_store, constant)
                    let lower_key = self.immediate_value_key as i32 as i64;
                    self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, constant.wrapping_sub(lower_key)));
                    self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_SCRATCH, RSP, value_stack_slot));
                    8
                }
            }
            _ => 0,
        };

        match vm_addr {
            Value::RegisterPlusConstant64(reg, constant, user_provided) => {
                if constant == 0 {
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, reg, REGISTER_SCRATCH));
                } else if !(user_provided && self.should_sanitize_constant(constant))
                    && (i32::MIN as i64..=i32::MAX as i64).contains(&constant)
                {
                    self.emit_ins(X86Instruction::lea(
                        OperandSize::S64,
                        reg,
                        REGISTER_SCRATCH,
                        Some(X86IndirectAccess::offset(reg, constant as i32)),
                    ));
                } else {
                    if user_provided && self.should_sanitize_constant(constant) {
                        self.emit_sanitized_load_immediate(REGISTER_SCRATCH, constant);
                    } else {
                        self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, constant));
                    }
                    self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, reg, REGISTER_SCRATCH, None));
                }
            },
            _ => {
                #[cfg(debug_assertions)]
                unreachable!();
            },
        }

        let anchor = ANCHOR_TRANSLATE_MEMORY_ADDRESS + anchor_base + len.trailing_zeros() as usize;
        // Record return RIP offset BEFORE emit_ins so diversification NOPs appended after `call` do not shift it.
        let call_return_offset = (self.offset_in_text_section + 5) as u32;
        self.memory_pc_entries.push((call_return_offset, self.pc as u32));
        self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(anchor, 5)));
        if let Some(dst) = dst {
            self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, dst));
        }
    }

    /// Helper for conditional branch instructions without an immediate value
    #[inline]
    pub fn emit_conditional_branch_reg(&mut self, size: OperandSize, op: u8, bitwise: bool, first_operand: X86Register, second_operand: X86Register, target_pc: usize) {
        if target_pc == self.pc + 1 {
            if cfg!(feature = "tracer") {
                self.emit_validate_instruction_count(Some(self.pc));
            }
            return;
        }
        if target_pc <= self.pc || cfg!(feature = "tracer") {
            self.emit_validate_instruction_count(Some(self.pc));
        }
        if bitwise { // Logical
            self.emit_ins(X86Instruction::test(size, first_operand, second_operand, None));
        } else { // Arithmetic
            self.emit_ins(X86Instruction::cmp(size, first_operand, second_operand, None));
        }
        self.emit_conditional_branch_transfer(op, target_pc);
    }

    /// Helper for conditional branch instructions with an immediate value
    #[inline]
    pub fn emit_conditional_branch_imm(&mut self, size: OperandSize, op: u8, bitwise: bool, immediate: i64, second_operand: X86Register, target_pc: usize) {
        if target_pc == self.pc + 1 {
            if cfg!(feature = "tracer") {
                self.emit_validate_instruction_count(Some(self.pc));
            }
            return;
        }
        if target_pc <= self.pc || cfg!(feature = "tracer") {
            self.emit_validate_instruction_count(Some(self.pc));
        }
        if self.should_sanitize_constant(immediate) {
            self.emit_sanitized_load_immediate(REGISTER_SCRATCH, immediate);
            if bitwise { // Logical
                self.emit_ins(X86Instruction::test(size, REGISTER_SCRATCH, second_operand, None));
            } else { // Arithmetic
                self.emit_ins(X86Instruction::cmp(size, REGISTER_SCRATCH, second_operand, None));
            }
        } else if bitwise { // Logical
            self.emit_ins(X86Instruction::test_immediate(size, second_operand, immediate, None));
        } else { // Arithmetic
            self.emit_ins(X86Instruction::cmp_immediate(size, second_operand, immediate, None));
        }
        self.emit_conditional_branch_transfer(op, target_pc);
    }

    #[inline]
    fn emit_conditional_branch_transfer(&mut self, op: u8, target_pc: usize) {
        if target_pc > self.pc + 1 {
            let jcc_imm_offset = (self.offset_in_text_section + 2) as u32;
            self.emit_ins(X86Instruction::conditional_jump_immediate(op, 0));
            self.forward_branch_stubs.push(ForwardBranchStub {
                jcc_imm_offset,
                pc: self.pc as u32,
                target_pc: target_pc as u32,
            });
        } else {
            debug_assert!(target_pc <= self.pc);
            let rel8_offset = self.offset_in_text_section + 1;
            let short_jcc_end = self.offset_in_text_section + 2;
            self.emit_ins(X86Instruction::conditional_jump_short(0x70 | ((op & 0x0f) ^ 1), 0));
            self.emit_profile_instruction_count(target_pc);
            let jump_offset = self.relative_to_target_pc(target_pc, 5);
            self.emit_ins(X86Instruction::jump_immediate(jump_offset));
            let rel8 = i8::try_from(self.offset_in_text_section - short_jcc_end)
                .expect("backward branch fallthrough offset fits in i8");
            self.result.text_section_mut()[rel8_offset] = rel8 as u8;
        }
    }

    /// Helper for bit shift instructions
    pub fn emit_shift(&mut self, size: OperandSize, opcode_extension: u8, source: X86Register, destination: X86Register, immediate: Option<i64>) {
        if let Some(immediate) = immediate {
            self.emit_ins(X86Instruction::alu_immediate(size, 0xc1, opcode_extension, destination, immediate, None));
            return;
        }
        if self.has_bmi2 {
            self.emit_ins(X86Instruction::bmi2_shift(size, opcode_extension, destination, destination, source));
            return;
        }
        if let OperandSize::S32 = size {
            self.emit_ins(X86Instruction::mov(OperandSize::S32, destination, destination)); // Truncate to 32 bit
        }
        if source == RCX {
            self.emit_ins(X86Instruction::alu_immediate(size, 0xd3, opcode_extension, destination, 0, None));
        } else if destination == RCX {
            self.emit_ins(X86Instruction::push(source, None));
            self.emit_ins(X86Instruction::xchg(OperandSize::S64, source, RCX, None));
            self.emit_ins(X86Instruction::alu_immediate(size, 0xd3, opcode_extension, source, 0, None));
            self.emit_ins(X86Instruction::mov(OperandSize::S64, source, RCX));
            self.emit_ins(X86Instruction::pop(source));
        } else {
            self.emit_ins(X86Instruction::push(RCX, None));
            self.emit_ins(X86Instruction::mov(OperandSize::S64, source, RCX));
            self.emit_ins(X86Instruction::alu_immediate(size, 0xd3, opcode_extension, destination, 0, None));
            self.emit_ins(X86Instruction::pop(RCX));
        }
    }

    fn emit_load_constant(&mut self, destination: X86Register, value: i64) {
        if self.should_sanitize_constant(value) {
            self.emit_sanitized_load_immediate(destination, value);
        } else {
            self.emit_ins(X86Instruction::load_immediate(destination, value));
        }
    }

    fn emit_unsigned_div_rem_imm(&mut self, size: OperandSize, alt_dst: bool, dst: X86Register, imm: i64) -> bool {
        match size {
            OperandSize::S32 => {
                let d = imm as u32;
                if d == 0 {
                    return false;
                }
                if d == 1 {
                    if alt_dst {
                        self.emit_ins(X86Instruction::load_immediate(dst, 0));
                    } else {
                        self.emit_ins(X86Instruction::mov(OperandSize::S32, dst, dst));
                    }
                    return true;
                }
                if d.is_power_of_two() {
                    let k = d.trailing_zeros();
                    if alt_dst {
                        self.emit_sanitized_alu(OperandSize::S32, 0x21, 4, dst, (d - 1) as i64);
                    } else {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0xc1, 5, dst, k as i64, None));
                    }
                    return true;
                }
                let magic = compute_unsigned_div32_magic(d);
                if !alt_dst {
                    if !magic.add_indicator && !self.should_sanitize_constant(magic.mul as i64) && i32::try_from(magic.mul as i64).is_ok() {
                        self.emit_ins(X86Instruction::mov(OperandSize::S32, dst, dst));
                        self.emit_ins(X86Instruction::imul_immediate(OperandSize::S64, dst, dst, magic.mul as i64));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, dst, magic.shift as i64, None));
                    } else if !magic.add_indicator {
                        self.emit_load_constant(REGISTER_SCRATCH, magic.mul as i64);
                        self.emit_ins(X86Instruction::mov(OperandSize::S32, dst, dst));
                        self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, dst, REGISTER_SCRATCH, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, dst, magic.shift as i64, None));
                    } else {
                        self.emit_load_constant(REGISTER_SCRATCH, magic.mul as i64);
                        self.emit_ins(X86Instruction::mov(OperandSize::S32, dst, dst));
                        self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, REGISTER_SCRATCH, dst, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, REGISTER_SCRATCH, 32, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_SCRATCH, dst, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, dst, magic.shift as i64, None));
                    }
                } else {
                    self.emit_load_constant(REGISTER_SCRATCH, magic.mul as i64);
                    self.emit_ins(X86Instruction::mov(OperandSize::S32, dst, dst));
                    self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, REGISTER_SCRATCH, dst, None));
                    if !magic.add_indicator {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, REGISTER_SCRATCH, magic.shift as i64, None));
                    } else {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, REGISTER_SCRATCH, 32, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, dst, REGISTER_SCRATCH, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, REGISTER_SCRATCH, magic.shift as i64, None));
                    }
                    let d_sext = d as i32 as i64;
                    if !self.should_sanitize_constant(d_sext) {
                        self.emit_ins(X86Instruction::imul_immediate(OperandSize::S32, REGISTER_SCRATCH, REGISTER_SCRATCH, d_sext));
                        self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x29, REGISTER_SCRATCH, dst, None));
                    } else {
                        // Pack (q << 32) | x into dst so REGISTER_SCRATCH is free for sanitized load of d
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 4, REGISTER_SCRATCH, 32, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x09, REGISTER_SCRATCH, dst, None));
                        self.emit_sanitized_load_immediate(REGISTER_SCRATCH, d_sext);
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 1, dst, 32, None));
                        self.emit_ins(X86Instruction::alu_escaped(OperandSize::S32, 1, 0xaf, REGISTER_SCRATCH, dst, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, dst, 32, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x29, REGISTER_SCRATCH, dst, None));
                    }
                }
                true
            }
            OperandSize::S64 => {
                let d = imm as u64;
                if d == 0 {
                    return false;
                }
                if d == 1 {
                    if alt_dst {
                        self.emit_ins(X86Instruction::load_immediate(dst, 0));
                    }
                    return true;
                }
                if d.is_power_of_two() {
                    let k = d.trailing_zeros();
                    if alt_dst {
                        self.emit_sanitized_alu(OperandSize::S64, 0x21, 4, dst, (d - 1) as i64);
                    } else {
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, dst, k as i64, None));
                    }
                    return true;
                }
                if d > (1u64 << 63) {
                    let d_i64 = d as i64;
                    if !alt_dst {
                        if !self.should_sanitize_constant(d_i64) && i32::try_from(d_i64).is_ok() {
                            self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, dst, d_i64, None));
                        } else {
                            self.emit_load_constant(REGISTER_SCRATCH, d_i64);
                            self.emit_ins(X86Instruction::cmp(OperandSize::S64, REGISTER_SCRATCH, dst, None));
                        }
                        self.emit_ins(X86Instruction::load_immediate_preserve_flags(dst, 1));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 3, dst, 0, None));
                    } else {
                        self.emit_load_constant(REGISTER_SCRATCH, d_i64);
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 3, REGISTER_SCRATCH, 0, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, dst, REGISTER_SCRATCH, None));
                        self.emit_ins(X86Instruction::cmp(OperandSize::S64, dst, REGISTER_SCRATCH, None));
                        self.emit_ins(X86Instruction::cmov(OperandSize::S64, 0x42, REGISTER_SCRATCH, dst));
                    }
                    return true;
                }
                let m = compute_unsigned_div64_magic(d);
                self.emit_load_constant(REGISTER_SCRATCH, m.magic as i64);
                if dst != RAX {
                    self.emit_ins(X86Instruction::push(RAX, None));
                }
                if dst != RDX {
                    self.emit_ins(X86Instruction::push(RDX, None));
                }
                if !m.add_indicator && !alt_dst {
                    if dst != RAX {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, dst, RAX));
                    }
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 4, REGISTER_SCRATCH, 0, None));
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RDX, m.post_shift as i64, None));
                    if dst != RDX {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, dst));
                    }
                } else if dst != RAX && dst != RDX {
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, dst, RAX));
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 4, REGISTER_SCRATCH, 0, None));
                    if m.add_indicator {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, dst, RAX));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RDX, RAX, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RAX, 1, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, RAX, RDX, None));
                    }
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RDX, m.post_shift as i64, None));
                    if !alt_dst {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, dst));
                    } else {
                        let d_i64 = d as i64;
                        if !self.should_sanitize_constant(d_i64) && i32::try_from(d_i64).is_ok() {
                            self.emit_ins(X86Instruction::imul_immediate(OperandSize::S64, RDX, RDX, d_i64));
                        } else {
                            self.emit_load_constant(RAX, d_i64);
                            self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, RDX, RAX, None));
                        }
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RDX, dst, None));
                    }
                } else {
                    if dst != RAX {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, dst, RAX));
                    }
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RDX));
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, RAX, REGISTER_SCRATCH));
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 4, RDX, 0, None));
                    if m.add_indicator {
                        self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RAX));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RDX, RAX, None));
                        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RAX, 1, None));
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, RAX, RDX, None));
                    }
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RDX, m.post_shift as i64, None));
                    if !alt_dst {
                        if dst != RDX {
                            self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, dst));
                        }
                    } else {
                        let d_i64 = d as i64;
                        if !self.should_sanitize_constant(d_i64) && i32::try_from(d_i64).is_ok() {
                            self.emit_ins(X86Instruction::imul_immediate(OperandSize::S64, RDX, RDX, d_i64));
                            self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RDX, REGISTER_SCRATCH, None));
                            self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, dst));
                        } else {
                            self.emit_ins(X86Instruction::push(REGISTER_SCRATCH, None));
                            self.emit_load_constant(RAX, d_i64);
                            self.emit_ins(X86Instruction::alu_escaped(OperandSize::S64, 1, 0xaf, RDX, RAX, None));
                            self.emit_ins(X86Instruction::pop(REGISTER_SCRATCH));
                            self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, RDX, REGISTER_SCRATCH, None));
                            self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, dst));
                        }
                    }
                }
                if dst != RDX {
                    self.emit_ins(X86Instruction::pop(RDX));
                }
                if dst != RAX {
                    self.emit_ins(X86Instruction::pop(RAX));
                }
                true
            }
            _ => unreachable!(),
        }
    }

    /// Helper for multiplication and division instructions
    #[allow(clippy::too_many_arguments)]
    pub fn emit_product_quotient_remainder(
        &mut self,
        size: OperandSize,
        alt_dst: bool,
        division: bool,
        signed: bool,
        src: X86Register,
        dst: X86Register,
        imm: Option<i64>,
    ) {
        //         LMUL UHMUL SHMUL UDIV SDIV UREM SREM
        // ALU     F7/4 F7/4  F7/5  F7/6 F7/7 F7/6 F7/7
        // src-in  REGISTER_SCRATCH  REGISTER_SCRATCH   REGISTER_SCRATCH   REGISTER_SCRATCH  REGISTER_SCRATCH  REGISTER_SCRATCH  REGISTER_SCRATCH
        // dst-in  RAX  RAX   RAX   RAX  RAX  RAX  RAX
        // dst-out RAX  RDX   RDX   RAX  RAX  RDX  RDX

        if !division && !alt_dst {
            if let Some(imm) = imm {
                if !self.should_sanitize_constant(imm) && i32::try_from(imm).is_ok() {
                    self.emit_ins(X86Instruction::imul_immediate(size, dst, dst, imm));
                } else {
                    self.emit_load_constant(REGISTER_SCRATCH, imm);
                    self.emit_ins(X86Instruction::alu_escaped(size, 1, 0xaf, dst, REGISTER_SCRATCH, None));
                }
                if let OperandSize::S32 = size {
                    if signed && !self.executable.get_sbpf_version().explicit_sign_extension_of_results() {
                        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
                    }
                }
                return;
            }
        }

        if division && !signed {
            if let Some(imm) = imm {
                if self.emit_unsigned_div_rem_imm(size, alt_dst, dst, imm) {
                    return;
                }
            }
        }

        if division {
            // Prevent division by zero
            if imm.is_none() {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, self.pc as i64)); // Save pc
                self.emit_ins(X86Instruction::test(size, src, src, None)); // src == 0
                self.emit_ins(X86Instruction::conditional_jump_immediate(0x84, self.relative_to_anchor(ANCHOR_DIV_BY_ZERO, 6)));
            }

            // Signed division overflows with MIN / -1.
            // If we have an immediate and it's not -1, we can skip the following check.
            if signed && imm.unwrap_or(-1) == -1 {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, if let OperandSize::S64 = size { i64::MIN } else { i32::MIN as i64 }));
                self.emit_ins(X86Instruction::cmp(size, dst, REGISTER_SCRATCH, None)); // dst == MIN

                if imm.is_none() {
                    // The exception case is: dst == MIN && src == -1
                    // Via De Morgan's law becomes: !(dst != MIN || src != -1)
                    // Also, we know that src != 0 in here, so we can use it to set REGISTER_SCRATCH to something not zero
                    self.emit_ins(X86Instruction::load_immediate_preserve_flags(REGISTER_SCRATCH, 0)); // No XOR here because we need to keep the status flags
                    self.emit_ins(X86Instruction::cmov(size, 0x45, src, REGISTER_SCRATCH)); // if dst != MIN { REGISTER_SCRATCH = src; }
                    self.emit_ins(X86Instruction::cmp_immediate(size, src, -1, None)); // src == -1
                    self.emit_ins(X86Instruction::cmov(size, 0x45, src, REGISTER_SCRATCH)); // if src != -1 { REGISTER_SCRATCH = src; }
                    self.emit_ins(X86Instruction::test(size, REGISTER_SCRATCH, REGISTER_SCRATCH, None)); // REGISTER_SCRATCH == 0
                }

                // MIN / -1, raise EbpfError::DivideOverflow
                self.emit_ins(X86Instruction::load_immediate_preserve_flags(REGISTER_SCRATCH, self.pc as i64));
                self.emit_ins(X86Instruction::conditional_jump_immediate(0x84, self.relative_to_anchor(ANCHOR_DIV_OVERFLOW, 6)));
            }
        }

        if let Some(imm) = imm {
            if self.should_sanitize_constant(imm) {
                self.emit_sanitized_load_immediate(REGISTER_SCRATCH, imm);
            } else {
                self.emit_ins(X86Instruction::load_immediate(REGISTER_SCRATCH, imm));
            }
        } else {
            self.emit_ins(X86Instruction::mov(OperandSize::S64, src, REGISTER_SCRATCH));
        }
        if dst != RAX {
            self.emit_ins(X86Instruction::push(RAX, None));
            self.emit_ins(X86Instruction::mov(OperandSize::S64, dst, RAX));
        }
        if dst != RDX {
            self.emit_ins(X86Instruction::push(RDX, None));
        }
        if division {
            if signed {
                self.emit_ins(X86Instruction::sign_extend_rax_rdx(size));
            } else {
                self.emit_ins(X86Instruction::alu(size, 0x31, RDX, RDX, None)); // RDX = 0
            }
        }

        self.emit_ins(X86Instruction::alu_immediate(size, 0xf7, 0x4 | ((division as u8) << 1) | signed as u8, REGISTER_SCRATCH, 0, None));

        if dst != RDX {
            if alt_dst {
                self.emit_ins(X86Instruction::mov(OperandSize::S64, RDX, dst));
            }
            self.emit_ins(X86Instruction::pop(RDX));
        }
        if dst != RAX {
            if !alt_dst {
                self.emit_ins(X86Instruction::mov(OperandSize::S64, RAX, dst));
            }
            self.emit_ins(X86Instruction::pop(RAX));
        }
        if let OperandSize::S32 = size {
            if signed && !self.executable.get_sbpf_version().explicit_sign_extension_of_results() {
                self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x63, dst, dst, None)); // sign extend i32 to i64
            }
        }
    }

    fn emit_set_exception_kind(&mut self, err: EbpfError) {
        let err_kind = unsafe { *std::ptr::addr_of!(err).cast::<u64>() };
        let err_discriminant = ProgramResult::Err(err).discriminant();
        self.emit_ins(X86Instruction::store_immediate(OperandSize::S64, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult)), err_discriminant as i64)); // result.discriminant = err_discriminant;
        self.emit_ins(X86Instruction::store_immediate(OperandSize::S64, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult) + std::mem::size_of::<u64>() as i32), err_kind as i64)); // err.kind = err_kind;
    }

    fn emit_check_result_is_ok(&mut self) {
        let ok = ProgramResult::Ok(0);
        let ok_discriminant = ok.discriminant();
        self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, REGISTER_PTR_TO_VM, ok_discriminant as i64, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult)))));
    }

    fn emit_subroutines(&mut self) {
        // Routine for instruction tracing
        #[cfg(feature = "tracer")]
        {
            self.set_anchor(ANCHOR_TRACE);
            // Save registers on stack
            self.emit_ins(X86Instruction::push(REGISTER_SCRATCH, None));
            for reg in REGISTER_MAP.iter().rev() {
                self.emit_ins(X86Instruction::push(*reg, None));
            }
            self.emit_ins(X86Instruction::mov(OperandSize::S64, RSP, REGISTER_MAP[0]));
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, - 8 * 3, None)); // RSP -= 8 * 3;
            self.emit_rust_call(Value::Constant64(Vec::<crate::static_analysis::RegisterTraceEntry>::push as *const u8 as i64, false), &[
                Argument { index: 1, value: Value::Register(REGISTER_MAP[0]) }, // registers
                Argument { index: 0, value: Value::RegisterPlusConstant32(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::RegisterTrace), false) },
            ], &[]);
            // Pop stack and return
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, 8 * 3, None)); // RSP += 8 * 3;
            self.emit_ins(X86Instruction::pop(REGISTER_MAP[0]));
            self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, 8 * (REGISTER_MAP.len() - 1) as i64, None)); // RSP += 8 * (REGISTER_MAP.len() - 1);
            self.emit_ins(X86Instruction::pop(REGISTER_SCRATCH));
            self.emit_ins(X86Instruction::return_near());
        }

        // Epilogue
        self.set_anchor(ANCHOR_EPILOGUE);
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, 1, None)); // REGISTER_INSTRUCTION_METER -= 1;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, REGISTER_SCRATCH, REGISTER_INSTRUCTION_METER, None)); // REGISTER_INSTRUCTION_METER -= pc;
        // *DueInsnCount = *PreviousInstructionMeter - REGISTER_INSTRUCTION_METER;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x2B, REGISTER_INSTRUCTION_METER, REGISTER_PTR_TO_VM, Some(X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::PreviousInstructionMeter))))); // REGISTER_INSTRUCTION_METER -= *PreviousInstructionMeter;
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xf7, 3, REGISTER_INSTRUCTION_METER, 0, None)); // REGISTER_INSTRUCTION_METER = -REGISTER_INSTRUCTION_METER;
        self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_INSTRUCTION_METER, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::DueInsnCount)))); // *DueInsnCount = REGISTER_INSTRUCTION_METER;
        // Print stop watch value
        fn stopwatch_result(numerator: u64, denominator: u64) {
            println!("Stop watch: {} / {} = {}", numerator, denominator, if denominator == 0 { 0.0 } else { numerator as f64 / denominator as f64 });
        }
        if self.stopwatch_is_active {
            self.emit_rust_call(Value::Constant64(stopwatch_result as *const u8 as i64, false), &[
                Argument { index: 1, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::StopwatchDenominator), false) },
                Argument { index: 0, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::StopwatchNumerator), false) },
            ], &[]);
        }
        // Restore stack pointer in case we did not exit gracefully
        self.emit_ins(X86Instruction::load(OperandSize::S64, REGISTER_PTR_TO_VM, RSP, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::HostStackPointer))));
        self.emit_ins(X86Instruction::return_near());

        // Handler for EbpfError::ExceededMaxInstructions
        self.set_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS);
        self.emit_set_exception_kind(EbpfError::ExceededMaxInstructions);
        self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_INSTRUCTION_METER, REGISTER_SCRATCH)); // REGISTER_SCRATCH = REGISTER_INSTRUCTION_METER;
        // Cancel the epilogue's decrement so due_insn_count matches the interpreter.
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, 1, None)); // REGISTER_INSTRUCTION_METER += 1;
        // Fall through

        // Epilogue for errors
        self.set_anchor(ANCHOR_THROW_EXCEPTION_UNCHECKED);
        self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_SCRATCH, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::Registers) + 11 * std::mem::size_of::<u64>() as i32))); // registers[11] = pc;
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_EPILOGUE, 5)));

        // Quit gracefully
        self.set_anchor(ANCHOR_EXIT);
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_INSTRUCTION_METER, 1, None)); // REGISTER_INSTRUCTION_METER += 1;
        self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_SCRATCH, REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::Registers) + 11 * std::mem::size_of::<u64>() as i32))); // registers[11] = pc;
        self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_MAP[0], REGISTER_PTR_TO_VM, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult) + std::mem::size_of::<u64>() as i32))); // result.return_value = R0;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x31, REGISTER_SCRATCH, REGISTER_SCRATCH, None)); // REGISTER_SCRATCH ^= REGISTER_SCRATCH; // REGISTER_SCRATCH = 0;
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_EPILOGUE, 5)));

        // Handler for exceptions which report their pc
        self.set_anchor(ANCHOR_THROW_EXCEPTION);
        // Validate that we did not reach the instruction meter limit before the exception occured
        self.emit_validate_instruction_count(None);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION_UNCHECKED, 5)));

        // Handler for EbpfError::CallDepthExceeded
        self.set_anchor(ANCHOR_CALL_REG_DEPTH_EXCEEDED);
        self.emit_ins(X86Instruction::load(OperandSize::S64, RSP, REGISTER_SCRATCH, X86IndirectAccess::OffsetIndexShift(-16, RSP, 0)));
        // Fall through
        self.set_anchor(ANCHOR_CALL_DEPTH_EXCEEDED);
        self.emit_set_exception_kind(EbpfError::CallDepthExceeded);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        // Handler for EbpfError::CallOutsideTextSegment
        self.set_anchor(ANCHOR_CALL_REG_OUTSIDE_TEXT_SEGMENT);
        self.emit_set_exception_kind(EbpfError::CallOutsideTextSegment);
        self.emit_ins(X86Instruction::load(OperandSize::S64, RSP, REGISTER_SCRATCH, X86IndirectAccess::OffsetIndexShift(-8, RSP, 0)));
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        // Handler for EbpfError::DivideByZero
        self.set_anchor(ANCHOR_DIV_BY_ZERO);
        self.emit_set_exception_kind(EbpfError::DivideByZero);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        // Handler for EbpfError::DivideOverflow
        self.set_anchor(ANCHOR_DIV_OVERFLOW);
        self.emit_set_exception_kind(EbpfError::DivideOverflow);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        // See `ANCHOR_INTERNAL_FUNCTION_CALL_REG` for more details.
        self.set_anchor(ANCHOR_CALL_REG_UNSUPPORTED_INSTRUCTION);
        self.emit_ins(X86Instruction::load(OperandSize::S64, RSP, REGISTER_SCRATCH, X86IndirectAccess::OffsetIndexShift(-8, RSP, 0))); // Retrieve the current program counter from the stack
        self.set_anchor(ANCHOR_CALL_REG_LDDW_UNSUPPORTED);
        self.emit_ins(X86Instruction::pop(REGISTER_MAP[0])); // Restore the clobbered REGISTER_MAP[0]
        // Fall through

        // Handler for EbpfError::UnsupportedInstruction
        self.set_anchor(ANCHOR_CALL_UNSUPPORTED_INSTRUCTION);
        #[cfg(feature = "tracer")]
        self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_TRACE, 5)));
        self.emit_set_exception_kind(EbpfError::UnsupportedInstruction);
        self.emit_ins(X86Instruction::jump_immediate(self.relative_to_anchor(ANCHOR_THROW_EXCEPTION, 5)));

        // Routine for external functions
        self.set_anchor(ANCHOR_EXTERNAL_FUNCTION_CALL);
        self.emit_validate_instruction_count(None);
        self.emit_instruction_meter_to_vm(Value::Register(REGISTER_SCRATCH));
        self.emit_rust_call(Value::RegisterIndirect(RSP, 0x50, false), &[
            Argument { index: 5, value: Value::Register(ARGUMENT_REGISTERS[5]) },
            Argument { index: 4, value: Value::Register(ARGUMENT_REGISTERS[4]) },
            Argument { index: 3, value: Value::Register(ARGUMENT_REGISTERS[3]) },
            Argument { index: 2, value: Value::Register(ARGUMENT_REGISTERS[2]) },
            Argument { index: 1, value: Value::Register(ARGUMENT_REGISTERS[1]) },
            Argument { index: 0, value: Value::Register(REGISTER_PTR_TO_VM) },
        ], &[]);
        self.emit_instruction_meter_from_vm(Value::Register(REGISTER_SCRATCH));
        // Test if result indicates that an error occured
        self.emit_check_result_is_ok();
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x85, self.relative_to_anchor(ANCHOR_THROW_EXCEPTION_UNCHECKED, 6)));
        // Store Ok value in result register
        self.emit_ins(X86Instruction::load(OperandSize::S64, REGISTER_PTR_TO_VM, REGISTER_MAP[0], X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult) + std::mem::size_of::<u64>() as i32)));
        self.emit_ins(X86Instruction::return_near());

        // Routine for emit_internal_call(Value::Register())
        // Entry stack: [RSP] = return_rip, [RSP - 16] = guest_current_pc
        // Inputs: Guest target address in REGISTER_SCRATCH
        // Outputs: Host target address in REGISTER_SCRATCH
        self.set_anchor(ANCHOR_INTERNAL_FUNCTION_CALL_REG);
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x3B, REGISTER_INSTRUCTION_METER, RSP, Some(X86IndirectAccess::OffsetIndexShift(-16, RSP, 0))));
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x86, self.relative_to_anchor(ANCHOR_THROW_EXCEEDED_MAX_INSTRUCTIONS, 6)));
        let call_depth_access = X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::CallDepth));
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_PTR_TO_VM, 1, Some(call_depth_access)));
        self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S32, REGISTER_PTR_TO_VM, self.config.max_call_depth as i64, Some(call_depth_access)));
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x83, self.relative_to_anchor(ANCHOR_CALL_REG_DEPTH_EXCEEDED, 6)));
        if !self.executable.get_sbpf_version().manual_stack_frame_bump() {
            let num_frames = if self.executable.get_sbpf_version().stack_frame_gaps()
                && self.config.enable_stack_frame_gaps {
                2
            } else {
                1
            };
            let stack_frame_size = self.config.stack_frame_size as i64 * num_frames;
            if stack_frame_size != 0 {
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, REGISTER_MAP[FRAME_PTR_REG], stack_frame_size, None));
            }
        }
        // Push RAX: now [RSP] = saved RAX, [RSP + 8] = return_rip, [RSP - 8] = guest_current_pc
        self.emit_ins(X86Instruction::push(REGISTER_MAP[0], None));
        // Calculate offset relative to program_vm_addr
        self.emit_ins(X86Instruction::load_immediate(REGISTER_MAP[0], self.program_vm_addr as i64));
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x29, REGISTER_MAP[0], REGISTER_SCRATCH, None)); // guest_target_pc = guest_target_address - self.program_vm_addr;
        // Force alignment of guest_target_pc
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 4, REGISTER_SCRATCH, !(INSN_SIZE as i64 - 1), None)); // guest_target_pc &= !(INSN_SIZE - 1);
        // Bound check
        // if(guest_target_pc >= number_of_instructions * INSN_SIZE) throw CALL_OUTSIDE_TEXT_SEGMENT;
        let number_of_instructions = self.pc_section_len;
        self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S64, REGISTER_SCRATCH, (number_of_instructions * INSN_SIZE) as i64, None)); // guest_target_pc.cmp(number_of_instructions * INSN_SIZE)
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x83, self.relative_to_anchor(ANCHOR_CALL_REG_OUTSIDE_TEXT_SEGMENT, 6)));
        // Calculate the guest_target_pc (dst / INSN_SIZE) to update REGISTER_INSTRUCTION_METER
        // and as target_pc for potential ANCHOR_CALL_REG_UNSUPPORTED_INSTRUCTION
        let shift_amount = INSN_SIZE.trailing_zeros();
        debug_assert_eq!(INSN_SIZE, 1 << shift_amount);
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, REGISTER_SCRATCH, shift_amount as i64, None)); // guest_target_pc /= INSN_SIZE;
        // Load host_target_address offset from self.result.pc_section
        self.emit_ins(X86Instruction::load_immediate(REGISTER_MAP[0], self.pc_section_ptr as i64)); // host_target_address = self.result.pc_section;
        self.emit_ins(X86Instruction::load(OperandSize::S32, REGISTER_MAP[0], REGISTER_MAP[0], X86IndirectAccess::OffsetIndexShift(0, REGISTER_SCRATCH, 2))); // host_target_address = self.result.pc_section[guest_target_pc];
        // Check destination is valid
        self.emit_ins(X86Instruction::test_immediate(OperandSize::S32, REGISTER_MAP[0], 1 << 31, None)); // host_target_address & (1 << 31)
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x85, self.relative_to_anchor(ANCHOR_CALL_REG_UNSUPPORTED_INSTRUCTION, 6))); // If host_target_address & (1 << 31) != 0, throw UnsupportedInstruction
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S32, 0x81, 4, REGISTER_MAP[0], i32::MAX as i64, None)); // host_target_address &= (1 << 31) - 1;
        // A version of `self.emit_profile_instruction_count(None);` which reads self.pc from the stack
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x2b, REGISTER_INSTRUCTION_METER, RSP, Some(X86IndirectAccess::OffsetIndexShift(-8, RSP, 0)))); // instruction_meter -= guest_current_pc;
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, REGISTER_INSTRUCTION_METER, 1, None)); // instruction_meter -= 1;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_SCRATCH, REGISTER_INSTRUCTION_METER, None)); // instruction_meter += guest_target_pc;
        let unsupported_offset = unsafe { self.anchors[ANCHOR_CALL_UNSUPPORTED_INSTRUCTION].offset_from(self.text_ptr) as i64 };
        self.emit_ins(X86Instruction::cmp_immediate(OperandSize::S32, REGISTER_MAP[0], unsupported_offset, None));
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x84, self.relative_to_anchor(ANCHOR_CALL_REG_LDDW_UNSUPPORTED, 6)));
        // Compute final host_target_address into REGISTER_SCRATCH, restore REGISTER_MAP[0], and return
        self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_MAP[0], REGISTER_SCRATCH));
        self.emit_ins(X86Instruction::load_immediate(REGISTER_MAP[0], self.text_ptr as i64)); // RAX = self.result.text_section;
        self.emit_ins(X86Instruction::alu(OperandSize::S64, 0x01, REGISTER_MAP[0], REGISTER_SCRATCH, None)); // host_target_address += self.result.text_section;
        self.emit_ins(X86Instruction::pop(REGISTER_MAP[0]));
        self.emit_ins(X86Instruction::return_near());

        self.set_anchor(ANCHOR_FIRST_ATTEMPT_SUCCESS);
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, 32, None)); // RSP += 32
        self.emit_ins(X86Instruction::return_near());

        // Cold helper: recovers self.pc from return RIP on [RSP_guest - 8] into REGISTER_SCRATCH
        // and enforces instruction meter precedence before second_attempt / fault generation.
        // Invoked with RSP == RSP_guest - 40, so inside ANCHOR_RECOVER_MEMORY_PC:
        // RSP == RSP_guest - 48 (16B-aligned), and after emit_rust_call pushes 72B of caller-saved regs,
        // return_rip at [RSP_guest - 8] is at [RSP + 112].
        self.set_anchor(ANCHOR_RECOVER_MEMORY_PC);
        self.emit_rust_call(
            Value::Constant64(lookup_memory_pc as *const u8 as i64, false),
            &[
                Argument {
                    index: 1,
                    value: Value::RegisterIndirect(RSP, 112, false),
                },
                Argument {
                    index: 0,
                    value: Value::Constant64(self.result.memory_pc_table_ptr() as i64, false),
                },
            ],
            &[REGISTER_SCRATCH],
        );
        self.emit_validate_instruction_count(None);
        self.emit_ins(X86Instruction::return_near());

        // Translates a vm memory address to a host memory address
        let lower_key = self.immediate_value_key as i32 as i64;
        for len in [1i32, 2i32, 4i32, 8i32] {
            let size_idx = len.trailing_zeros() as usize;
            // AccessType::Load (anchor_base = 0)
            self.emit_translate_memory_anchor(0, len, size_idx);
            // Sanitized immediate store stub (anchor_base = 8): unmask [RSP_guest - 32] (= [RSP - 24] after `call`)
            // and fall directly through into AccessType::Store (anchor_base = 4).
            self.set_anchor(ANCHOR_TRANSLATE_MEMORY_ADDRESS + 8 + size_idx);
            self.emit_ins(X86Instruction::alu_immediate(
                OperandSize::S64,
                0x81,
                0,
                RSP,
                lower_key,
                Some(X86IndirectAccess::OffsetIndexShift(-24, RSP, 0)),
            ));
            // AccessType::Store (anchor_base = 4)
            self.emit_translate_memory_anchor(4, len, size_idx);
        }
    }

    fn emit_forward_cond_jump(&mut self, op: u8) -> usize {
        let jump_end = self.offset_in_text_section + 6;
        self.emit_ins(X86Instruction::conditional_jump_immediate(op, 0));
        jump_end
    }

    fn emit_forward_jump(&mut self) -> usize {
        let jump_end = self.offset_in_text_section + 5;
        self.emit_ins(X86Instruction::jump_immediate(0));
        jump_end
    }

    fn patch_forward_jump(&mut self, jump_end: usize) {
        let target = self.offset_in_text_section;
        let rel = (target as isize - jump_end as isize) as i32;
        unsafe {
            let ptr = self
                .result
                .text_section_mut()
                .as_mut_ptr()
                .add(jump_end - 4)
                .cast::<i32>();
            ptr::write_unaligned(ptr, rel);
        }
    }

    fn emit_translate_memory_anchor(&mut self, anchor_base: usize, len: i32, size_idx: usize) {
        debug_assert!(matches!(anchor_base, 0 | 4));
        self.set_anchor(ANCHOR_TRANSLATE_MEMORY_ADDRESS + anchor_base + size_idx);

        // Pure-x86 inline fast path when gapped stack frames are not active.
        // Entry state: RSP == RSP_guest - 8 ([rsp] == return_rip, [rsp - 24] == store_value), R11 == vm_addr.
        if self.inline_fast_mem {
            let use_aligned_mapping = self.executable.get_sbpf_version() >= SBPFVersion::V4
                || self.config.aligned_memory_mapping;
            let mut slow_jumps = [0usize; 6];
            let mut slow_jump_count = 0usize;

            self.emit_ins(X86Instruction::push(RAX, None)); // RSP_guest - 16
            self.emit_ins(X86Instruction::push(RCX, None)); // RSP_guest - 24 ([rsp - 8] == store_value)
            self.emit_ins(X86Instruction::load(
                OperandSize::S64,
                REGISTER_PTR_TO_VM,
                RAX,
                X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::MemoryMapping)),
            ));

            if use_aligned_mapping {
                let va_bits = self.config.aligned_region_size.trailing_zeros();
                self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RCX));
                if va_bits >= 64 {
                    self.emit_ins(X86Instruction::alu(OperandSize::S32, 0x31, RCX, RCX, None));
                } else if va_bits > 0 {
                    self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 5, RCX, va_bits as i64, None));
                }
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x3b,
                    RCX,
                    RAX,
                    Some(X86IndirectAccess::Offset(MemoryMapping::REGIONS_LEN_OFFSET as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x83); // jae .slow_path
                slow_jump_count += 1;

                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 4, RCX, MemoryRegion::SIZE_SHIFT as i64, None));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x03,
                    RCX,
                    RAX,
                    Some(X86IndirectAccess::Offset(MemoryMapping::REGIONS_PTR_OFFSET as i32)),
                ));
                self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RAX));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x2b,
                    RAX,
                    RCX,
                    Some(X86IndirectAccess::Offset(MemoryRegion::VM_ADDR_OFFSET as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x82); // jb .slow_path
                slow_jump_count += 1;
            } else {
                // Probe MappingCache entries in MRU-first order [3, 2, 1, 0].
                let entry_off = |idx: usize| {
                    MemoryMapping::UNALIGNED_CACHE_ENTRIES_OFFSET + idx * MappingCacheEntry::SIZE
                };
                let mut hit_jumps = [0usize; 3];
                for (probe_i, &cache_idx) in [3usize, 2, 1].iter().enumerate() {
                    let base = entry_off(cache_idx);
                    self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RCX));
                    self.emit_ins(X86Instruction::alu(
                        OperandSize::S64,
                        0x2b,
                        RCX,
                        RAX,
                        Some(X86IndirectAccess::Offset((base + MappingCacheEntry::VM_ADDR_START_OFFSET) as i32)),
                    ));
                    self.emit_ins(X86Instruction::alu(
                        OperandSize::S64,
                        0x3b,
                        RCX,
                        RAX,
                        Some(X86IndirectAccess::Offset((base + MappingCacheEntry::VM_LEN_OFFSET) as i32)),
                    ));
                    hit_jumps[probe_i] = self.emit_forward_cond_jump(0x82); // jb .hit
                }
                let base0 = entry_off(0);
                self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RCX));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x2b,
                    RCX,
                    RAX,
                    Some(X86IndirectAccess::Offset((base0 + MappingCacheEntry::VM_ADDR_START_OFFSET) as i32)),
                ));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x3b,
                    RCX,
                    RAX,
                    Some(X86IndirectAccess::Offset((base0 + MappingCacheEntry::VM_LEN_OFFSET) as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x83); // jae .slow_path
                slow_jump_count += 1;

                self.emit_ins(X86Instruction::load(
                    OperandSize::S64,
                    RAX,
                    RCX,
                    X86IndirectAccess::Offset((base0 + MappingCacheEntry::REGION_INDEX_OFFSET) as i32),
                ));
                let jmp_hit_0 = self.emit_forward_jump();

                self.patch_forward_jump(hit_jumps[0]);
                self.emit_ins(X86Instruction::load(
                    OperandSize::S64,
                    RAX,
                    RCX,
                    X86IndirectAccess::Offset((entry_off(3) + MappingCacheEntry::REGION_INDEX_OFFSET) as i32),
                ));
                let jmp_hit_3 = self.emit_forward_jump();

                self.patch_forward_jump(hit_jumps[1]);
                self.emit_ins(X86Instruction::load(
                    OperandSize::S64,
                    RAX,
                    RCX,
                    X86IndirectAccess::Offset((entry_off(2) + MappingCacheEntry::REGION_INDEX_OFFSET) as i32),
                ));
                let jmp_hit_2 = self.emit_forward_jump();

                self.patch_forward_jump(hit_jumps[2]);
                self.emit_ins(X86Instruction::load(
                    OperandSize::S64,
                    RAX,
                    RCX,
                    X86IndirectAccess::Offset((entry_off(1) + MappingCacheEntry::REGION_INDEX_OFFSET) as i32),
                ));

                self.patch_forward_jump(jmp_hit_0);
                self.patch_forward_jump(jmp_hit_3);
                self.patch_forward_jump(jmp_hit_2);

                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0xc1, 4, RCX, MemoryRegion::SIZE_SHIFT as i64, None));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x03,
                    RCX,
                    RAX,
                    Some(X86IndirectAccess::Offset(MemoryMapping::REGIONS_PTR_OFFSET as i32)),
                ));
                self.emit_ins(X86Instruction::mov(OperandSize::S64, REGISTER_SCRATCH, RAX));
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x2b,
                    RAX,
                    RCX,
                    Some(X86IndirectAccess::Offset(MemoryRegion::VM_ADDR_OFFSET as i32)),
                ));
            }

            if len == 1 {
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x3b,
                    RAX,
                    RCX,
                    Some(X86IndirectAccess::Offset(MemoryRegion::LEN_OFFSET as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x83); // jae .slow_path
                slow_jump_count += 1;
            } else {
                self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RAX, len as i64, None));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x82); // jc .slow_path
                slow_jump_count += 1;
                self.emit_ins(X86Instruction::alu(
                    OperandSize::S64,
                    0x3b,
                    RAX,
                    RCX,
                    Some(X86IndirectAccess::Offset(MemoryRegion::LEN_OFFSET as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x87); // ja .slow_path
                slow_jump_count += 1;
            }

            if anchor_base != 0 {
                self.emit_ins(X86Instruction::cmp_immediate(
                    OperandSize::S8,
                    RCX,
                    0,
                    Some(X86IndirectAccess::Offset(MemoryRegion::WRITABLE_OFFSET as i32)),
                ));
                slow_jumps[slow_jump_count] = self.emit_forward_cond_jump(0x84); // je .slow_path
                slow_jump_count += 1;
            }

            self.emit_ins(X86Instruction::alu(
                OperandSize::S64,
                0x03,
                RAX,
                RCX,
                Some(X86IndirectAccess::Offset(MemoryRegion::HOST_ADDR_OFFSET as i32)),
            ));
            let access_disp = if len == 1 { 0 } else { -len };
            let op_size = match len {
                1 => OperandSize::S8,
                2 => OperandSize::S16,
                4 => OperandSize::S32,
                8 => OperandSize::S64,
                _ => unreachable!(),
            };
            if anchor_base == 0 {
                self.emit_ins(X86Instruction::load(
                    op_size,
                    RAX,
                    REGISTER_SCRATCH,
                    X86IndirectAccess::Offset(access_disp),
                ));
            } else {
                self.emit_ins(X86Instruction::load(
                    if len == 8 { OperandSize::S64 } else { OperandSize::S32 },
                    RSP,
                    RCX,
                    X86IndirectAccess::OffsetIndexShift(-8, RSP, 0),
                ));
                self.emit_ins(X86Instruction::store(
                    op_size,
                    RCX,
                    RAX,
                    X86IndirectAccess::Offset(access_disp),
                ));
            }
            self.emit_ins(X86Instruction::pop(RCX));
            self.emit_ins(X86Instruction::pop(RAX));
            self.emit_ins(X86Instruction::return_near());

            for &j in &slow_jumps[..slow_jump_count] {
                self.patch_forward_jump(j);
            }
            self.emit_ins(X86Instruction::pop(RCX));
            self.emit_ins(X86Instruction::pop(RAX));
        }

        // Slow path: RSP == RSP_guest - 8, [RSP - 24] == store_value (at RSP_guest - 32), R11 == vm_addr.
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 5, RSP, 24, None)); // RSP = RSP_guest - 32
        if anchor_base == 0 { // AccessType::Load
            let load = match len {
                1 => MemoryMapping::load_first_attempt::<u8> as *const u8 as i64,
                2 => MemoryMapping::load_first_attempt::<u16> as *const u8 as i64,
                4 => MemoryMapping::load_first_attempt::<u32> as *const u8 as i64,
                8 => MemoryMapping::load_first_attempt::<u64> as *const u8 as i64,
                _ => unreachable!()
            };
            self.emit_ins(X86Instruction::push(REGISTER_SCRATCH, None)); // RSP_guest - 40
            self.emit_ins(X86Instruction::push(REGISTER_MAP[0], None));  // RSP_guest - 48 (16B-aligned)
            self.emit_rust_call(Value::Constant64(load, false), &[
                Argument { index: 1, value: Value::Register(REGISTER_SCRATCH) }, // Specify first as the src register could be overwritten by other arguments
                Argument { index: 0, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::MemoryMapping), false) },
            ], &[REGISTER_MAP[0], REGISTER_SCRATCH]);
            self.emit_ins(X86Instruction::test(OperandSize::S8, REGISTER_MAP[0], REGISTER_MAP[0], None));
            self.emit_ins(X86Instruction::pop(REGISTER_MAP[0])); // Leaves the flags alone; RSP = RSP_guest - 40
            self.emit_ins(X86Instruction::conditional_jump_immediate(0x85, self.relative_to_anchor(ANCHOR_FIRST_ATTEMPT_SUCCESS, 6)));
            // Cold fault / COW path: recover pc from return RIP and check instruction meter before second_attempt
            self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_RECOVER_MEMORY_PC, 5)));
            self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_SCRATCH, RSP, X86IndirectAccess::OffsetIndexShift(16, RSP, 0))); // [RSP_guest - 24] = pc
            self.emit_ins(X86Instruction::pop(REGISTER_SCRATCH)); // R11 = vm_addr, RSP = RSP_guest - 32 (16B-aligned)
            self.emit_instruction_meter_to_vm(Value::RegisterIndirect(RSP, 8, false));
            let load = match len {
                1 => MemoryMapping::load_second_attempt::<u8, C> as *const u8 as i64,
                2 => MemoryMapping::load_second_attempt::<u16, C> as *const u8 as i64,
                4 => MemoryMapping::load_second_attempt::<u32, C> as *const u8 as i64,
                8 => MemoryMapping::load_second_attempt::<u64, C> as *const u8 as i64,
                _ => unreachable!()
            };
            self.emit_rust_call(Value::Constant64(load, false), &[
                Argument { index: 2, value: Value::Register(REGISTER_PTR_TO_VM) },
                Argument { index: 1, value: Value::Register(REGISTER_SCRATCH) }, // Specify first as the src register could be overwritten by other arguments
                Argument { index: 0, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::MemoryMapping), false) },
            ], &[]);
            self.emit_instruction_meter_from_vm(Value::RegisterIndirect(RSP, 8, false));
        } else { // AccessType::Store
            let store = match len {
                1 => MemoryMapping::store_first_attempt::<u8> as *const u8 as i64,
                2 => MemoryMapping::store_first_attempt::<u16> as *const u8 as i64,
                4 => MemoryMapping::store_first_attempt::<u32> as *const u8 as i64,
                8 => MemoryMapping::store_first_attempt::<u64> as *const u8 as i64,
                _ => unreachable!()
            };
            self.emit_ins(X86Instruction::push(REGISTER_SCRATCH, None)); // RSP_guest - 40
            self.emit_ins(X86Instruction::push(REGISTER_MAP[0], None));  // RSP_guest - 48 (16B-aligned)
            self.emit_rust_call(Value::Constant64(store, false), &[
                Argument { index: 2, value: Value::Register(REGISTER_SCRATCH) }, // Specify first as the src register could be overwritten by other arguments
                Argument { index: 1, value: Value::RegisterIndirect(RSP, 88, false) }, // [RSP_guest - 32] after 72B caller-saved push
                Argument { index: 0, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::MemoryMapping), false) },
            ], &[REGISTER_MAP[0]]);
            self.emit_ins(X86Instruction::test(OperandSize::S8, REGISTER_MAP[0], REGISTER_MAP[0], None));
            self.emit_ins(X86Instruction::pop(REGISTER_MAP[0])); // Leaves the flags alone; RSP = RSP_guest - 40
            self.emit_ins(X86Instruction::conditional_jump_immediate(0x85, self.relative_to_anchor(ANCHOR_FIRST_ATTEMPT_SUCCESS, 6)));
            // Cold fault / COW path: recover pc from return RIP and check instruction meter before second_attempt
            self.emit_ins(X86Instruction::call_immediate(self.relative_to_anchor(ANCHOR_RECOVER_MEMORY_PC, 5)));
            self.emit_ins(X86Instruction::store(OperandSize::S64, REGISTER_SCRATCH, RSP, X86IndirectAccess::OffsetIndexShift(16, RSP, 0))); // [RSP_guest - 24] = pc
            self.emit_ins(X86Instruction::pop(REGISTER_SCRATCH)); // R11 = vm_addr, RSP = RSP_guest - 32 (16B-aligned)
            self.emit_instruction_meter_to_vm(Value::RegisterIndirect(RSP, 8, false));
            let store = match len {
                1 => MemoryMapping::store_second_attempt::<u8, C> as *const u8 as i64,
                2 => MemoryMapping::store_second_attempt::<u16, C> as *const u8 as i64,
                4 => MemoryMapping::store_second_attempt::<u32, C> as *const u8 as i64,
                8 => MemoryMapping::store_second_attempt::<u64, C> as *const u8 as i64,
                _ => unreachable!()
            };
            self.emit_rust_call(Value::Constant64(store, false), &[
                Argument { index: 3, value: Value::Register(REGISTER_PTR_TO_VM) },
                Argument { index: 2, value: Value::Register(REGISTER_SCRATCH) }, // Specify first as the src register could be overwritten by other arguments
                Argument { index: 1, value: Value::RegisterIndirect(RSP, 72, false) }, // [RSP_guest - 32] after 72B caller-saved push
                Argument { index: 0, value: Value::RegisterIndirect(REGISTER_PTR_TO_VM, self.slot_in_vm(RuntimeEnvironmentSlot::MemoryMapping), false) },
            ], &[]);
            self.emit_instruction_meter_from_vm(Value::RegisterIndirect(RSP, 8, false));
        }

        // Throw error if the result indicates one
        self.emit_ins(X86Instruction::load(OperandSize::S64, RSP, REGISTER_SCRATCH, X86IndirectAccess::OffsetIndexShift(8, RSP, 0))); // REGISTER_SCRATCH = pc
        self.emit_ins(X86Instruction::alu_immediate(OperandSize::S64, 0x81, 0, RSP, 24, None)); // RSP += 24 -> RSP_guest - 8
        self.emit_check_result_is_ok();
        self.emit_ins(X86Instruction::conditional_jump_immediate(0x85, self.relative_to_anchor(ANCHOR_THROW_EXCEPTION_UNCHECKED, 6)));

        if anchor_base == 0 { // AccessType::Load
            // unwrap() the result into REGISTER_SCRATCH
            self.emit_ins(X86Instruction::load(OperandSize::S64, REGISTER_PTR_TO_VM, REGISTER_SCRATCH, X86IndirectAccess::Offset(self.slot_in_vm(RuntimeEnvironmentSlot::ProgramResult) + std::mem::size_of::<u64>() as i32)));
        }

        self.emit_ins(X86Instruction::return_near());
    }

    #[inline]
    fn set_anchor(&mut self, anchor: usize) {
        self.anchors[anchor] = unsafe { self.text_ptr.add(self.offset_in_text_section).cast_const() };
    }

    /// Resolves or records a relocation for jumps/calls to the subroutines
    ///
    /// instruction_length = 5 (Unconditional jump / call)
    /// instruction_length = 6 (Conditional jump)
    #[inline]
    pub fn relative_to_anchor(&self, anchor: usize, instruction_length: usize) -> i32 {
        let instruction_end = unsafe { self.text_ptr.add(self.offset_in_text_section + instruction_length).cast_const() };
        let destination = self.anchors[anchor];
        debug_assert!(!destination.is_null());
        (unsafe { destination.offset_from(instruction_end) } as i32) // Relative jump
    }

    /// Resolves or records a relocation for jumps/calls inside the program
    #[inline]
    pub fn relative_to_target_pc(&mut self, target_pc: usize, instruction_length: usize) -> i32 {
        let instruction_end = unsafe { self.text_ptr.add(self.offset_in_text_section + instruction_length).cast_const() };
        assert!(target_pc < self.pc_section_len);
        let target_offset = unsafe { *self.pc_section_ptr.add(target_pc) };
        let destination = if target_offset != 0 {
            // Backward jump
            let offset = target_offset as usize & (i32::MAX as u32 as usize);
            debug_assert!(offset < self.text_len);
            unsafe { self.text_ptr.add(offset).cast_const() }
        } else {
            // Forward jump, needs relocation
            self.text_section_jumps.push(Jump { location: unsafe { instruction_end.sub(4) }, target_pc });
            return 0;
        };
        debug_assert!(!destination.is_null());
        (unsafe { destination.offset_from(instruction_end) } as i32) // Relative jump
    }

    fn resolve_jumps(&mut self) {
        // Relocate forward jumps
        for jump in &self.text_section_jumps {
            assert!(jump.target_pc < self.pc_section_len);
            let target_offset = unsafe { *self.pc_section_ptr.add(jump.target_pc) };
            let offset = target_offset as usize & (i32::MAX as u32 as usize);
            debug_assert!(offset < self.text_len);
            let destination = unsafe { self.text_ptr.add(offset).cast_const() };
            let offset_value =
                unsafe { destination.offset_from(jump.location) } as i32 // Relative jump
                - mem::size_of::<i32>() as i32; // Jump from end of instruction
            unsafe { ptr::write_unaligned(jump.location.cast_mut().cast::<i32>(), offset_value); }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct UnsignedDiv32Magic {
    mul: u64,
    add_indicator: bool,
    shift: u32,
}

fn compute_unsigned_div32_magic(d: u32) -> UnsignedDiv32Magic {
    debug_assert!(d >= 3 && !d.is_power_of_two());
    let l = 31 - d.leading_zeros();
    let sh = 32 + l;
    let two_sh = 1u128 << sh;
    let d128 = d as u128;
    let e = d128 - (two_sh % d128);
    let x_max = u32::MAX - ((u32::MAX - (d - 1)) % d);
    if (x_max as u128) * e < two_sh {
        UnsignedDiv32Magic {
            mul: (two_sh / d128 + 1) as u64,
            add_indicator: false,
            shift: sh,
        }
    } else {
        let two_sh2 = 1u128 << (sh + 1);
        let m_full = two_sh2 / d128 + 1;
        UnsignedDiv32Magic {
            mul: (m_full - (1u128 << 32)) as u64,
            add_indicator: true,
            shift: l + 1,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct UnsignedDiv64Magic {
    magic: u64,
    add_indicator: bool,
    post_shift: u32,
}

fn compute_unsigned_div64_magic(d: u64) -> UnsignedDiv64Magic {
    debug_assert!((3..(1u64 << 63)).contains(&d) && !d.is_power_of_two());
    let l = 63 - d.leading_zeros();
    let two_sh = 1u128 << (64 + l);
    let d128 = d as u128;
    let e = d128 - (two_sh % d128);
    let x_max = u64::MAX - ((u64::MAX - (d - 1)) % d);
    if (x_max as u128) * e < two_sh {
        UnsignedDiv64Magic {
            magic: (two_sh / d128 + 1) as u64,
            add_indicator: false,
            post_shift: l,
        }
    } else {
        let two_sh2 = 1u128 << (64 + l + 1);
        let m_full = two_sh2 / d128 + 1;
        UnsignedDiv64Magic {
            magic: (m_full - (1u128 << 64)) as u64,
            add_indicator: true,
            post_shift: l,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unsigned_div32_magic_formula() {
        let divisors: &[u32] = &[
            3,
            5,
            6,
            7,
            9,
            10,
            11,
            12,
            13,
            14,
            15,
            17,
            19,
            21,
            25,
            31,
            35,
            63,
            100,
            127,
            255,
            641,
            1000,
            10_000,
            65_535,
            65_537,
            1_000_000_000,
            0x7fff_ffff,
            0x8000_0001,
            0x8000_0003,
            0xffff_fff9,
            0xffff_fffe,
            u32::MAX,
        ];
        for &d in divisors {
            let m = compute_unsigned_div32_magic(d);
            assert!(m.mul <= u32::MAX as u64);
            let mut dividends = vec![
                0u32,
                1,
                2,
                d - 1,
                d,
                d.saturating_add(1),
                u32::MAX - 1,
                u32::MAX,
                0x7fff_ffff,
                0x8000_0000,
            ];
            for k in [1u64, 2, 3, 7, 100, 65536] {
                if let Some(kd) = k.checked_mul(d as u64) {
                    if kd <= u32::MAX as u64 {
                        dividends.push(kd as u32);
                        dividends.push((kd - 1) as u32);
                        if kd < u32::MAX as u64 {
                            dividends.push((kd + 1) as u32);
                        }
                    }
                }
            }
            let max_mult = (u32::MAX / d) * d;
            dividends.push(max_mult);
            dividends.push(max_mult - 1);
            if max_mult < u32::MAX {
                dividends.push(max_mult + 1);
            }
            for x in dividends {
                let x64 = x as u64;
                let q = if !m.add_indicator {
                    x64.wrapping_mul(m.mul) >> m.shift
                } else {
                    let t = x64.wrapping_mul(m.mul) >> 32;
                    (x64 + t) >> m.shift
                };
                assert_eq!(
                    q,
                    (x / d) as u64,
                    "div32 failed for x={x:#x}, d={d:#x}, m={m:?}"
                );
                let r = x.wrapping_sub((q as u32).wrapping_mul(d));
                assert_eq!(r, x % d, "mod32 failed for x={x:#x}, d={d:#x}");
            }
        }
    }

    #[test]
    fn test_unsigned_div64_magic_formula() {
        let divisors: &[u64] = &[
            3,
            5,
            6,
            7,
            9,
            10,
            11,
            12,
            13,
            14,
            15,
            17,
            19,
            21,
            641,
            10_000,
            1_000_000_000,
            0x7fff_ffff,
            0x8000_0001,
            u32::MAX as u64,
            (1u64 << 32) + 1,
            (1u64 << 48) - 1,
            i64::MAX as u64,
            (1u64 << 63) - 3,
        ];
        for &d in divisors {
            let m = compute_unsigned_div64_magic(d);
            let mut dividends = vec![
                0u64,
                1,
                2,
                d - 1,
                d,
                d + 1,
                u32::MAX as u64,
                i64::MAX as u64,
                1u64 << 63,
                u64::MAX - 1,
                u64::MAX,
            ];
            let max_mult = (u64::MAX / d) * d;
            dividends.push(max_mult);
            dividends.push(max_mult - 1);
            if max_mult < u64::MAX {
                dividends.push(max_mult + 1);
            }
            for x in dividends {
                let t = ((x as u128 * m.magic as u128) >> 64) as u64;
                let q = if !m.add_indicator {
                    t >> m.post_shift
                } else {
                    (t + ((x - t) >> 1)) >> m.post_shift
                };
                assert_eq!(q, x / d, "div64 failed for x={x:#x}, d={d:#x}, m={m:?}");
                let r = x.wrapping_sub(q.wrapping_mul(d));
                assert_eq!(r, x % d, "mod64 failed for x={x:#x}, d={d:#x}");
            }
        }
    }

    #[test]
    fn test_x86_j4_instruction_bytes() {
        use crate::{
            program::{BuiltinProgram, FunctionRegistry, SBPFVersion},
            test_utils::TestContextObject,
        };
        use std::sync::Arc;

        let loader = Arc::new(BuiltinProgram::new_loader(Config {
            noop_instruction_rate: 0,
            ..Config::default()
        }));
        let exe = Executable::<TestContextObject>::from_text_bytes(
            &ebpf::Insn {
                opc: ebpf::EXIT,
                ..ebpf::Insn::default()
            }
            .to_array(),
            loader,
            SBPFVersion::V3,
            FunctionRegistry::default(),
        )
        .unwrap();
        let emit_one = |ins: X86Instruction| -> Vec<u8> {
            let mut jit = JitCompiler::<TestContextObject>::new(&exe).unwrap();
            ins.emit(&mut jit);
            jit.result.text_section()[..jit.offset_in_text_section].to_vec()
        };

        // cmp_immediate S16 / S32 / S64 with imm8 vs imm16/imm32
        assert_eq!(
            emit_one(X86Instruction::cmp_immediate(
                OperandSize::S16,
                RAX,
                -5,
                None
            )),
            vec![0x66, 0x83, 0xf8, 0xfb],
        );
        assert_eq!(
            emit_one(X86Instruction::cmp_immediate(
                OperandSize::S16,
                RAX,
                200,
                None
            )),
            vec![0x66, 0x81, 0xf8, 0xc8, 0x00],
        );
        assert_eq!(
            emit_one(X86Instruction::cmp_immediate(
                OperandSize::S64,
                R8,
                -128,
                None
            )),
            vec![0x49, 0x83, 0xf8, 0x80],
        );
        assert_eq!(
            emit_one(X86Instruction::cmp_immediate(
                OperandSize::S64,
                R8,
                128,
                None
            )),
            vec![0x49, 0x81, 0xf8, 0x80, 0x00, 0x00, 0x00],
        );

        // load_immediate(dst, 0) xor zero-idiom vs load_immediate_preserve_flags(dst, 0)
        assert_eq!(
            emit_one(X86Instruction::load_immediate(RAX, 0)),
            vec![0x31, 0xc0],
        );
        assert_eq!(
            emit_one(X86Instruction::load_immediate(R11, 0)),
            vec![0x45, 0x31, 0xdb],
        );
        assert_eq!(
            emit_one(X86Instruction::load_immediate_preserve_flags(R11, 0)),
            vec![0x41, 0xbb, 0x00, 0x00, 0x00, 0x00],
        );

        // imul_immediate 0x6B (imm8) and 0x69 (imm32) with distinct src and dst
        assert_eq!(
            emit_one(X86Instruction::imul_immediate(
                OperandSize::S64,
                R8,
                R11,
                -3
            )),
            vec![0x4d, 0x6b, 0xd8, 0xfd],
        );
        assert_eq!(
            emit_one(X86Instruction::imul_immediate(
                OperandSize::S32,
                RAX,
                RCX,
                1000
            )),
            vec![0x69, 0xc8, 0xe8, 0x03, 0x00, 0x00],
        );

        // BMI2 VEX3 shifts: SHLX, SHRX, SARX
        assert_eq!(
            emit_one(X86Instruction::bmi2_shift(
                OperandSize::S32,
                4,
                RAX,
                RSI,
                RDX
            )),
            vec![0xc4, 0xe2, 0x69, 0xf7, 0xc6], // shlx eax, esi, edx
        );
        assert_eq!(
            emit_one(X86Instruction::bmi2_shift(OperandSize::S64, 5, R8, R9, R10)),
            vec![0xc4, 0x42, 0xab, 0xf7, 0xc1], // shrx r8, r9, r10
        );
        assert_eq!(
            emit_one(X86Instruction::bmi2_shift(
                OperandSize::S64,
                7,
                RCX,
                RCX,
                R15
            )),
            vec![0xc4, 0xe2, 0x82, 0xf7, 0xc9], // sarx rcx, rcx, r15
        );
    }
}
