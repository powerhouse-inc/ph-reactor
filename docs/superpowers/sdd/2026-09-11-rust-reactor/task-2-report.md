# Task 2 report — Node bootstrap

**Done:** `bootstrap/node.rs`. Probes `node --version` (5 s timeout)
across PATH, `~/.local/bin`, `/usr/local/bin`, `/usr/bin`; semver gate
(`version_at_least`, prerelease-aware: `v24.0.0-rc.1 < 24`). Private
runtime: pinned `v24.11.1` from nodejs.org (arch from the binary:
x64/arm64), sha256 verified against the matching line of
`SHASUMS256.txt`, extracted (flate2+tar) with the top-level dir
flattened into `<state>/node/node-<ver>-linux-<arch>/`. Marker file
re-verified on every start (self-heals). 3 download attempts, 5 min
timeout each.

**Tests:** 8 (semver gate incl. prerelease, SHASUMS line parsing incl.
non-matching entries, arch mapping, marker recovery). All green.

**Live evidence:** clean state dir downloaded + verified v24.11.1 in
11 s (see evidence file).
