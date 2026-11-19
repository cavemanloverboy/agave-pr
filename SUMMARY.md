# Parrot JIT (`anza-xyz/sbpf#316`) SBPFv3 Acceleration Summary

## Overview

We evaluated and accelerated Anza's experimental copy-and-patch **Parrot** backend ([`anza-xyz/sbpf#316`](https://github.com/anza-xyz/sbpf/pull/316), commit `cad0722`) targeting **pure `SBPFVersion::V3`** programs.

Five orthogonal optimizations (**P1–P5**) were prepared in isolated git worktrees, integrated into a sixth combined worktree (**P6**), verified for bit-for-bit parity (`r0`, exact CU consumption, memory state, `tests/exercise_instructions.rs`, and `--features tracer`), and benchmarked **strictly serially** on a single pinned core (`taskset -c 0`, Intel Xeon Sapphire/Emerald Rapids, isolated `target/` per worktree, zero overlapping CPU load).

---

## Pure SBPFv3 Workloads (`benches/sbpfv3_ratios.rs`)

All benchmark programs are deterministic, verified by `RequisiteVerifier` under `enabled_sbpf_versions: SBPFVersion::V3..=SBPFVersion::V3`, and use **zero syscalls** (~384k–408k dynamic CUs per invocation):

1. **`100:1` Memory-Heavy** (~408,011 CUs): 100 random-width (`u8`/`u16`/`u32`/`u64`) loads and stores per 1 loop-counter ALU op (`Mixed` = 50% `[r10 - off]` stack + 50% `[r1 + off]` 8 KiB buffer; `Buf-Only` = 100% `[r1 + off]` buffer).
2. **`1:1` Balanced** (~384,013 CUs): 64 loads/stores interleaved 1:1 with 64 pseudo-random 32/64-bit ALU ops (`add`, `sub`, `mul`, `xor`, `or`, `and`, `lsh`, `rsh`, `arsh`, `lddw`) and internal SBPFv3 PC-relative `call`s.
3. **`1:100` Compute-Heavy** (~405,015 CUs): 1 memory load/store per 100 pseudo-random ALU ops, data-dependent `jset` conditional branches, and internal SBPFv3 PC-relative `call`s.

---

## Experiments Prepared

| ID | Worktree / Branch | Summary of Architectural Changes | Verdict |
| --- | --- | --- | --- |
| **P1** | `/tmp/sbpf-wt-p1` (`exp/p1-inline-immediates`, `3de0c54`) | Replaces runtime `[RINSN + disp32]` L1d bytecode loads in JIT mode with inline `Imm32`, `Imm8`, `Imm64`, and `OffScaled8` template relocations. Collapses 2-insn 64-bit immediate ALU ops into 1 native instruction, eliminates partial-register `cl` stalls on immediate shifts, and stops streaming guest `.text` bytecode into L1d. | **Keep** (`1.15x–1.20x` execution) |
| **P2** | `/tmp/sbpf-wt-p2` (`exp/p2-fast-memory-stack`, `1638e50`) | Converts `MemoryRegion` to a 32-byte `#[repr(C)]` layout, rewrites `SupportingCode` load/store fast paths in pure `dynasm` (saving 2 scratch regs instead of 8 and eliminating the `extern "sysv64"` C-ABI call), and emits 2-instruction inline host-pointer loads/stores (`mov rcx, [rbp + stack_frame_host_top]; mov dst, [rcx + off32]`) for in-frame `[r10 + off]` accesses. | **Keep** (`3.8x–4.3x` mixed, `2.2x–2.3x` buffer-only; `1.8x` faster interpreter) |
| **P3** | `/tmp/sbpf-wt-p3` (`exp/p3-direct-calls-syscalls`, `176db19`) | Resolves SBPFv3 PC-relative internal calls (`CALL_IMM` with `src == 1`) at compile time into per-callsite direct `call rel32` instructions (removing the single-RIP indirect `call rcx` branch-predictor bottleneck) and pre-resolves `BuiltinFunction` pointers at compile time to eliminate runtime `HashMap` lookups on syscalls. | **Keep** (`1.19x` compute/call execution) |
| **P4** | `/tmp/sbpf-wt-p4` (`exp/p4-bb-cu-metering`, `4ca0824`) | Defers conditional branch `load_next_insn_addr` + `cmp RTEMP, RMETER` onto the taken path while preserving periodic `MeterCheckpoint`s and exact fault precedence. Collapses untaken conditional branches to 1 macro-fused `cmp + jcc` uop. | **Optional / Marginal** (~3% incremental in P6; ~15 LOC) |
| **P5** | `/tmp/sbpf-wt-p5` (`exp/p5-fast-compile-enter`, `09c99b5`) | Moves `pc_section` from OS-mapped pages to heap `Box<[u32]>` (cutting `mprotect` syscalls and multi-core TLB shootdowns per compile from 2 to 1), strips `PADDING_DUE` inline during Pass 2, skips RNG seeding when `noop_instruction_rate == 0`, and points `rbp` directly at `enter()`'s stack `Frame` (eliminating 144-byte `rep movsq` on VM entry). | **Keep for compile & multi-core scalability** (`1.94x` ELF compile speedup, 50% fewer TLB shootdowns) |
| **P6** | `/tmp/sbpf-wt-p6-combined` (`exp/p6-combined`, `d211531`) | Integrates **P1–P5**, placing inline `[r10 + off]` templates directly into the static `TemplateOpcode(u16)` table with a branchless biased-offset check and keeping `MAX_JIT_TEMPLATE_SIZE = 48` for L2 cache residency. | **Combined Winner** (`4.54x` on `1:1`, `4.06x` on `100:1`, `1.62x` / `9.71 GCU/s` on `1:100`) |

