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
3. **Add the service account key.** Open the key file you downloaded from
   Datum in any text editor, copy all of it, braces included, and paste it
   into **service_account_key** on the add-on's **Configuration** tab. Click
   **Save**. The project is read from the key, so there's nothing else to
   fill in. See below for why this must be a service account.
4. **Let Home Assistant accept proxied requests.** Home Assistant rejects
   requests that arrive through a proxy it does not trust, and every request
   through the tunnel does. Without this step the public address returns
   `400: Bad Request`. Go to **Settings → System → Network**, turn on
   **Use X-Forwarded-For**, and add `127.0.0.1` and `::1` as trusted proxies.
   If Home Assistant asks you to confirm the change, confirm it, or it reverts
   after a few minutes.

   Don't use an `http:` block in `configuration.yaml` for this. Current Home
   Assistant ignores it once the setting has moved into its own storage, and
   warns that it stops working altogether in 2027.2.
5. **Start.** Click **Start**. The add-on creates the tunnel on first start
   and logs its address:
   `Home Assistant is reachable at https://<name>.datumproxy.net`.
   Later restarts reuse the same tunnel and address.

Updates appear as an **Update** button on the add-on, like any other add-on.

## Configuration

| Option | What it is |
|---|---|
| `service_account_key` | The service account key JSON, pasted whole. Hidden in the UI and stored only in the add-on's private storage. |
| `project` | The Datum project the tunnel is created in. Leave empty to use the project the key belongs to. |
| `service_account_key_file` | Where to read the key from if `service_account_key` is empty. Defaults to `/share/datum-service-account.json`, for installs that already placed a file there. |
| `target` | What the tunnel points at. Leave empty for this Home Assistant: the add-on asks Home Assistant which port it uses. Must be plain HTTP. |
| `tunnel_label` | A name for the tunnel, to recognise it in the dashboard. The tunnel is found again by this name on every start, so changing it creates a new tunnel with a new address. |
| `log_level` | Daemon log verbosity. Leave at `info` unless diagnosing something. |

### Use a service account, not your own login

Create a service account in your Datum project, download its key JSON, and
paste the key into `service_account_key`.

The key is in your Home Assistant backups, whether pasted or placed in `/share`, so treat backups as
containing a credential. If one leaks, delete the key in Datum and create a
new one.

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

**The public address returns `502` with `upstream error: client error
(Connect)`.** The tunnel works, but nothing answered at `target`. If you set
`target` by hand, check the port. The address you use for Home Assistant on
your network shows it: no port in the address means port 80. To switch to a
different target, also change `tunnel_label`, because an existing tunnel keeps
the target it was created with.

**The public address returns `400: Bad Request`.** Home Assistant doesn't
trust the proxy yet. See step 4 of "Installing".

**Every request fails with "not set-up for reverse proxies", or Home Assistant
warns "HTTP YAML configuration is ignored after migration".** Home Assistant
2026.x stops reading the `http:` block in `configuration.yaml` once it has
moved that setting into its own storage, which it does on first boot. After
that, editing the YAML changes nothing, even though the config check still
passes. Remove the `http:` block and set the trusted proxy under **Settings →
System → Network**.

## Known limits

The watchdog in `config.yaml` restarts the add-on if the daemon's local API
stops answering. That proves the process is alive; it does **not** prove the
tunnel is serving. Those are separate failure modes, and detecting the second
one is tracked separately.
