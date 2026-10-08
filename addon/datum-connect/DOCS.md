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
3. **Start, then open Datum Connect.** Start the add-on, then open
   **Datum Connect** in the sidebar. If it isn't there, turn on **Show in
   sidebar** on the add-on's Info tab, or click **Open Web UI** there.
   Opened that way, Home Assistant hides the add-on's tabs; the links at
   the top of the page (Info, Documentation, Configuration, Log) go back.
4. **Connect, approve, and pick a project.** Click **Connect to Datum**. The
   page shows a link and a code: open the link (it opens in a new tab), sign
   in to Datum as usual, check that the code there matches the one on the
   page, and approve. Back in Home Assistant, the page says who signed in
   and lists your projects, grouped by organization. Choose one (with only
   one, it is already chosen) and click **Continue**. The page shows each
   step as it is done: a service account, its access to the project, and
   its key. Then it says `Key saved. Starting the tunnel…` and, a few
   seconds later, moves on to step 5, or straight to the tunnel's status
   with its public address if Home Assistant is already set up for it.

   **A new address can take 10-20 minutes before it works in every
   browser.** On a real install, a brand-new address gave Firefox a `503`
   and Chrome "Unable to connect" for 15-20 minutes, while it already worked
   in other clients after about 8. It is the new address settling in, not a
   fault; later restarts reuse the same address, with no wait.

   If the code expires first, the page shows a new one; after 20 minutes
   without an approval it offers **Get a new code** instead. If something
   goes wrong, the page says what in plain words, with **Try again**.

   If you set `project` on the Configuration tab before starting, that
   project is preselected; you still click **Continue**.
5. **Last step: let Home Assistant accept connections through Datum.**
   Home Assistant rejects requests that arrive through a proxy it does not
   trust, and every request through the tunnel does: until this is done,
   the public address returns `400: Bad Request`. The page asks right after
   the key is saved: "Last step: let Home Assistant accept connections
   through Datum. This turns on 'Trust X-Forwarded-For' and adds 127.0.0.1
   and ::1 as trusted proxies, so Home Assistant still sees each visitor's
   real address. Home Assistant will restart." Click **Allow**. The add-on
   changes only those two settings and keeps every other network setting
   as it is. Home Assistant restarts: inside Home Assistant the page goes
   blank or says `Home Assistant is restarting with the new setting…` for a
   minute or two, and comes back by itself. The add-on then sends a request
   the way Datum does, and only confirms the change once Home Assistant
   accepts it. The page then says `Done ✓`. The tunnel is already running
   meanwhile; it starts whether or not you answer.

   If the check fails, the change is not confirmed and Home Assistant goes
   back to the previous setting by itself within 5 minutes. The page says
   why and offers **Retry** or **Skip**. If someone has a change to these
   settings waiting for confirmation, the page leaves it alone and asks you
   to finish it in **Settings → System → Network** first. If Home Assistant
   already trusts the add-on (set up by hand, or by an earlier version), the
   page doesn't ask.

   **Or by hand:** click **Skip: I'll set it up myself in Settings → System
   → Network**. Then go to **Settings → System → Network**, turn on **Use
   X-Forwarded-For**, and add `127.0.0.1` and `::1` as trusted proxies. If
   Home Assistant asks you to confirm the change, confirm it, or it reverts
   after a few minutes. Until then, the page's **Home Assistant proxy
   settings** row warns `Not set up: your public address returns 400 Bad
   Request until this is done`, with **Allow**.

   Don't use an `http:` block in `configuration.yaml` for this. Current Home
   Assistant ignores it once the setting has moved into its own storage, and
   warns that it stops working altogether in 2027.2.

The page is the easiest way, but not the only one:

- **The notification.** While the add-on is not connected, a notification
  (bell icon) links to the page. Once a code has been issued, it also holds
  the approval link and the code.
- **The log.** The same link and code are in the add-on's **Log** tab:

  `To connect this Home Assistant to Datum, open https://auth.datum.net/ui/v2/login/device?user_code=ABCD-EFGH and enter code ABCD-EFGH (expires in 5 minutes)`

  After approving, instead of choosing on the page, you can set `project`
  on the Configuration tab to one of the ids the log lists and click Save.
  Use the project's id, not its display name; stray spaces, quotes or
  capitals from copying it are ignored. Home Assistant then offers to
  restart the add-on. Either answer works: pairing continues without a new
  login. The log says `Continuing pairing as <you> (no new login needed)`
  after a restart. A restart more than 15 minutes after approving asks you
  to approve again.
- **Your own key.** See "Advanced: use your own service account key".

Either way, the log then says `Approved as <you>`,
`Created service account ...`, `Granted access` and `Saved key`, creates
the tunnel and logs its address:
`Home Assistant is reachable at https://<name>.datumproxy.net (a new address can take up to 20 minutes to work everywhere)`.
Later restarts reuse the same key, tunnel and address, with no approval.

