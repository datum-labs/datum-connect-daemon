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
3. **Let Home Assistant accept proxied requests.** Home Assistant rejects
   requests that arrive through a proxy it does not trust, and every request
   through the tunnel does. Without this step the public address returns
   `400: Bad Request`. Go to **Settings → System → Network**, turn on
   **Use X-Forwarded-For**, and add `127.0.0.1` and `::1` as trusted proxies.
   If Home Assistant asks you to confirm the change, confirm it, or it reverts
   after a few minutes.

   Don't use an `http:` block in `configuration.yaml` for this. Current Home
   Assistant ignores it once the setting has moved into its own storage, and
   warns that it stops working altogether in 2027.2.
4. **Optionally, choose the project.** If your Datum login can see more than
   one project, set `project` on the add-on's **Configuration** tab to the
   project's id (not its display name) and click **Save**. With only one
   project, leave it empty.
5. **Start, and approve.** Click **Start**, then open the **Log** tab. It
   says:

   `To connect this Home Assistant to Datum, open https://auth.datum.net/ui/v2/login/device?user_code=ABCD-EFGH and enter code ABCD-EFGH (expires in 5 minutes)`

   Open the link on any device, sign in to Datum as usual, check that the
   code matches, and approve. If the code expires first, the log shows a new
   one; the add-on keeps offering new codes for an hour. Within a few
   seconds of approving, the log says `Approved as <you>`,
   `Created service account ...`, `Granted access` and `Saved key`, then
   creates the tunnel and logs its address:
   `Home Assistant is reachable at https://<name>.datumproxy.net`.
   Later restarts reuse the same key, tunnel and address, with no approval.

The approval screen says **datumctl**. That is expected: until the add-on has
its own Datum login app, it borrows the public one that the `datumctl`
command line tool uses. Approving lets the add-on act as you for under a
minute, to make what is listed below; it never stores your login.

If the add-on's **Watchdog** switch is on, turn it off until pairing is done.
While the add-on waits for approval its daemon is not running yet, so the
watchdog can restart the add-on part way through, which replaces the code.

Updates appear as an **Update** button on the add-on, like any other add-on.

### What pairing creates

Approving creates three things in your Datum project, and nothing else:

- a **service account** named `home-assistant-<5 letters/digits>`, marked as
  created by this add-on, with the device's name;
- one **access grant** (a policy binding in your organization) giving that
  service account the `editor` role on the project, and on nothing else;
- one **key** for it, valid for a year. The add-on saves it in its private
  storage and uses it from then on. Your own login is discarded as soon as
  these three exist.

To approve, your login must be able to create service accounts in the project
and to grant access (an organization owner or editor can). If it can create
the service account but not grant access, the log says so, names the service
account, and stops. Ask an organization owner or editor to grant it the
`editor` role on the project, then restart the add-on and approve again. It
picks up the same service account rather than making another.

**To revoke access,** delete the service account in the Datum portal, under the
project's **Service accounts**. The add-on stops working until it is paired
again.

**To pair again** (for example after revoking, or to switch projects), turn on
`repair` on the Configuration tab and restart. The add-on forgets its key and
shows a new code. It doesn't delete the old service account; the log names it
so you can delete it in the portal. Turn `repair` off again afterwards, or
every restart pairs again.

**Backups.** The paired key is left out of Home Assistant backups, so a backup
file never contains it. The trade-off: after restoring a backup, the add-on
has no key and asks you to approve again, which creates a new service account.
Delete the old one in the portal.

## Configuration

| Option | What it is |
|---|---|
| `project` | The Datum project the tunnel is created in, by id. When pairing, leave it empty to use the only project your login can see. With your own key, leave it empty to use the project the key belongs to. |
| `target` | What the tunnel points at. Leave empty for this Home Assistant: the add-on asks Home Assistant which port it uses. Must be plain HTTP. Changing it repoints the existing tunnel on the next start; its address stays the same. |
| `tunnel_label` | A name for the tunnel, to recognise it in the dashboard. The tunnel is found again by this name on every start, including after the add-on is reinstalled, so changing it creates a new tunnel with a new address. |
| `repair` | Forget the paired key and pair again on the next start. See "What pairing creates". Leave off. |
| `service_account_key` | Your own service account key JSON, pasted whole, instead of pairing. See "Advanced: use your own service account key". |
| `service_account_key_file` | Where to read your own key from if `service_account_key` is empty. Defaults to `/share/datum-service-account.json`, for installs that already placed a file there. |
| `log_level` | Daemon log verbosity. Leave at `info` unless diagnosing something. |

