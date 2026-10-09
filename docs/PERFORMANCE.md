# Performance and correctness testing protocol

## No measured baseline is supplied

All numerical goals in DESIGN.md are targets. No Rust build, GUI launch, live swarm
or CPU benchmark ran in the authoring environment. `templartorrent-bench` is source
for repeatable microbenchmarks, not precomputed evidence. Store raw results rather
than copying optimistic numbers into a report.

## Existing microbench command

```sh
cargo run --release -p templartorrent-bench -- --pieces 100000 --iterations 500
```

The binary emits JSONL for `eligible_scalar`, `eligible_auto`, and
`reference_piece_pick`, with OS/architecture/backend and p50/p95/p99/max nanoseconds.
It uses deterministic synthetic arrays and excludes setup from measured operations.
Run 1k, 100k and 1m pieces, sparse/dense completion variants after adding that
parameter, and fast/slow CPUs. A synthetic bitmap improvement does not by itself
prove a better download startup or lower memory use.

The scalar and AVX2 kernel outputs must match at every length including zero,
unaligned sub-slices and non-multiples of four u64 words. A non-AVX2 VM/machine must
start the same shipping binary. Test the final tail masking at the scheduler boundary
(the SIMD word operation itself intentionally does not know the piece count).

## End-to-end agent harness to implement

A Rust harness starts controlled local peers and HTTP/UDP trackers, creates original
pseudo-random files/torrents from a seed, injects failure/timing and drives the same
engine command surface as the application. Make local-only networking the default;
assert no external socket destinations in those tests. The harness must hash-check
completed output independently. Use a separate optional interoperability stage with
an independent real peer implementation to avoid both ends sharing the same bug.

Keep networking loops real for transport measurements. Fake clocks are for scheduler
and state-machine tests; do not call fake-time wire results wall-clock throughput.
For CI, use bounded timeouts, reproducible data, owned temporary paths and fixed
maximum resource use. Destroy only those owned fixture paths after all handles close.

## Scenario matrix

| Scenario | Fixture and purpose | Evidence |
|---|---|---|
| Empty cold/warm launch | No jobs, 1080p, geo unloaded | First-present marker, RSS, idle CPU |
| Restored session | 100 inactive jobs / 3 active | Window before metadata parsing; no full eager scans |
| Magnet startup | Local tracker, metadata peer, complete seeder | Add->metadata->first byte->first verified piece |
| Ordered media | 1000 natural-named files, wire order shuffled | Promotion, unavailable first file, boundary correctness |
| Tight memory | 48 MiB payload cap, large pieces, slow disk | Internal high watermark, RSS, progress/no deadlock |
| Many peers | Global cap 128, IPv4/IPv6, duplicate candidates | Socket/task/queue cap and fair admission |
| Rate change | Multiple peers, 1/50/250/0 kB/s, loss/RTT | Global rate + documented burst/outstanding bytes |
| Removal race | Writes, reads and timer callbacks in progress | Quiescence, no post-delete writes, no escaping root |
| Resume crash | Kill between data flush and resume replacement | No false-complete bits; targeted recheck |
| Private metadata | Original trackers plus hostile PEX/DHT hints | Zero disallowed discovery after policy known |
| Peer fairness | Preferred slow peer, normal fast peer, rare supplier | Preference bounded; no rare-piece starvation |
| Visible UI stress | 10k files/128 peers, split drag/resize | Visible-row work only, p95 input-to-present |
| Minimized window | Active transfer, no animations | No 60-Hz UI loop; continued engine progress |
| Windows/Linux | Native builds, paste, magnet open, DPI | Functional parity + clean shutdown |

## Proposed metrics schema for the live harness

```json
{
  "schema": 1,
  "scenario": "local_magnet_startup",
  "commit": "actual commit",
  "rustc": "actual rustc -Vv",
  "target": "actual target triple",
  "cpu": "actual CPU and enabled runtime backend",
  "os": "actual OS/kernel/build",
  "seed": 1,
  "display_physical_pixels": [1920, 1080],
  "dpi_scale": 1.0,
  "first_present_ms": null,
  "metadata_ms": null,
  "first_payload_ms": null,
  "first_verified_piece_ms": null,
  "first_file_complete_ms": null,
  "peak_rss_bytes": null,
  "payload_high_watermark_bytes": null,
  "peak_open_files": null,
  "peak_peers": null,
  "idle_cpu_percent": null,
  "ui_input_p95_ms": null,
  "verified_goodput_bytes_s": null,
  "wasted_payload_bytes": null,
  "result": "not_run"
}
```

Null is missing measurement, not zero. Record hardware/power mode, storage,
allocator, release profile, lockfiles and display scale. Use multiple independent
runs and retain distributions. Separate filesystem-cache-warm from truly cold runs;
a fresh process is not necessarily a cold filesystem cache. Avoid global cache
flush commands on a developer's live machine; use an isolated fixture VM/device.

Memory has at least three measures: allocator-owned/retained bytes, budgeted payload,
and OS RSS/private working set. Read Linux process status/smaps or Windows process
memory counters through an instrumented Rust helper/OS bindings. The payload budget
is not an alternative to total RSS sampling. Include child processes if a test
harness uses any; report app and harness separately.

At 1920×1080 one u32 frame buffer is 8,294,400 bytes (7.91 MiB). At 3840×2160 it is
33,177,600 bytes (31.64 MiB). Account for all presentation/back buffers, not only the
application surface. Do not compare 4K RSS to a 1080p target without labeling it.

## Optimization acceptance

No correctness/privacy/safety regression. No hard-cap breach. Explain every material
memory regression. Require repeated whole-scenario improvement before integrating a
SIMD or scheduler change; keep microbench evidence too. Re-run non-AVX2 startup,
core oracle comparisons and piece-boundary tests after low-level changes. Public
swarm transfer rate is never the sole acceptance metric.
