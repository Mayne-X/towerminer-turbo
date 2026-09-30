# towerminer — TowerWalk CPU miner for Jetsam (v1.4)

Private. Fork of `jetsam-extminer`: same wire protocol (getBlockTemplate /
submitBlock, Bearer key, pool `nonce_prefix` in bits 96..128, per-process
offset in bits 64..96), new engine for the cache-resident walk.

## v0.2.2 (2026-09-30)

- **Submission**: every solution on its own thread (at most 4 in flight),
  45 s timeout (the node may hold a submit ~30 s while it finishes its proof),
  one attempt only; no answer = `unknown` (neither accepted nor refused).
  Report line: `found / accepted / refused / unknown`.
- **Template poll**: 30 s timeout; exponential backoff only when the pool
  does not answer; an answered error retries after `--poll-ms`.
- **Table**: Zen 4/5 (L2 >= 1 MiB) 2 threads/core x 1 pad (+10 % on a
  7950X3D); Zen 2 2 x 1 x prefetch (+7 % on 2x EPYC 7742) [MEASURED
  2026-09-30]. Zen 3 unchanged.
- **Pipe + prefetch**: the pipelined walk now has a prefetch variant, so
  `--pads 1 --prefetch 1` keeps the pipe (gated: both variants, 2M and 4K).
- **--tune**: foreign CPU load measured before and during each candidate;
  above 5 % the candidate is `noisy` and nothing is stored without
  `--tune-force` (`foreign_busy`, `cv`, `noisy` in tune.json v3).
- Worker pinning failures are logged; the gate threads run at nice +10 under
  the thermal watchdog and the gate prints the guard's longest reading gap.

## v0.2 (2026-09-28)

- **Fast kernel** (`--kernel auto` = fast): at 1 pad, byte-offset addressing
  + one-add fill + pipelined fill anchors + 8-seed sponge batch; at >= 2 pads,
  grouped fold. Every lever gated end to end before it was timed.
- **Two policies**: `--policy hashrate` (default) and `--policy efficiency`
  (most hashes per joule; on Zen 3: 2 threads/core x 2 pads x prefetch).
  `--tune N` measures both on the machine and stores them (`tune.json` v2,
  keyed to the binary's sha256, the CPU set and the THP mode).
- **Thermal guard on its own thread**: reads every 250 ms (50 ms while the
  regulator runs), stops at 81 C (never above 82), predictive on a sustained
  slope; refuses to start without a sensor (`--no-thermal-guard` to force);
  workers stop hashing if the guard is starved.
- **Duty-cycle regulator** instead of pause/resume: all workers hash during the
  first `d x 1 s` of each second, `d` from a PI controller on Tctl
  (`--temp-target`, default 74 C), start-up ramp from 0.3 (`--ramp-secs 10`).
- **Huge pages verified per worker** (`/proc/self/smaps`), `--require-huge`.
- **Solutions submitted at once** from a dedicated thread with its own HTTP
  client (5 s timeouts); the template id is refreshed when the pool re-serves
  the same content.
- `--bench-walk` reports H/s, package W (RAPL), H/J, Tctl/Tccd, clock;
  `--check-nonces`, `--check-hardware`, `scripts/build.sh` (release gate).

## Measured on the lab box (Ryzen 9 5950X, Zen 3, 16c/32t), 2026-09-28

One ABAB series, 20 s x 5, median, package power from RAPL; binary sha256
767336f1… (the release). Raw files: `/root/towerminer-v02/raw2/e8-final.tsv`.

| config | H/s | W | H/J | Tctl max | vs node | vs v0.1 |
|---|---:|---:|---:|---:|---:|---:|
| node search path (rayon, 32 threads, 4K pages) | 12 295 | 127.6 | 96.4 | 59.6 | — | — |
| v0.1 (1 thread/core x 1 pad) | 16 117 | 138.4 | 116.5 | 65.1 | +31.1 % | — |
| v0.2 `--policy hashrate` (1 x 1, fast, pipe, ring 8) | 16 892 | 136.9 | 123.4 | 63.2 | +37.4 % | +4.8 % |
| v0.2 `--policy efficiency` (2 x 2, prefetch, grouped fold) | 16 378 | 115.8 | 141.4 | 59.6 | +33.2 % | +1.6 %, H/J +21.4 % |

Same thermal budget, 180 s x 3 from < 50 C (`e8-r4.tsv`): v0.1 pause 58 /
resume 53 C vs v0.2 regulator at 56 C.

| config | H/s | W | H/J | Tctl max | Tctl mean (60-180 s) |
|---|---:|---:|---:|---:|---:|
| v0.1 pause/resume | 6 482 | 75.2 | 88.6 | 60.4 | 55.6 |
| v0.2 regulator, v0.1 kernel | 7 412 (+14 %) | 86.2 | 86.0 | 63.9 | 55.3 |
| v0.2 `--policy hashrate` | 8 649 (+33 %) | 91.5 | 94.6 | 62.8 | 56.1 |
| v0.2 `--policy efficiency` | 16 494 (+154 %) | 116.4 | 141.7 | 56.0 | 54.9 |

On a machine that runs hot, `--policy efficiency` is the first lever; the
regulator holds the target when even that is too hot. Bench runs start at
full duty (peaks above target at the start); mining starts with the ramp.

Earlier fleet numbers (v0.1, vs the node, 2026-09-26): EPYC 7742 +24 %,
5950X +35 %, 7950X3D +42 %, 7900X +43 % per core, 9950X3D +28 % (one CCD).
The v0.2 kernel gains are measured on Zen 3 only; on Zen 2/4/5 they are
derived from the same mechanism — run `--tune` on the machine.

## Safety

- `--gate`: 256 golden vectors end to end + random seeds vs the vendored
  reference walk, for every kernel shape (base/fast x pads 1..4 x prefetch x
  2M/4K) and the pipelined chain. Mutant builds (`--features
  mutant-xorshift28` / `mutant-lane-order`) must FAIL it.
- `--check-nonces N`: every solution recomputed from its nonce (seed ring,
  pipelined anchors), duplicates and region checked; submit latency printed.
- Start-up self-test on the exact profile (exit 2), every solution
  re-checked by the reference walk before submit, sentinel 1/4096 (exit 3).
- Exit codes: 1 fatal, 2 self-test, 3 divergence, 4 huge pages required and
  absent, 5 no temperature sensor, 82 thermal.

## Usage

    TOWERMINER_KEY=<pool key> towerminer --rpc http://<pool>:9512 [--cpus LIST]
    towerminer --policy efficiency ...        # coolest, most H/J
    towerminer --temp-target 70 ...           # regulate lower
    towerminer --tune 10                      # once per machine, quiet window
    towerminer --bench-walk 20 [--bench-regulate]
    towerminer --gate ; towerminer --check-nonces 5 ; towerminer --check-hardware
    scripts/build.sh                          # release build + release gate

Vendored sources: `vendor/` = `git archive ea67571` of jetsam
(`vendor/SOURCE_COMMIT`, `VENDOR.sha256`), never modified.