## Advanced: use your own service account key

Instead of pairing, you can make the service account and its key yourself in
the Datum portal: create a service account in your project, give it access to
the project (it needs to create tunnels and their WAF and traffic policies;
`editor` does), create a key, and download the key JSON. Open the file in any
text editor, copy all of it, braces included, paste it into
`service_account_key` on the Configuration tab, and click **Save**. The project
is read from the key.

Which key the add-on uses, in order:

1. a pasted `service_account_key`;
2. a key file at `service_account_key_file`, if one is there;
3. the key saved by an earlier pairing;
4. otherwise, it pairs.

So a pasted key always wins over a paired one, and clearing the paste goes
back to the paired key, if there is one, without approving again.

A pasted key is part of the add-on's configuration, so it is in your Home
Assistant backups, as is a key file in `/share`. Treat those backups as
containing a credential. If one leaks, delete the key in Datum and create a
new one.

### Why a service account, not your own login

Pairing borrows your login only to make a service account; the add-on then
runs on that service account. It must be a service account. This is not a
style preference; both failure modes have been observed on a real daemon
within four days of each other. A personal session token was refused roughly
two and a half hours *before* the expiry it advertised, while the credentials
helper kept handing the daemon the same dead token — the tunnel served errors
for about 36 hours with every local signal reporting healthy. Two days later
the login expired outright and could only be restored by a human at a browser.

An appliance in a house with nobody watching the logs fails exactly those ways,
and the only visible symptom is that the hostname stops working.

A service account avoids both. The add-on's daemon reads the key itself,
signs a short-lived assertion with it, and exchanges that for an access
token. It does this again every time it needs a token, including straight
after one is refused, so there is no browser step, no helper process, and no
cached token to get stuck on. If the exchange fails, the log line says
`service account:` and why.

The service account also needs permission to create WAF policies (see below).
A paired one has it, through `editor`.

## Edge protection (WAF)

Every time it starts the tunnel, the add-on's daemon sets up Datum's web
application firewall for it: a policy named `<tunnel id>-waf` with the OWASP Core Rule Set at
paranoia level 1 and rule `920420` excluded. It is on (`Enforce`) for
everything except Home Assistant's streaming endpoints:

- live logs (the Supervisor's and each add-on's log, followed live)
- the event stream that keeps the Home Assistant UI up to date
- camera streams

Those are left out because, while a WAF covers a response, Datum's edge holds
back a streamed response until it ends, so live views never load. They still
need a Home Assistant login like every other page; only the firewall is
skipped. To do this, the tunnel sends those paths through their own rule,
and the policy covers only the tunnel's main rule, named `protected`. The
`Edge protection policy` log line names the policy.

The daemon only creates the policy when it is missing. If one already exists,
it is kept as is, so changes made in the portal survive restarts. Policies the
add-on creates carry the annotation
`connect.datum.net/managed-by: datum-connect-addon`. A policy without it is
never changed, with one exception: the switched-off policy add-on 0.1.5 or 0.1.6
created is switched on as above, once, and only if it is exactly as those versions
left it.

If the log says `!!! Edge protection is OFF: tunnel ... has no rule named
'protected'`, the policy covers nothing. Restart the add-on, and report it if
that persists.

Why these settings:

- **Rule `920420` is excluded** because Home Assistant's login page sends its
  JSON as `text/plain`, a content type that rule rejects, so login fails at
  every paranoia level. Attacks in `text/plain` bodies are still caught by the
  other rules.
- **Paranoia level 2 is not used yet.** It blocks any `{{ }}` template in a
  REST request, which breaks saving automations with template conditions.

