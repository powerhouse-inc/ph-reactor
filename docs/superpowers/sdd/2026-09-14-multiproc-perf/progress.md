# Multiproc & performance — Progress

Report: `../../2026-09-14-multiproc-perf.md`
Branch: `feat/multiproc` (worktree `~/.worktrees/ph-reactor-multiproc`)

| # | Task | Status | Commit |
|---|---|---|---|
| 1 | Profile the daemon run model (engine tick, store path, settings server) | done | — (investigation) |
| 2 | Store: re-publish local actions only (drop the remote-action re-gossip amplification) | done | `9c62e2f` |
| 3 | Store: `connect_outbound()` feed — local actions delivered on apply | done | `9c62e2f` |
| 4 | Engine: publish from the feed immediately; tick drain as deduped backstop; hoist tick summary | done | `0ae7b02` |
| 5 | Benchmark: `tests/multiproc_bench.rs` (store throughput + 2-node convergence), before/after | done | `da7633a` |
| 6 | Verification: clippy + full workspace tests; multi-process assessment in the report | done | `da7633a` |

- Convergence: 5,005 ms → 21.6 ms (2-node, release). Store throughput
  unchanged (~12k ops/s) — the store was never the bottleneck; the 5 s
  engine tick was.
- Multi-process split assessed and declined with evidence (see the
  report): per-vault process isolation already exists; the in-process
  work is short-burst CPU + async I/O, and the store's single-writer
  mutex means a second process buys no parallelism.
