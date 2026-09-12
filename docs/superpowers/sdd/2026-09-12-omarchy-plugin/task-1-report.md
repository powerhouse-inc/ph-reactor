# Task 1 report — Machine layer `status --json`

**Done:**
- `src/cli.rs`: `Status` subcommand gains `--json` (documented as the
  stable machine-readable contract since 0.2.0).
- `src/daemon.rs`: the status handler resolves the shared
  `StatusSnapshot` — live from the daemon's `/api/status` when it is up,
  degraded from `config.json` when it is not (switchboard reported down,
  drives listed with their configured state, settings URL from the
  configured port) — and emits either the human text form or the
  snapshot as single-line `serde_json`.
- `Cargo.toml`: 0.1.0 → 0.2.0. The version bump is what makes
  "unsupported CLI" detectable by the plugin (minimum contract 0.2.0).
- README: "Stable CLI contract" section (shape, drive status vocabulary,
  token-env rule).

**Tests:** `cargo test` 45/45 (contract tests: degraded snapshot shape,
live snapshot shape, `--json` flag parsing). Binary verified by hand:
`status --json` single line, `--version` prints `ph-reactor 0.2.0`.
Zero build warnings.

**Deviations:** none.

**Commit:** 64b5082 on `feat/omarchy-plugin`.
