# Task 7 report — Validation, README, preview, CI

**Done:**
- `README.md` (plugin repo): requirements, install, usage (bar widget
  glyphs, panel, autoStart), the CLI contract table, IPC target and
  methods, security section, validation/tests, demo, update/removal,
  license.
- `tests/run` (green on this machine, no Omarchy host): manifest
  validation via the toolkit's `validate_manifest.py` copy, 15
  `node --test` state-machine cases, JSON validity of every committed
  fixture, `bash -n` on `demo/run` **and** `demo/harness.sh` + both
  fixture bins, a **demo preflight** that executes the fixture CLI and
  asserts the exact contract the plugin consumes (clap-style version
  line, single-line `StatusSnapshot` JSON, `switchboard`/`drives`/
  `settings`/`updated_at` fields with `url` on every drive), and
  `omarchy plugin validate` (gated on an Omarchy CLI being present —
  the demo stub satisfies it in the harness environment).
- CI: `.github/workflows/test.yml` runs `./tests/run` on PRs and
  pushes to main (pinned action versions, `contents: read`).
- Preview: `preview.svg` toolkit placeholder, clearly labeled as such
  (final artwork deferred — a real bar/panel screenshot requires an
  Omarchy host).

**Verification:** `bash tests/run` → all checks pass, including the
stub `omarchy plugin validate` path (manifest + 15 node tests).

**Deviations:** the toolkit's `demo_preflight.py` is not available on
this machine, so the preflight's portable half (fixture execution +
contract assertions) is implemented in `tests/run`. Negative paths
verified to fail (wrong version format, missing `switchboard` key).
