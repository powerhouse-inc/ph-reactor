# Task 8 report — Evidence + publication prep

**Done:**
- Marketplace submission issue drafted (title `[Plugin]: ph-reactor`,
  category `Developer Tools`, tags `system`/`quickshell`, install
  command, requirements, security notes) and saved at
  `../../evidence/omarchy-plugin/marketplace-submission-issue.md`.
  **Not opened** — opening requires owner approval.

**Evidence status:**
- Machine layer: commit 64b5082 (`feat/omarchy-plugin`), 45/45 cargo
  tests, binary-verified `status --json` / `--version` output.
- Plugin: portable checks green (`tests/run`); end-to-end fixture run
  through the real state machine (task 6 report).
- **Live-shell evidence: not claimed.** This machine has no Omarchy
  host and no Qt 6; the QML layer has not been executed. On the
  owner's Omarchy 4 machine: `bash demo/harness.sh`, interact, capture
  a screenshot per the evidence-record format (commands + Omarchy
  version + SHAs) before the marketplace issue is opened.

**Deviations:** none.
