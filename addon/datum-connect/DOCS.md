# Datum Connect

Reach this Home Assistant from anywhere through a Datum tunnel, with no inbound
ports open on your router.

## Installing

Everything happens in the Home Assistant UI.

[![Add this repository to your Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fdatum-labs%2Fdatum-connect-daemon)

1. **Add the repository.** Click the button above, or go to **Settings →
   Apps** (or **Add-ons**), open the store's three-dot menu, choose
   **Repositories**, and add `https://github.com/datum-labs/datum-connect-daemon`.
2. **Install** Datum Connect from the store.
3. **Start it, then open Datum Connect** in the sidebar. If it isn't there,
   turn on **Show in sidebar** on the add-on's Info tab, or click **Open Web
   UI**. The links at the top of the page go back to the add-on's Info,
   Documentation, Configuration and Log tabs.
4. **Connect.** Click **Connect to Datum**, open the link, sign in to Datum,
   check the code matches, and approve. Back on the page, choose a project
   and click **Continue**. The add-on creates what it needs (see "What
   connecting creates") and starts the tunnel. A new address usually works
   within a minute.
5. **Let Home Assistant accept connections through Datum.** Home Assistant
   rejects requests from a proxy it doesn't trust, so until this is done the
   public address returns `400: Bad Request`. Click **Allow**: the add-on
   turns on **Trust X-Forwarded-For** and adds `127.0.0.1` and `::1` as
   trusted proxies, changing nothing else. Home Assistant restarts (a few
   minutes on a Home Assistant Green; the page comes back by itself), the
   add-on checks that it works, and the page says `Done ✓`. If the check
   fails, nothing is confirmed and Home Assistant reverts on its own; click
   **Retry**. If Home Assistant already trusts the add-on, this step is
   skipped.

   **Or by hand:** click **Skip**, then in **Settings → System → Network**,
   turn on **Use X-Forwarded-For**, add `127.0.0.1` and `::1` as trusted
   proxies, and confirm the change when asked. Don't use an `http:` block in
   `configuration.yaml`; current Home Assistant ignores it.

Updates appear as an **Update** button on the add-on.

**Without the page.** While the add-on isn't connected, a notification (bell
icon) and the add-on's Log tab both show the approval link and code. After
approving, set `project` on the Configuration tab and click Save; connecting
continues without a second sign-in, even if Home Assistant restarts the
add-on, as long as that's within 15 minutes of approving.

**About the approval screen.** It says **datumctl**: the add-on borrows the
public Datum login app until it has its own. Your login is used only to set
up the add-on, never logged, and never sent to the page.

## What connecting creates

In your Datum project, and nothing else:

- a **service account** named `home-assistant-<5 characters>`;
- one **access grant** giving it the `editor` role on that project;
- one **key** for it, valid for a year, kept in the add-on's private storage.

Your login is discarded once these exist. To approve, your login must be
allowed to create service accounts and grant access in the project (an
organization owner or editor can). If it can create the service account but
not grant access, the page names the service account so an owner or editor
can grant it `editor`; then click **Try again**.

- **Revoke:** delete the service account in the Datum portal, under the
  project's **Service accounts**.
- **Re-pair** (for example, to switch projects): click **Re-pair** on the
  page. **Unpair** does the same and also stops the tunnel. Neither deletes
  the old service account; the page names it so you can delete it in the
  portal. Without the page, turn on `repair` on the Configuration tab and
  restart, then turn it off again.
- **Backups** never contain the key. After restoring a backup, connect again
  and delete the old service account in the portal.

## Configuration

| Option | What it is |
|---|---|
| `project` | The Datum project, by id. Optional: the page lists your projects and preselects this one. |
| `target` | What the tunnel points at. Leave empty for this Home Assistant. Must be plain HTTP. Changing it keeps the same address. |
| `tunnel_label` | The tunnel's name. Changing it creates a new tunnel with a new address; the old one is stopped and listed under **Older tunnels**, where **Remove** deletes it. |
| `repair` | Connect again on the next start. Leave off. |
| `service_account_key` | Your own service account key JSON, instead of connecting (see below). |
| `service_account_key_file` | Where to read your own key from if the option above is empty. Default `/share/datum-service-account.json`. |
| `log_level` | Log detail. Leave at `info`. |

## Using your own service account key

Instead of connecting through the page, create a service account in the
Datum portal, give it `editor` on the project, create a key, and paste the
whole key JSON into `service_account_key`. The project is read from the key.

A pasted key always wins over a paired one; clearing it goes back to the
paired key. A pasted key is part of the add-on's configuration, so it is in
your backups: if one leaks, delete the key in Datum and make a new one.

It must be a service account, not your own login: personal sessions expire,
can only be renewed in a browser, and an unattended device then silently stops
working.

## What the add-on sets up

- **Edge protection (WAF).** Datum's web application firewall is on for the
  tunnel (OWASP Core Rule Set, paranoia level 1). Home Assistant's live views
  (logs, the event stream, cameras) skip it, because the firewall holds back
  streamed responses; they still need your Home Assistant login. Rule `920420`
  is excluded because Home Assistant's login sends JSON as `text/plain`.
- **Request timeout.** Datum's edge otherwise ends a response after 15
  seconds, which cuts live views and downloads. The add-on raises it to 1
  hour, the platform maximum.

Both are policies named after the tunnel (`<tunnel id>-waf`,
`<tunnel id>-timeout`). The add-on creates them if missing and otherwise
leaves them as they are, so changes you make in the Datum portal stick.

The Datum Connect page shows the tunnel's address and status, these
settings, older tunnels, and the project and service account in use. It's
served through Home Assistant, so only Home Assistant administrators can open
it.

## Troubleshooting

Check the add-on's **Log** tab first.

**Datum's approval page says "Something went wrong."** Open the link in a
private window or another browser and sign in there; a stale Datum sign-in is
the usual cause. If the code has expired, click **Get a new code**.

**The public address returns `400: Bad Request`.** Home Assistant doesn't
trust the proxy yet. Click **Allow** on the page (installing step 5).

**Allow didn't finish.** Nothing was confirmed, and Home Assistant reverts on
its own. Once Home Assistant is back, click **Retry**. If the page says
another change is waiting, finish or discard it in **Settings → System →
Network** first.

**The public address returns `502`.** Nothing answered at `target`. If you set
it by hand, check the port: no port in your Home Assistant address means 80.

**The public address shows "Service offline".** A new address usually works
within a minute. If it's still offline after 10 minutes, restart the add-on;
if that doesn't help, set a new `tunnel_label`. After reinstalling the
add-on, the old tunnel gets a new connector and can take a few minutes.

**The page says "Waiting for the add-on…" or "Home Assistant is
restarting…".** It comes back by itself. If not, click **Reload**.

**The page says "This page is out of date".** Reload it.

**Choosing a project timed out, or the code expired.** Click **Get a new
code** and approve again.

**Edge protection or the request timeout wasn't set up.** The log says
`NOT set up` or `NOT raised`. The service account is missing permission;
grant `editor` on the project and restart the add-on.

**Home Assistant says it ignores the HTTP YAML configuration.** Remove the
`http:` block from `configuration.yaml` and click **Allow** on the page.

## Known limits

- A single live view or download is cut after 1 hour; reloading starts a new
  one.
- The watchdog (off by default) only checks that the add-on is running, not
  that the tunnel is serving.

## Developing

The add-on image is prebuilt in CI, because the daemon is too large to build
on the device. To test unpublished changes, copy this folder to `/addons/`,
remove the `image:` line from `config.yaml`, put the
`datum-connect-daemon-linux-arm64` workflow artifact at
`bin/datum-connect-daemon`, and choose **Check for updates** in the store.

CI also compiles every jq program in the scripts with jq 1.6, the version in
the base image. To run that locally:
`JQ=/path/to/jq-1.6 addon/datum-connect/tests/check-jq.sh`.
