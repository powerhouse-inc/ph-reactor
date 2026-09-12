# Task 1 report — Crate scaffold, paths, config

**Done:** `apps/ph-reactor` crate (lib `ph_reactor` + bin `ph-reactor`,
edition 2021, rust-version 1.80). `paths.rs` resolves
`PH_REACTOR_STATE_DIR` > `$HOME/.ph/reactor` and creates the layout
(node/, switchboard/, logs/, run/). `config.rs`: `ReactorConfig` v1
with defaults (port 4001, `@powerhousedao/switchboard@latest`,
`https://registry.dev.vetra.io`, knowledge-note boot package, settings
127.0.0.1:4002, logLevel info), atomic save (tmp+rename, 0600),
corrupt-file recovery (moved aside, defaults written), dotted-key
`set()` with validation, unknown fields preserved.

**Tests:** 12 (defaults, corrupt recovery, atomic save, dotted-key
valid/invalid, paths resolution). All green.

**Deviations:** none.
