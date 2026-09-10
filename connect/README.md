# Datum Connect Plugin

A `datumctl` plugin (`datumctl connect tunnel listen ...`) that wraps the Rust
[`datum-connect`](connect-lib/) binary to manage Datum Connect tunnels.

## Architecture

```
datumctl connect tunnel listen ...
  │
  ▼
┌─────────────────────────────────────┐
│ Go supervisor  (datumctl-connect)   │  reads stdout for JSON events
│   connect-plugin/tunnel/listen/     │  forwards stderr to terminal
│   connect-plugin/internal/*         │
│                                     │
│   ┌─────────────────────────────┐   │
│   │ Rust binary  (datum-connect)│   │  stderr → user (progress, ✓ lines)
│   │   connect-lib/bin/src/      │   │  stdout → Go supervisor (JSON)
│   │   connect-lib/lib/          │   │
│   └─────────────────────────────┘   │
└─────────────────────────────────────┘
```

- **Go supervisor** (`connect-plugin/main.go`): datumctl plugin binary, parses
  tunnel-ready events from stdout, forwards signals (Ctrl+C), manages
  startup/grace timeout.
- **Rust binary** (`connect-lib/bin/`): headless tunnel agent driven by
  iroh + HTTPProxy APIs. Progress text goes to stderr; JSON lifecycle
  events go to stdout.
- **Rust library** (`connect-lib/lib/`): shared types, Kube API client,
  DatumCloud API bindings, heartbeat agent, tunnel service.

## Directory Layout

```
connect/
├── connect-plugin/          # Go plugin source
│   ├── main.go              # Plugin entrypoint
│   ├── tunnel/              # Cobra subcommands (listen, run, list, …)
│   │   └── listen/main.go   # Primary: spawns Rust binary, reads events
│   ├── internal/            # Go support packages
│   │   ├── binary/          # Rust binary discovery
│   │   ├── daemon/          # Background daemonisation
│   │   ├── env/             # Child environment builder (DATUM_SESSION, etc.)
│   │   ├── exec/            # Typed JSON message parser
│   │   ├── logfile/         # Log file management
│   │   ├── output/          # Formatted output (table/json/yaml)
│   │   ├── pidfile/         # PID tracking
│   │   ├── rbaccheck/       # Service-account RBAC validation
│   │   ├── signals/         # OS signal relay
│   │   ├── state/           # Daemon/run state persistence
│   │   ├── svcconfig/       # System service config builders
│   │   └── svcunit/         # systemd unit file generation
│   ├── e2e_test.go          # E2E tests (manifest, listen)
│   ├── e2e_interaction_test.go  # E2E tests (install, service, PID)
│   ├── go.mod / go.sum
│   ├── scripts/             # Build/release helpers
│   ├── testdata/            # Test fixtures
│   └── fake-datum-connect-test  # Test helper binary
├── connect-lib/             # Rust workspace
│   ├── Cargo.toml
│   ├── bin/                 # Binary crate (datum-connect)
│   │   └── src/
│   │       ├── main.rs      # Entrypoint, CLI, Listen handler
│   │       └── progress.rs  # Tunnel progress rendering (✓ / ○)
│   └── lib/                 # Library crate (connect-lib)
│       └── src/
│           ├── datum_cloud/  # API client, auth, env
│           ├── heartbeat.rs  # HeartbeatAgent
│           ├── tunnel.rs     # TunnelService
│           └── …
├── flake.nix                # Nix dev shell
├── Taskfile.yaml            # Build/test/install tasks
└── README.md
```

## Install

```bash
datumctl plugin install datum-cloud/connect
```

Downloads the pre-built archive from the [latest GitHub release](https://github.com/datum-cloud/connect/releases) and places both binaries in `~/.datumctl/plugins/`.

## Components

### connect-plugin — Go supervisor (`connect-plugin/`)

The datumctl plugin binary. Parses JSON events from the Rust subprocess's stdout, forwards signals, manages startup/grace timeout.

```bash
# Build (debug)
cd connect-plugin && go build -o datumctl-connect .

# Test
cd connect-plugin && go test -timeout 5m ./internal/...

# Install to ~/.datumctl/plugins/
cp connect-plugin/datumctl-connect ~/.datumctl/plugins/
```

Requires **Go ~1.25.8+** (see `connect-plugin/go.mod`).

### connect-lib — Rust library + binary (`connect-lib/`)

The tunnel agent. The `datum-connect` binary is a headless tunnel daemon driven by iroh + HTTPProxy APIs. It is also published as a **library crate** (`connect-lib/lib/`) exposing shared types, Kube API client, DatumCloud API bindings, heartbeat agent, and tunnel service — suitable for embedding in other clients such as [Datum Desktop](https://github.com/datum-cloud/app).

```bash
# Build binary (debug)
cd connect-lib && cargo build -p datum-connect

# Run unit tests across the workspace
cd connect-lib && cargo test

# Package crate for downstream use
cd connect-lib && cargo package -p connect-lib
```

Requires **Rust stable** (see `connect-lib/rust-toolchain.toml`).

## Developing

The canonical build and test commands are in `Taskfile.yaml`. Each task delegates to the underlying Go or Rust toolchain in the relevant subdirectory.

```bash
# Build both binaries (debug)
task build

# Release build with LTO + strip
task build:release

# Run all tests
task test

# Run Go or Rust tests individually
task test:go
task test:rust
```

Helper scripts are also available in `connect-plugin/scripts/`.

## Nix

A dev shell with Go, Rust, task, pkg-config, and openssl is available:

```bash
nix develop
```

The packaged Rust binary can also be built with Nix:

```bash
nix build
```

## Releases

Push a semver tag (`vX.Y.Z`); `.github/workflows/release.yml` cross-compiles `datum-connect` (Rust) via a matrix of OS runners, then runs GoReleaser to produce per-platform archives containing both `datumctl-connect` and `datum-connect`, plus `checksums.txt`.

The plugin is versioned independently of both `datumctl` and the `datum-connect` Rust binary. After a release, update the `Plugin` manifest at `datum-cloud/datumctl-plugins/index.yaml`.

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Two-binary architecture | Rust binary ships independently; Go supervisor handles dispatch, daemonisation, signal management. |
| `--json` mode always on | Go supervisor parses line-delimited JSON events from stdout; human text goes to stderr. |
| No `DATUM_ACCESS_TOKEN` in child env | Rust binary uses `DATUM_CREDENTIALS_HELPER` + `DATUM_SESSION` to exec helper for token. |
| Proxy verification retries indefinitely | Datum Cloud can take time to settle; user sees periodic `○ waiting for proxy …` messages every 10s. |
| State isolation | Plugin uses `~/.local/share/datumctl/connect/` — no OAuth files, no selected_context. |
