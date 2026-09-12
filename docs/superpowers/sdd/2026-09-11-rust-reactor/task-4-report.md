# Task 4 report — Supervisor

**Done:** `supervisor.rs` + `logrotate.rs`. Spawns
`node <state>/switchboard/node_modules/@powerhousedao/switchboard/dist/index.mjs`
(cwd = switchboard dir), pipes stdio to `<state>/logs/switchboard.log`
(size-rotated 10 MB x 3). Health: `GET /health` every 5 s; 3 consecutive
failures or exit -> SIGTERM (5 s grace -> SIGKILL) -> restart with
backoff 1 s -> 300 s cap, reset after 5 min healthy; 5 consecutive
failed boots stop retrying (Error state). Config watch channel:
changes respawn the child with no backoff. `ShutdownSignal`: SIGTERM to
the child (10 s grace -> SIGKILL), then `Stopped` event.

**Fixes found during E2E:** (1) the SIGTERM/SIGINT arm of the daemon
loop previously only logged and kept spinning (the daemon ignored
signals until `stop` force-killed it) - both signals now trigger the
graceful shutdown; (2) a non-`.await`ed future in the resync path
(dropped remove call) - now awaited.

**Tests:** 7 (fake-node stub: restart on kill, backoff, crash-loop cap,
SIGTERM delivery + pidfile removal, log rotation). All green.
