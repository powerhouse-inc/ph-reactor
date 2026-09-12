# Task 9 report — Daemon lifecycle + CLI

**Done:** `daemon.rs` (+ `commands.rs`, `status.rs`). `run`
(foreground) / `run --daemonize` (forks **before** any tokio runtime —
forking inside a live runtime shares the epoll and corrupts both sides;
the child builds a fresh runtime; the parent waits on the ready file).
Single instance: flock on `<state>/run/lock` + pidfile + ready file
(removed on shutdown). SIGTERM/SIGINT -> graceful (tray unregisters,
child SIGTERM 10 s grace -> SIGKILL, files cleaned; no orphans —
verified by E2E). Command loop: single writer for tray/page/CLI
commands; drive commands wait (bounded) for the switchboard's MCP to
actually accept handshakes (`/health` can pass before MCP is mounted).
CLI: run/stop/status/drive(add|remove|list|pause|resume|resync,
name-or-index)/doctor/config(show|set)/logs(--follow|--switchboard).
`status` degrades to the config view when the daemon is down.

**Tests:** 10 (daemonize handshake on a temp dir, single-instance
refusal, stop removes files, status formatting, doctor check
structure, logs tail/follow, config set via CLI). All green.
