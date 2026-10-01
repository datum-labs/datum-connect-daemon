# Datum Connect

Reach this Home Assistant from anywhere through a Datum tunnel, with no inbound
ports opened on your router.

## Installing

Everything happens in the Home Assistant UI. Nothing is installed over SSH —
Home Assistant OS does not allow that — and nothing is built on the device.

[![Add this repository to your Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fdatum-labs%2Fdatum-connect-daemon)

1. **Add the repository.** Click the button above, or go to **Settings →
   Apps** (called **Add-ons** before Home Assistant renamed them), open the
   store, open the three-dot menu, choose
   **Repositories**, and add
   `https://github.com/datum-labs/datum-connect-daemon`.
2. **Install.** Find **Datum Connect** in the store and click **Install**.
   This downloads a prebuilt image.
3. **Add the service account key.** Put the key JSON from Datum at
   `/share/datum-service-account.json`. The **File Editor** or **Samba**
   add-on can do this from your browser or computer — see below for why this
   must be a service account.
4. **Configure.** On the add-on's **Configuration** tab, set `project` to your
   Datum project.
5. **Let Home Assistant accept proxied requests.** Home Assistant rejects
   requests that arrive through a proxy it does not trust, and every
   request through the tunnel does. Add `127.0.0.1` as a trusted proxy with
   `use_x_forwarded_for` on. If you add it with an `http:` block in
   `configuration.yaml` and requests still fail with "not set-up for reverse
   proxies", see "Diagnosing" below.
6. **Start.** Click **Start**. The add-on creates the tunnel on first start
   and logs its address:
   `Home Assistant is reachable at https://<name>.datumproxy.net`.
   Later restarts reuse the same tunnel and address.

Updates appear as an **Update** button on the add-on, like any other add-on.

## Configuration

| Option | What it is |
|---|---|
| `project` | The Datum project the tunnel is created in. Required. |
| `service_account_key_file` | Path to the Datum service account JSON. Defaults to `/share/datum-service-account.json`. Required. |
| `target` | What the tunnel points at. Defaults to Home Assistant on this host. |
| `tunnel_label` | A name for the tunnel, to recognise it in the dashboard. |
| `log_level` | Daemon log verbosity. Leave at `info` unless diagnosing something. |

### Use a service account, not your own login

Download the service account credential JSON from Datum and place it where the
add-on can read it — `/share` is reachable from the Samba and File Editor
add-ons. Then point `service_account_key_file` at it.

It must be a service account. This is not a style preference; both failure
modes have been observed on a real daemon within four days of each other. A
personal session token was refused roughly two and a half hours *before* the
expiry it advertised, while the credentials helper kept handing the daemon the
same dead token — the tunnel served errors for about 36 hours with every local
signal reporting healthy. Two days later the login expired outright and could
only be restored by a human at a browser.

An appliance in a house with nobody watching the logs fails exactly those ways,
and the only visible symptom is that the hostname stops working.

A service account avoids both. The add-on signs a short-lived assertion with
the key and exchanges it for an access token on every request the daemon makes
for one, so there is no browser step and no cached token to get stuck on.

## Why the image is prebuilt

Add-on `Dockerfile`s are normally free to build whatever they like, and the
obvious shape for this one is a multi-stage Rust build. That does not work.

Without a prebuilt image the Supervisor builds the add-on **on the device**.
The Home Assistant Green has 4GB of RAM and eMMC storage, and the daemon is 565
crates including `aws-lc-sys`. The binary is therefore built in CI on a native
arm64 runner inside a bookworm container, so it links the same glibc the
appliance has (2.36), and the build fails if it ever links anything newer. CI
then packages it into the image `config.yaml` points at.

### Developing locally instead

To test unpublished changes, copy this folder to `/addons/` on the device,
delete the `image:` line from `config.yaml`, put the
`datum-connect-daemon-linux-arm64` workflow artifact at
`bin/datum-connect-daemon`, then choose **Check for updates** in the add-on
store. It appears under **Local add-ons** and the Supervisor builds it from the
`Dockerfile`, which only copies files in.

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

**Every request fails with "not set-up for reverse proxies".** Home Assistant
2026.x stops reading the `http:` block in `configuration.yaml` once it has
migrated that setting into its own storage, which it does on first boot. Then
editing the YAML changes nothing, even though the config check still passes.
Set the trusted proxy in the UI instead, or delete `.storage/http` and restart
Home Assistant so it reads the YAML again.

## Known limits

The watchdog in `config.yaml` restarts the add-on if the daemon's local API
stops answering. That proves the process is alive; it does **not** prove the
tunnel is serving. Those are separate failure modes, and detecting the second
one is tracked separately.
