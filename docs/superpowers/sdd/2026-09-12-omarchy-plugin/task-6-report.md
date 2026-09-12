# Task 6 report — Reversible demo harness

**Done:** `demo/harness.sh` — one command:
- Prepends `demo/bin` to PATH with two fixtures:
  - `ph-reactor` — emulates the CLI contract: `--version`
    (`ph-reactor 0.2.3`), `status --json` (the exact Rust
    `StatusSnapshot` shape: `switchboard`/`drives`/`settings`/
    `updated_at`; fictional `switchboard.example` drives, one synced,
    one paused; a `VETRA_TOKEN` env-var *name* only), and accepts
    `run`/`stop`/`drive` actions.
  - `omarchy` — headless stub of the CLI (`shell add/remove/list`,
    `plugin validate` runs the portable checks).
- Exports `OMARCHY_PATH` and `OMARCHY_SHELL_IPC_TIMEOUT`.
- If a real Omarchy binary exists outside the stubs: enables the plugin
  with the official `omarchy shell add <plugin dir>`, waits for the
  user, then disables it again on exit. Otherwise prints the exact
  headless commands (`ph-reactor --version`, `ph-reactor status --json`,
  `bash tests/run`) and the on-machine recipe.

**Verification (headless, this machine):**
- `bash demo/harness.sh` runs clean (env + instructions).
- End-to-end data path: fixture `--version` + `status --json` output
  piped through `js/ReactorState.js` exactly as `Service.qml` does →
  phase `ready`, message "Reactor running — 2 drives syncing", daemon
  fields and drive URLs populated; unknown command → `error` with the
  bounded CLI message.

**Deviations:** the spec's original `demo/run` (shell.json backup/
restore harness) was simplified to the official `omarchy shell
add/remove` flow — the plugin's bar entry is added by Omarchy itself,
so no hand-edited shell surgery is needed; the generator's `demo/run`
(quick fixture validity check) remains as the CI path.
