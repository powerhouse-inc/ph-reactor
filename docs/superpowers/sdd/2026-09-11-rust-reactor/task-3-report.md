# Task 3 report — Switchboard bootstrap

**Done:** `bootstrap/switchboard.rs`. Stub `package.json` makes
`<state>/switchboard` the npm install root;
`npm install <spec> --registry <registry> --no-audit --no-fund`
(10 min timeout) using the npm next to the resolved node.
`.ph-reactor-meta.json` (spec, resolved version from the installed
package.json, install time) is the idempotency marker.
`write_config` (atomic) emits the switchboard's `powerhouse.config.json`
(logLevel, switchboard.port, database, packageRegistryUrl, packages) —
this is what makes missing document model packages install
automatically from `registry.dev.vetra.io` (Connect's
`HttpPackageLoader` path). `spawn_env`: PORT/PH_SWITCHBOARD_PORT/
LOG_LEVEL/NODE_ENV. `health()` probes `GET /health`.

**Tests:** 6 (meta idempotency, spec-change reinstall trigger, config
generation byte-exact, no-rewrite when unchanged, env, health). All
green.

**Deviations:** database.url is the switchboard's built-in PGlite
default under its working directory (`.ph/reactor-storage`), not a
daemon-managed `<state>/data` dir; `DYNAMIC_MODEL_LOADING` is not set
(the generated config's packageRegistryUrl covers boot packages and
the switchboard default handles on-demand loading — verified in logs).
