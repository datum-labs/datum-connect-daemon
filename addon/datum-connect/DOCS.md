# Datum Connect

Reach this Home Assistant from anywhere through a Datum tunnel, with no inbound
ports opened on your router.

## Installing

This is a **local add-on**, which the Supervisor builds on the device. It does
not compile anything — see "Why the binary is not built here" below — but it
does need the daemon binary present before you install it.

1. Get the arm64 daemon binary. It is published as the
   `datum-connect-daemon-linux-arm64` artifact of the
   *Build add-on daemon (arm64)* workflow.
2. Copy this `datum-connect` folder to `/addons/` on the device, using the
   **Samba** or **Advanced SSH & Web Terminal** add-on.
3. Put the binary at `/addons/datum-connect/bin/datum-connect-daemon`.
4. In Home Assistant, go to **Settings → Add-ons → Add-on Store**, open the
   three-dot menu and choose **Check for updates**. The add-on appears under
   **Local add-ons**.

## Configuration

| Option | What it is |
|---|---|
| `project` | The Datum project the tunnel is created in. Required. |
| `service_account_key` | A service account credential. Required. |
| `target` | What the tunnel points at. Defaults to Home Assistant on this host. |
| `tunnel_label` | A name for the tunnel, to recognise it in the dashboard. |
| `log_level` | Daemon log verbosity. Leave at `info` unless diagnosing something. |

### Use a service account, not your own login

The credential must be a service account, not a token from your own
`datumctl` session. This is not a style preference. A personal session token
was observed being refused roughly two and a half hours *before* the expiry it
advertised, and the credentials helper — which decides whether to refresh
based on that expiry — kept handing the daemon the same dead token. The tunnel
served errors for about 36 hours while every local signal reported healthy.

An appliance sitting in a house with nobody watching the logs fails exactly
that way, and the only visible symptom is that the hostname stops working.

## Why the binary is not built here

Add-on `Dockerfile`s are normally free to build whatever they like, and the
obvious shape for this one is a multi-stage Rust build. That does not work.

The Supervisor builds a local add-on **on the device**. The Home Assistant
Green has 4GB of RAM and eMMC storage, and the daemon is 565 crates including
`aws-lc-sys`. The binary is therefore built in CI on a native arm64 runner
inside a bookworm container, so it links the same glibc the appliance has
(2.36), and the build fails if it ever links anything newer.

## Diagnosing

The add-on's own log is the first place to look, but it cannot tell you
everything — in particular, a healthy-looking daemon can still have a tunnel
that serves nothing, because the local side and the cloud side fail
independently.

`connect/scripts/auth-probe.py` in this repository checks the credential path
on its own and reports three stages separately: whether the helper can mint a
token, whether that token is fresh, and whether the control plane accepts it.
Those fail for different reasons and need different fixes, and an error from
the public hostname looks identical for all three.

## Known limits

The watchdog in `config.yaml` restarts the add-on if the daemon's local API
stops answering. That proves the process is alive; it does **not** prove the
tunnel is serving. Those are separate failure modes, and detecting the second
one is tracked separately.
