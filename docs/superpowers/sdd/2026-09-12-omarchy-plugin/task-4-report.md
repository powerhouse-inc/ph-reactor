# Task 4 report — Service.qml

**Done:** process-wide singleton (`kinds: service`):
- Resolves the external CLI by name on PATH (`cliName` property,
  default `ph-reactor`; the demo harness overrides PATH) and invokes it
  via `Quickshell.Io.PhProcess` with **argument arrays only** — no
  constructed command strings, no shell.
- Probe cycle: `--version` (re-probed whenever the last state suggests
  the CLI changed; cached otherwise) then `status --json`; result
  normalized through `ReactorState.reduce` into the published `state`
  object. One probe/action in flight at a time (`refresh()` rejects
  overlap); 10 s kill watchdog; stdout capped at 16 KB, stderr at 4 KB.
- Spawn-failure shortcut: a `PhProcess` error before any output
  completes the cycle as "command not found" (no watchdog wait).
- Actions: `start()` → `run --daemonize`; `stop()` → `stop`;
  `driveAdd(url, name, tokenEnv, offline)`;
  `driveRemove/Pause/Resume/Resync(name)` — each returns
  `issued:<label>` immediately, the outcome lands in `actionResult`
  (bounded) and the next refresh reflects it.
- `autoStart` property (default false, pushed by the bar widget from its
  inline `shell.json` setting): when true, the daemon is started once,
  if and only if the first observation is `stopped`. Never otherwise.
- `IpcHandler` (target = plugin id): `refresh`, `status` (bounded JSON,
  drives truncated to 10), `start`, `stop`, `driveAdd`,
  `driveRemove`, `drivePause`, `driveResume`, `driveResync`,
  `setAutoStart`.

**Security:** no `sudo`/`pkexec`, no downloads, no shared `/tmp` state,
no reading of `config.json` or token values — only the env var *name*
crosses the boundary (via `--token-env`); the CLI owns the token.

**Tests:** portable state-machine tests cover the normalization the
service feeds (task 3). QML itself cannot execute on this machine (no
Qt 6); the service's process-handling semantics mirror the tested
reducer inputs/outputs. QtTest deferred to CI (per spec).

**Deviations:** none beyond those recorded in the spec/progress
(OpenUrl absent in Omarchy's Commons; panel is `PopupCard`).