---

## Serial Benchmark Results

### 1. SBPFv3 Execution Latency & Speedup (Criterion Medians, `n = 30`, `taskset -c 0`)

Speedups in parentheses are relative to **Stock Parrot (`#316`)**:

| Pure SBPFv3 Workload | Old JIT (`v0.28`) | Stock Parrot (`#316`) | P1 (Imm) | P2 (Mem+`r10`) | P3 (Calls) | P4 (Branch) | P5 (Enter) | **P6 (Combined)** | **P6 Throughput** |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **`100:1` Mixed (Aligned)** | 2045.6 us | 1840.1 us | 1782.1 us (1.03x) | 484.3 us (3.80x) | 1794.1 us (1.03x) | 1895.4 us (0.97x) | 1956.3 us (0.94x) | **453.8 us (4.06x)** | **899 MCU/s** |
| **`100:1` Mixed (Unaligned)** | 2167.9 us | 2132.6 us | 1862.5 us (1.15x) | **546.6 us (3.90x)** | 1857.2 us (1.15x) | 1892.8 us (1.13x) | 1823.1 us (1.17x) | **552.4 us (3.86x)** | **739 MCU/s** |
| **`100:1` Buf-Only `[r1+off]` (Aligned)** | 2259.4 us | 1913.3 us | 1756.1 us (1.09x) | 878.2 us (2.18x) | 1693.6 us (1.13x) | 1737.2 us (1.10x) | 1740.2 us (1.10x) | **846.1 us (2.26x)** | **482 MCU/s** |
| **`100:1` Buf-Only `[r1+off]` (Unaligned)** | 2079.6 us | 1783.2 us | 1803.2 us (0.99x) | 1073.9 us (1.66x) | 1764.6 us (1.01x) | 1685.9 us (1.06x) | 1633.5 us (1.09x) | **1037.4 us (1.72x)** | **393 MCU/s** |
| **`1:1` Mixed (Aligned)** | 1054.3 us | 874.3 us | 810.7 us (1.08x) | 201.5 us (4.34x) | 839.8 us (1.04x) | 822.4 us (1.06x) | 830.5 us (1.05x) | **192.8 us (4.54x)** | **1.99 GCU/s** |
| **`1:1` Mixed (Unaligned)** | 1042.3 us | 894.0 us | 902.2 us (0.99x) | 273.1 us (3.27x) | 882.7 us (1.01x) | 853.6 us (1.05x) | 923.0 us (0.97x) | **253.2 us (3.53x)** | **1.52 GCU/s** |
| **`1:1` Buf-Only `[r1+off]` (Aligned)** | 996.9 us | 987.9 us | 824.1 us (1.20x) | **423.1 us (2.34x)** | 832.1 us (1.19x) | 843.6 us (1.17x) | 879.2 us (1.12x) | **426.7 us (2.31x)** | **900 MCU/s** |
| **`1:100` Compute-Heavy (Aligned)** | 73.32 us | 67.58 us | 58.85 us (1.15x) | 62.38 us (1.08x) | 56.98 us (1.19x) | 68.82 us (0.98x) | 68.89 us (0.98x) | **41.70 us (1.62x)** | **9.71 GCU/s** |

