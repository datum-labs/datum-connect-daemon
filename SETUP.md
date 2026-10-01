# Setup

Build-from-source instructions for the daemon and CLI plugin. This is the mechanical "get it building and running" part — see the top-level [README.md](./README.md) for what this project actually is and what's proven to work.

## What you're building

A Rust daemon (`datum-connect-daemon`) and a Go CLI plugin (`datumctl-connect`) that manage tunnels through a persistent local process instead of a one-shot CLI invocation. On top of that, this project adds a local API auth layer (setup/operate/viewer tokens), an L7 traffic inspector with a browser dashboard, and peer-to-peer tunnels over iroh.

## Prerequisites

**Windows (native, no WSL required):** `rustup` with the stable MSVC toolchain (`rustup-init.exe` from https://win.rustup.rs, plus Visual Studio Build Tools with the "Desktop development with C++" workload for the linker) and Go 1.25+ for Windows.

**WSL2 / Linux:** WSL2 with Ubuntu 24.04, or any Linux system with `rustup` (stable) and Go 1.25+. On Debian/Ubuntu, `sudo apt install build-essential pkg-config` if the Rust build complains about missing system libraries.

If using WSL2, check which distro is actually the default (`wsl -l -v`) before running anything — a non-Ubuntu default distro (e.g. `docker-desktop`, if Docker Desktop is installed) will silently pick up the wrong environment. Target explicitly with `wsl -d Ubuntu-24.04 -- <command>`, or fix the default once with `wsl --set-default Ubuntu-24.04`.

## Get the code

```bash
git clone https://github.com/datum-labs/datum-connect-daemon.git
```

This repo is a self-contained snapshot, not a set of patches against another checkout — it doesn't depend on a sibling clone of `datum-cloud/app` or `datum-cloud/datumctl`. `connect-plugin`'s `go.mod` resolves the real published `go.datum.net/datumctl` module directly.

## Build

WSL/Linux:

```bash
cd datum-connect-daemon/connect/connect-lib
cargo build -p datum-connect-daemon
# binary lands at target/debug/datum-connect-daemon

cd ../connect-plugin
go build -o datumctl-connect .
```

Native Windows (PowerShell), same repo, same commands otherwise:

```powershell
Set-Location datum-connect-daemon\connect\connect-lib
cargo build -p datum-connect-daemon
# binary lands at target\debug\datum-connect-daemon.exe

Set-Location ..\connect-plugin
go build -o datumctl-connect.exe .
```

If `cargo build` fails with a linker error on Windows, the MSVC Build Tools the `stable-x86_64-pc-windows-msvc` toolchain needs aren't installed — see Prerequisites above.

### Dashboard development

The browser dashboard is a React app built on [`@datum-cloud/datum-ui`](https://github.com/datum-cloud/datum-ui), in `connect/connect-lib/daemon/dashboard/`. Vite builds it into a single self-contained `dist/index.html` (JS, CSS and fonts inlined), which the daemon embeds with `include_str!`. That file is committed, so the `cargo build` above needs no JavaScript tooling.

To change the dashboard you need [Bun](https://bun.sh):

```bash
cd connect
task dev:dashboard     # Vite dev server; proxies /v1 to a daemon already running on :47780
task build:dashboard   # rebuild dist/index.html; commit it with your change
task check:dashboard   # typecheck + fail if the committed dist/ is stale (what CI runs)
```

Rebuild the daemon after `build:dashboard` to pick up the new bundle.

## Auth

The daemon requires a working Datum Cloud credentials helper to start at all, even for peer-to-peer tunnels that never touch Datum Cloud — this is a known limitation (see [README.md](./README.md#status)), not something you can configure around today.

You'll need a Datum Cloud account and the `datumctl` CLI installed and logged in (`datumctl auth login`) — see [Datum Cloud's docs](https://docs.datum.net) if you don't have one yet. Once logged in:

```bash
export DATUM_PLUGIN_MODE=1
export DATUM_SESSION="<your-email>@api.datum.net"     # the session key from ~/.datumctl/config's active-session field,
                                                          # not your plain email
export DATUM_CREDENTIALS_HELPER=/path/to/datumctl
export DATUM_PROJECT=<your-project-slug>                # required even for peer-only tunnels
export DATUM_CONNECT_DIR="$HOME/.datumctl/connect"       # where daemon state lives
```

Native Windows (PowerShell), pointing at a native `datumctl.exe`:

```powershell
$env:DATUM_PLUGIN_MODE = "1"
$env:DATUM_SESSION = ((Get-Content "$env:USERPROFILE\.datumctl\config" | Select-String "^active-session:").Line -replace "^active-session:\s*", "")
$env:DATUM_CREDENTIALS_HELPER = "C:\path\to\datumctl.exe"
$env:DATUM_PROJECT = "<your-project-slug>"
$env:DATUM_CONNECT_DIR = "$env:USERPROFILE\.datumctl\connect"
```

If you're running WSL2 and already have a Windows `datumctl` login, WSL can exec Windows `.exe`s directly — point `DATUM_CREDENTIALS_HELPER` at the Windows binary (e.g. `/mnt/c/path/to/datumctl.exe`) instead of logging in again inside WSL.

## Run it directly

WSL/Linux:

```bash
./connect/connect-lib/target/debug/datum-connect-daemon --port 47780 &
```

Once the daemon logs "listening":

```bash
CLI=./connect/connect-plugin/datumctl-connect
TOKEN=$(cat "$DATUM_CONNECT_DIR/daemon_auth/setup.token")   # auto-generated on first daemon start
$CLI --port 47780 --token "$TOKEN" tunnel api list
```

Open `http://127.0.0.1:47780/` (WSL2 forwards localhost to Windows automatically) for the dashboard — paste in the same setup token.

Native Windows (PowerShell):

```powershell
Start-Process ".\connect\connect-lib\target\debug\datum-connect-daemon.exe" -ArgumentList "--port","47780"
$CLI = ".\connect\connect-plugin\datumctl-connect.exe"
$TOKEN = (Get-Content "$env:DATUM_CONNECT_DIR\daemon_auth\setup.token" -Raw).Trim()
& $CLI --port 47780 --token $TOKEN tunnel api list
```

Open `http://127.0.0.1:47780/` directly (no forwarding needed) for the dashboard. You can confirm the token file's own permissions with `icacls "$env:DATUM_CONNECT_DIR\daemon_auth\setup.token"` — it should list only your own account.

On Windows, `tunnel daemon stop` goes straight to a hard kill rather than a graceful-then-kill sequence, since Windows doesn't support delivering SIGTERM to an arbitrary process.

## Run it through the real `datumctl` plugin path

The "as a real user would use it" path — WSL/Linux:

```bash
cp connect/connect-plugin/datumctl-connect ~/.local/bin/
cp connect/connect-lib/target/debug/datum-connect-daemon ~/.local/bin/
datumctl plugin trust connect   # required — datumctl blocks unmanaged plugin binaries
                                  # on PATH until explicitly trusted. Re-run this
                                  # after every rebuild; it fingerprints the binary
                                  # and re-blocks it if the file changes.
datumctl connect --project <project-slug> tunnel daemon start
datumctl connect tunnel daemon status
```

Native Windows should follow the same shape (copy both `.exe`s onto `PATH`, `datumctl plugin trust connect`, `datumctl connect ... tunnel daemon start`).

## Troubleshooting

- **`tunnel api start` returning `Error: ... context deadline exceeded`** doesn't necessarily mean the start failed — on a freshly-created tunnel (new listen key, connector being created for the first time) the daemon can take longer to respond than the CLI's own request timeout. Before assuming something's broken, run `tunnel api get <id>` or `tunnel api progress <id>` — if a hostname has been assigned and the proxy/connector steps are advancing, the start went through server-side even though the CLI gave up waiting on the response.
- **DNS propagation** for freshly-created tunnel hostnames (`*.datumproxy.net`) can take a few minutes — don't assume a `curl: Could not resolve host` after 30 seconds means something's broken. It's also normal for the `connector_metadata_programmed` progress step to stay `unknown` even once the tunnel is serving real traffic correctly; that field isn't blocking.
- **`--project` on `tunnel daemon start`** requires a reasonably current `datumctl connect` build — older checkouts had an env-var-override ordering bug that silently ignored the flag.
- **WSL2:** a process backgrounded inside a single `wsl.exe -- bash -lc "..."` invocation dies when that invocation ends, even with `&`. Use `setsid nohup <cmd> < /dev/null > log 2>&1 & disown` for anything that needs to outlive one command.
- **Windows:** `icacls`-based token permission checks read `%USERDOMAIN%\%USERNAME%` — if a permission-hardening step warns instead of silently succeeding, check those two environment variables are set as expected.

## Confirming it works end to end

```bash
$CLI --port 47780 --token "$TOKEN" tunnel api create --label test --endpoint 127.0.0.1:8000
$CLI --port 47780 --token "$TOKEN" tunnel api start <id-from-above>
$CLI --port 47780 --token "$TOKEN" tunnel api progress <id>   # wait for hostnames to appear
curl https://<hostname-from-progress>/
```

To try the peer-to-peer path instead (no Datum Cloud involved, no public hostname needed — just two daemon instances that can reach each other), see the "App-to-app (peer) tunnels" section of [API-REFERENCE.md](./API-REFERENCE.md).
