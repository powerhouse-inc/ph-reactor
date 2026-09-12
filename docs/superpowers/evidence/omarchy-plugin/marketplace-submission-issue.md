# Marketplace submission issue (prepared, NOT opened)

Target: `HANCORE-linux/omarchy-plugin-marketplace` (open only after
owner approval).

---

**Title:** `[Plugin]: ph-reactor`

**Category:** Developer Tools
**Tags:** `system`, `quickshell`

**Plugin:** `io.github.powerhouse-inc.ph-reactor-omarchy`
**Repository:** https://github.com/powerhouse-inc/ph-reactor-omarchy
**License:** MIT
**Author:** powerhouse-inc

## Summary

Status and drive control for the `ph-reactor` local switchboard daemon
(https://github.com/powerhouse-inc/ph-reactor). A status-bar widget with
a phase glyph (ready / starting / stopped / degraded / error), a
private popout panel for daemon start/stop and remote-drive management
(add, remove, pause, resume, resync), and a scoped IPC surface.

## Kinds

`service` + `bar-widget`

## Requirements

- Omarchy 4 with Quattro shell-plugin support.
- The `ph-reactor` CLI 0.2.0 or newer on `PATH` (snap / Homebrew /
  install script). The plugin shells out to it only; it installs and
  downloads nothing.

## Install

```bash
omarchy plugin add https://github.com/powerhouse-inc/ph-reactor-omarchy.git --enable
```

## Notes

- Read-only by default: with `autoStart` off (the default) the plugin
  only observes the daemon via `ph-reactor status --json` (10 s
  interval, bounded output, 10 s watchdog, one probe in flight).
- No privileged commands, no token values on any surface — drive
  tokens stay in the daemon's environment; only env-var names are
  passed.
- Removing the plugin never touches the daemon or `~/.ph/reactor`.

## Preview

`preview.svg` in the repository root (placeholder pending final
artwork).