The approval screen says **datumctl**. That is expected: until the add-on has
its own Datum login app, it borrows the public one that the `datumctl`
command line tool uses. Approving lets the add-on act as you only while it
pairs: from the approval until you choose a project (30 minutes at most),
then usually under a minute to make what is listed below. It never logs your
login. It never sends your login to the page either: the page only ever gets the
link, the code, your email, the project list and progress. While it waits for
you to choose a project, it keeps your login in
`/data/pairing-session.json`, readable by this add-on only and left out of
backups, so that the restart Home Assistant offers when you save does not
cost a second approval. That file is good for 15 minutes at most and is
deleted as soon as pairing ends, whether it worked or not.

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
the service account but not grant access, the page (and the log) says so and
names the service account. Ask an organization owner or editor to grant it
the `editor` role on the project, then click **Try again** and approve again.
It picks up the same service account rather than making another.

**To revoke access,** delete the service account in the Datum portal, under the
project's **Service accounts**. The add-on stops working until it is paired
again.

**To pair again** (for example after revoking, or to switch projects), click
**Re-pair** on the Datum Connect page. The add-on forgets its key, restarts,
and shows **Connect to Datum** again. **Unpair** does the same, but stops the
tunnel first, so the public address stops serving. Neither deletes the old
service account, which the add-on cannot do with its own key; the page and
the log name it so you can delete it in the portal. Both only work on a key
made by pairing: with your own key, they are off.

Without the page, turn on `repair` on the Configuration tab and restart: same
as Re-pair. Turn `repair` off again afterwards, or every restart pairs again.

**Backups.** The paired key, and your login while pairing waits for a
project, are left out of Home Assistant backups, so a backup file never
contains them. The trade-off: after restoring a backup, the add-on
has no key and asks you to approve again, which creates a new service account.
Delete the old one in the portal.

## Configuration

| Option | What it is |
|---|---|
| `project` | The Datum project the tunnel is created in, by id. Optional. When pairing, the Datum Connect page lists your projects and preselects this one, if set; without the page, set it after approving and click Save. With your own key, leave it empty to use the project the key belongs to. |
| `target` | What the tunnel points at. Leave empty for this Home Assistant: the add-on asks Home Assistant which port it uses. Must be plain HTTP. Changing it repoints the existing tunnel on the next start; its address stays the same. |
| `tunnel_label` | A name for the tunnel, to recognise it in the dashboard. The tunnel is found again by this name on every start, including after the add-on is reinstalled, so changing it creates a new tunnel with a new address. The add-on runs one tunnel: the old one is stopped, stays stopped, and is listed on the Datum Connect page under "Older tunnels from this Home Assistant", where **Remove** deletes it. |
| `repair` | Forget the paired key and pair again on the next start, for when the page's Re-pair can't be used. See "What pairing creates". Leave off. |
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

## The Datum Connect page

Once connected, the page shows the tunnel at the top: its public address,
whether it is online, its edge protection and request timeout (below). Below
it are Home Assistant's proxy settings (step 5 of "Installing", which comes
first until it is done or skipped), older tunnels, the project, and the
service account the add-on runs on. It
refreshes by itself. The note that a new address can take 10-20 minutes is
only shown for a tunnel created less than half an hour ago.

