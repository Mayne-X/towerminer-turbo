# Changelog

## 0.3.0 — 2026-09-30 — first public release

- **Public release**, Apache-2.0, for Linux (x86-64, glibc 2.34+) and
  Windows 10/11 (x86-64).
- **No thermal guard in the default build**: no temperature sensor is needed
  and the miner never stops on heat by itself. The previous behaviour (guard
  on by default, sensor required, stop at 81 C, duty-cycle regulator) is the
  optional Linux build `--features fleet`.
- **Windows port**: scratchpads in large pages through `VirtualAlloc`
  (`MEM_LARGE_PAGES`, after enabling "Lock pages in memory"), normal pages
  otherwise with one clear warning; worker threads pinned with
  `SetThreadGroupAffinity` (processor groups, more than 64 CPUs); topology
  and cache sizes from `GetLogicalProcessorInformationEx`; foreign load for
  `--tune` from `GetSystemTimes`; `--tune` results under
  `%APPDATA%\towerminer`; HTTP over rustls (no OpenSSL anywhere).
- **CPU quota**: a Linux cgroup CPU limit (`cpu.max`, or v1
  `cfs_quota_us / cfs_period_us`, over the process's cgroup and its
  ancestors) caps the worker threads at `ceil(quota / period)`, spread one per
  core, and is printed at start-up. On rented machines 112 threads had been
  started for a quota of 13.4 CPUs.
- **Intel table** (family 6, previously "unknown CPU", 2 threads/core x
  1 pad), from `--tune` runs on 13 Intel machines: one thread per core
  (no SMT, or a quota that leaves no more workers than cores) -> 1 x 2 pads
  x prefetch (+51 % on a Core Ultra 9 285K, +8 % on a Xeon Gold 6430, the
  measured best of every quota-limited machine); SMT with L2 < 2 MiB ->
  2 x 1 pad, unchanged (best on i7-8700K and i7-11700F); SMT with
  L2 >= 2 MiB -> 2 x 2 pads x prefetch. AMD rows unchanged. The
  measurements are documented next to the table in `src/main.rs`.
- **`--threads N`** now means N logical CPUs, whole cores first; the profile
  applies inside them (it used to cap the worker count).
- **`--status-json`**: one JSON event per line on stdout (`profile`,
  `status` every 5 s, `block`, `error`), for front ends.
- **Clean stop** on Ctrl-C / SIGTERM / console close: exit 0 with totals.
- Transport errors now carry their cause ("connection refused", ...).
- `--version` shows `+fleet` for the thermal-guard build and `+mutant` for
  the gate's negative builds.

## 0.2.2 — 2026-09-30

- Submission: every solution on its own thread (at most 4 in flight), 45 s
  timeout (the node may hold a submit ~30 s while it finishes its proof), one
  attempt only; no answer = `unknown` (neither accepted nor refused).
- Template poll: 30 s timeout; exponential backoff only when the pool does
  not answer.
- Table: Zen 4/5 (L2 >= 1 MiB) 2 threads/core x 1 pad (+10 % on a
  7950X3D); Zen 2 2 x 1 x prefetch (+7 % on 2x EPYC 7742).
- The pipelined walk has a prefetch variant (gated).
- `--tune` measures the foreign CPU load before and during each candidate
  and stores nothing when it exceeds 5 % without `--tune-force`.

## 0.2 — 2026-09-28

- Fast kernel: at 1 pad, byte-offset addressing + one-add fill + pipelined
  fill anchors + 8-seed sponge batch; at >= 2 pads, grouped fold. Every lever
  gated end to end before it was timed (+4.8 % over 0.1 on a Ryzen 9 5950X,
  +37 % over the node's built-in search).
- Two policies, `--policy hashrate` and `--policy efficiency`; `--tune`
  measures both on the machine.
- Huge pages verified per worker, `--require-huge`.
- `--bench-walk`, `--check-nonces`, `--check-hardware`, release gate.

## 0.1 — 2026-09-26

- First TowerWalk engine: persistent pinned workers, several nonces walked in
  lockstep per thread, pads in 2 MiB pages, profile from the CPU family and
  L2 size, every solution re-checked by the reference walk.