If the log says `Edge protection NOT set up` or `Edge protection still OFF`,
the tunnel still works, but without the firewall. The usual cause is a service
account that may not create or update WAF policies. Grant that permission and
restart the add-on. A failure is never fatal and is retried every time the
tunnel starts, so one caused by a brief outage at boot fixes itself on the
next start.

## Request timeout

Datum's edge ends any response 15 seconds after the request by default. That
is too short for Home Assistant: live views such as an add-on's log stop
after 15-20 seconds, and so does any download that takes longer. Every time
it starts the tunnel, the add-on's daemon therefore raises the limit for it
to 1 hour, the most the platform allows, with a policy named
`<tunnel id>-timeout`. The log says
`Edge request timeout raised to 1h (policy ...)` when it creates the policy,
and `Edge request timeout set (existing policy ... kept)` after that.

As with the WAF, the policy is only created when it is missing. If one
already exists, it is kept as is, so a value changed since survives restarts.

If the log says `Edge request timeout NOT raised`, the tunnel still works, but
long streams and downloads are cut at about 15 seconds. The usual cause is a
service account that may not create traffic policies. Grant that permission
and restart the add-on. As with the WAF, it is retried on every start.

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

### Script check

CI (`.github/workflows/addon-script-check.yml`) runs shellcheck on the scripts
and compiles every jq program in them with the jq that ships in the base image
(1.6 on bookworm), because newer jq accepts things 1.6 rejects. To run the jq
part locally, use a 1.6 binary:
`JQ=/path/to/jq-1.6 addon/datum-connect/tests/check-jq.sh`.

## Diagnosing

The add-on's own log is the first place to look, but it cannot tell you
everything — in particular, a healthy-looking daemon can still have a tunnel
that serves nothing, because the local side and the cloud side fail
independently.

**Pairing stops with "can see N projects".** Your login can see several
projects. Set `project` to the id of one of those the log lists, then restart.

**Pairing stops with "can create the service account but can't grant it
access".** See "What pairing creates".

**Pairing says "Nobody approved a code within 60 minutes".** Restart the
add-on to get a new code.

The add-on mints its tokens inside the daemon, so a credential problem shows
up in the add-on's log as a `service account:` error: an unreadable or
malformed key at startup, or a refused token exchange later. A key that is
deleted or disabled in Datum is refused at the exchange, not silently kept.

**The public address returns `502` with `upstream error: client error
(Connect)`.** The tunnel works, but nothing answered at `target`. If you set
`target` by hand, check the port. The address you use for Home Assistant on
your network shows it: no port in the address means port 80. After changing
`target`, restart the add-on; the log then says which target the tunnel was
pointed at.

**The public address returns `400: Bad Request`.** Home Assistant doesn't
trust the proxy yet. See step 3 of "Installing".

**The log warns "Re-using tunnel ... its connector had to be replaced", and
the public address shows "Service offline".** The add-on found a tunnel from
an earlier install by its label, but the key that tunnel's connector was
registered under was deleted with `/data` when the add-on was uninstalled, so
the tunnel got a new connector. Such a tunnel has been seen to stay offline
even though everything on Datum's side reports ready. Set a new
`tunnel_label` on the Configuration tab and restart the add-on to get a fresh
tunnel, with a new public address. The old tunnel stays in the portal until
you delete it there.

**Every request fails with "not set-up for reverse proxies", or Home Assistant
warns "HTTP YAML configuration is ignored after migration".** Home Assistant
2026.x stops reading the `http:` block in `configuration.yaml` once it has
moved that setting into its own storage, which it does on first boot. After
that, editing the YAML changes nothing, even though the config check still
passes. Remove the `http:` block and set the trusted proxy under **Settings →
System → Network**.

## Known limits

**Streaming is limited to 1 hour.** Datum's edge allows a single response at
most an hour, even with the request timeout raised (see "Request timeout").
A live view, such as an add-on's log, left open longer than that is cut, and
reloading the page starts a new hour. The same applies to downloads.

The watchdog in `config.yaml` restarts the add-on if the daemon's local API
stops answering. That proves the process is alive; it does **not** prove the
tunnel is serving. Those are separate failure modes, and detecting the second
one is tracked separately.