**Older tunnels.** The add-on runs exactly one tunnel, the one
`tunnel_label` names. Changing the label creates a new tunnel; any other
tunnel this Home Assistant made (one it still has local state for) is
stopped when the add-on starts, is never resumed, and is listed under
**Older tunnels from this Home Assistant**. The log says
`Stopped older tunnel '<label>' (<id>, <address>); remove it from the Datum
Connect page.` **Remove** asks for confirmation, then deletes that tunnel in
Datum: its public address (HTTPProxy), its ConnectorAdvertisement, its
connector (unless another tunnel still uses it), its `<id>-timeout` request
timeout policy, and its `<id>-waf` WAF policy if the add-on created it (one
without the add-on's `connect.datum.net/managed-by` annotation is kept), and
then the add-on's local state for it. Tunnels from other machines in the same
project are never listed, stopped or removed.

The page is served by the add-on through Home Assistant's ingress, so it
needs your Home Assistant login, and only administrators see it. The add-on
runs on the host's network, so it serves the page only on the address Home
Assistant's Supervisor connects to (`172.30.32.1`, not on your LAN) and
answers no one but the Supervisor (`172.30.32.2`); the log says so if it
refuses anything.

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

**Datum Connect isn't in the sidebar.** Turn on **Show in sidebar** on the
add-on's Info tab, or click **Open Web UI** there.

**The page says "Waiting for the add-on…".** The add-on is starting,
restarting, or stopped, or Home Assistant itself is restarting (the page
reaches you through Home Assistant). It comes back by itself. Check that it is running, and its Log tab. If the log
says `cannot serve the Datum Connect page`, the add-on pairs from the log
and notification instead, as before 0.3.0.

**The log says "Refused a request to the Datum Connect page from ...".**
Something other than the Supervisor tried to open the page, or the
Supervisor connects from an unusual address on this system. If the page
itself does not load, report the address the log names.

**Datum's approval page says "Something went wrong. Please try again."** The
usual cause is a stale Datum sign-in in that browser: Authorize fails even
with a fresh code. Open the link in a private window or another browser, sign
in to Datum there, and click Authorize. Signing out of auth.datum.net in your
usual browser fixes it for next time. If the code has expired in the
meantime, click **Get a new code** on the Datum Connect page (0.3.1 and
later).

**The page says "This page is out of date".** The add-on restarted since the
page was opened. Reload the page.

**The public address doesn't load yet.** If it is new, wait: a new address
can take 10-20 minutes before it works in every browser (see step 4 of
"Installing"). The page shows that note only while the tunnel is less than
half an hour old.

**No notification appears.** Open the page instead, or use the link and code
in the add-on's log. A log line starting `Could not show the pairing link as
a Home Assistant notification` says why the notification failed. Pairing
works the same either way.

**The log says "isn't one of your projects"** (pairing from the log, without the page). The `project` you saved is not
an id your login can see. Set it to one of the ids listed, and click Save.
Pairing is still waiting, so you can restart or not, as Home Assistant
offers; either way there is no new login.

**The log says "Pairing paused".** The add-on was stopped or restarted while
pairing waited for a project. It continues when the add-on starts again,
without a new login, as long as that is within 15 minutes of approving.

**Pairing says "No project was chosen in time" or "Choosing a project took
too long".** Nobody chose one within 30 minutes of approving, and the
approval is no longer good. Click **Get a new code** on the page, or restart
the add-on, and approve again.

**Pairing stops with "can create the service account but can't grant it
access".** See "What pairing creates".

**The page says "The code expired".** Nobody approved a code within 20
minutes. Click **Get a new code**. (Pairing from the log without the page
waits 60 minutes, then says `Nobody approved a code within 60 minutes`;
restart the add-on to get a new code.)

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
trust the proxy yet: the last step of setting up was skipped or did not
finish. Click **Allow** on the Datum Connect page (under **Home Assistant
proxy settings**), or see step 5 of "Installing". The log says so at every
start while it is missing:
`Home Assistant does not accept connections through Datum yet`.

**Allow says "still got 400: Bad Request" or "did not come back with the new
setting".** The change was not confirmed, and Home Assistant goes back to
the previous setting by itself within 5 minutes; nothing else changed. Click
**Retry** (or **Try again**) once Home Assistant is back, or set it by hand
(step 5 of "Installing"). If the page says a change is waiting for confirmation, finish
or discard it in **Settings → System → Network** first.

**The log says "Re-using tunnel ... from an earlier install: its connector was
replaced".** The add-on found a tunnel from an earlier install by its label,
but the key that tunnel's connector was registered under was deleted with
`/data` when the add-on was uninstalled, so the tunnel got a new connector.
Its public address may show "Service offline" for a few minutes; on a real
device it came back within about 4 minutes. If it's still offline after 20
minutes, restart the add-on; if that doesn't help, set a new `tunnel_label`
on the Configuration tab and restart, to get a fresh tunnel with a new public
address. The old tunnel stays in the portal until you delete it there (the
Datum Connect page cannot remove it: this install has no local state for
it).

**Two tunnels after changing `tunnel_label`.** Since 0.3.3 the old one is
stopped at every start and listed under "Older tunnels from this Home
Assistant" on the Datum Connect page; click **Remove** to delete it. Before
0.3.3 both stayed online.

**A pairing notification stays after connecting.** The add-on dismisses it
when pairing succeeds and again every time it starts, so restarting the
add-on clears it. Otherwise, dismiss it by hand.

**Every request fails with "not set-up for reverse proxies", or Home Assistant
warns "HTTP YAML configuration is ignored after migration".** Home Assistant
2026.x stops reading the `http:` block in `configuration.yaml` once it has
moved that setting into its own storage, which it does on first boot. After
that, editing the YAML changes nothing, even though the config check still
passes. Remove the `http:` block and click **Allow** on the Datum Connect
page, or set the trusted proxy under **Settings → System → Network**.

## Known limits

**Streaming is limited to 1 hour.** Datum's edge allows a single response at
most an hour, even with the request timeout raised (see "Request timeout").
A live view, such as an add-on's log, left open longer than that is cut, and
reloading the page starts a new hour. The same applies to downloads.

The watchdog (off by default; turn it on on the Info tab) restarts the add-on
if the Datum Connect page's port stops answering. Before 0.3.0 it watched the
daemon's API port, which listens on `127.0.0.1` only, while the Supervisor's
watchdog connects to `172.30.32.1`, so with the watchdog on it could never
connect. That proves the process is alive; it does **not** prove the tunnel
is serving. Those are separate failure modes, and detecting the second one is
tracked separately.