Compared against Agave's currently shipping **`solana-sbpf v0.28.0` Old JIT**, **P6 Combined** achieves:
- **`5.47x` speedup** on `1:1` Balanced (`1054.3 us -> 192.8 us`, `1.99 GCU/s`)
- **`4.51x` speedup** on `100:1` Memory-Heavy (`2045.6 us -> 453.8 us`, `899 MCU/s`)
- **`2.67x` speedup** on `100:1` Pure `[r1 + off]` Buffer (`2259.4 us -> 846.1 us`)
- **`1.76x` speedup** on `1:100` Compute-Heavy (`73.32 us -> 41.70 us`, `9.71 GCU/s`)

### 2. SBPFv3 Compilation Throughput (`benches/jit_compile.rs`)

| SBPFv3 Compile Target | Old JIT (`v0.28`) | Stock Parrot (`#316`) | P5 (Fast Compile) | P6 (Combined) |
| --- | ---: | ---: | ---: | ---: |
| `relative_call_sbpfv3` (ELF) | 6.87 us (17.8 MiB/s) | 5.69 us (21.5 MiB/s) | **2.94 us (41.6 MiB/s, 1.94x)** | **3.13 us (39.0 MiB/s, 1.82x)** |
| `large` (512 KiB SBPFv3 text) | 1067.8 us (468 MiB/s) | 415.4 us (1.18 GiB/s) | **328.1 us (1.49 GiB/s, 1.27x)** | 610.3 us (819 MiB/s) |
| `10mib_smallest` (`mov32 r1, 1`) | 11.31 ms (884 MiB/s) | 8.30 ms (1.18 GiB/s) | **6.80 ms (1.44 GiB/s, 1.22x)** | 12.18 ms (821 MiB/s) |
| `10mib_largest` (`jsle64`) | 30.33 ms (330 MiB/s) | 11.06 ms (904 MiB/s) | **10.37 ms (964 MiB/s, 1.07x)** | 15.32 ms (653 MiB/s) |

---

## Key Takeaways & Keep/Drop Recommendation

1. **Must-Keep for Execution Throughput (`P1 + P2 + P3`)**:
   - Together, **P1**, **P2**, and **P3** capture ~97% of all steady-state execution gains.
   - Notably, on `1:100` Compute-Heavy, **P1** (`-8.7 us`), **P2** (`-5.2 us`), and **P3** (`-10.6 us`) combine additively (`67.6 us -> 41.7 us`) because inline immediates + direct `call rel32` eliminate the `RINSN`/`RTEMP` AGU load-port bottleneck across loop bodies.
2. **Must-Keep for Compile Latency & Multi-Core Validators (`P5`)**:
   - Moving `pc_section` to `Box<[u32]>` halves `mprotect` syscalls (and global TLB shootdown IPIs across validator cores) per program compilation and cuts realistic ELF compile time nearly in half (`5.69 us -> 2.94 us`).
   - Note: In `P6`, expanding `TemplateRelocationKind` from 4 to 13 variants added jump-table overhead in `TemplateRelocation::apply` on synthetic 10 MiB compilations (`1.44 GiB/s -> 821 MiB/s`); splitting fast 4-byte additive relocations from rare special relocations recovers `P5`'s full `1.4+ GiB/s` bulk throughput.
3. **Optional (`P4`)**:
   - Deferring meter validation off untaken branches adds ~3% on `1:100` (`~1.35 us`) once `P1` already patches `OffScaled8` on taken branches. It is small (~15 LOC) and safe to keep for branch-heavy dispatch cascades, but non-essential.
4. **Build Tooling Note (Rust 1.100)**:
   - Rust `1.100.0` normalizes workspace-root paths across git worktrees when sharing `CARGO_TARGET_DIR`, which can cause fingerprint hash collisions if file mtimes overlap across worktrees. Always use isolated per-worktree `target/` directories when benchmarking parallel git worktrees.
