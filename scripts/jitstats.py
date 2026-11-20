#!/usr/bin/env python3
"""Summarize SBF execution cost from an agave-validator log.

Needs RUST_LOG=solana=info,agave=info,solana_ledger::blockstore_processor=trace,solana_program_runtime::invoke_context=trace
usage: jitstats.py validator.log [first_slot] [last_slot]
"""
import re
import sys

POINT = re.compile(r"datapoint: (replay-slot-stats|per_program_timings)\S* (.*)")

log = sys.argv[1]
lo = int(sys.argv[2]) if len(sys.argv) > 2 else 0
hi = int(sys.argv[3]) if len(sys.argv) > 3 else 1 << 62

replay, programs = {}, {}
with open(log, errors="replace") as f:
    for line in f:
        m = POINT.search(line)
        if not m:
            continue
        fields = dict(kv.split("=", 1) for kv in m.group(2).split() if "=" in kv)
        num = lambda k: int(fields.get(k, "0i").rstrip("i"))
        slot = num("slot")
        if not lo <= slot <= hi:
            continue
        if m.group(1) == "replay-slot-stats":
            replay[slot] = (
                num("execute_details_execute_inner_us"),
                num("execute_details_create_executor_jit_compile_us"),
                num("execute_us"),
                num("total_transactions"),
            )
        elif fields.get("pubkey") == '"all"':
            programs[slot] = (num("execute_us"), num("accumulated_units"))

slots = sorted(replay.keys() & programs.keys())
if not slots:
    sys.exit("no slots with both replay-slot-stats and per_program_timings; check RUST_LOG")
vm_us = sum(replay[s][0] for s in slots)
jit_us = sum(replay[s][1] for s in slots)
exec_us = sum(replay[s][2] for s in slots)
txs = sum(replay[s][3] for s in slots)
ix_us = sum(programs[s][0] for s in slots)
cus = sum(programs[s][1] for s in slots)
print(f"slots {slots[0]}..{slots[-1]} ({len(slots)} replayed), {txs:,} txs, {cus:,} CUs")
print(f"VM time per CU          {vm_us * 1000 / cus:8.2f} ns   (execute_details_execute_inner_us)")
print(f"instruction time per CU {ix_us * 1000 / cus:8.2f} ns   (per_program_timings execute_us)")
print(f"tx execute per slot     {exec_us / len(slots):8.0f} us   (execute_us, all threads)")
print(f"JIT compile per slot    {jit_us / len(slots):8.0f} us   (create_executor_jit_compile_us)")
