# Zero-Trust SSH Demo — runnable Daytona version

A live, runnable demo built on top of `datum-connect-daemon`: an ephemeral
[Daytona](https://daytona.io) sandbox hosts the "subway map" demo site from
[`../ec2-zero-trust-ssh/`](../ec2-zero-trust-ssh/), its own dashboard, and
SSH, all three with **zero inbound firewall rules** — two are reached
through a Datum Cloud tunnel, one through a direct peer-to-peer (P2P)
tunnel. Same story as the static writeup in `../ec2-zero-trust-ssh/` (which
originally ran on EC2) and mechanically identical to
[`../tunnel-demo-daytona/`](../tunnel-demo-daytona/) — this is a second,
independent instance of that same automation, serving the subway-map page
instead of a placeholder, under its own tunnel labels
(`ec2demo-web`/`ec2demo-dashboard`) so it can run alongside a live
`tunnel-demo-daytona` sandbox without either one stealing the other's
Cloud tunnels.

## Setting it up

Same prerequisites as [`../tunnel-demo-daytona/README.md`](../tunnel-demo-daytona/README.md#prerequisites):
Linux `datum-connect-daemon`/`datumctl-connect` binaries in
`deploy/daytona-poc/`, a home daemon already running against a real Datum
Cloud project with its connect-dir at `deploy/daytona-poc/.home-connect`,
a Daytona API key at `deploy/daytona-poc/.daytona-api-key`, the `daytona`
Python SDK installed, and `ssh-keygen` on `PATH`.

```
python setup_demo.py
```

Builds the sandbox image, creates the sandbox, starts sshd + the web server
+ the dashboard inside it, connects the relay daemon to all three over P2P,
opens the two Cloud tunnels, then live-edits the sandbox's page over the
freshly-proven SSH connection to swap in the real hostnames (no restart
needed — it's a plain static file server), and prints a summary:

```
site:      https://<random>.datumproxy.net
dashboard: https://<random>.datumproxy.net
ssh:       ssh -i <keyfile> -p <port> demo@127.0.0.1
```

```
python teardown_demo.py
```

Revokes all three peer advertisements, deletes the sandbox, and
stops+deletes both Cloud tunnels.

See [`../tunnel-demo-daytona/README.md`](../tunnel-demo-daytona/README.md#things-worth-knowing-before-you-run-this-live)
for the caveats that apply equally here (hostname DNS propagation lag,
`connector_ready` flicker, stale-endpoint tunnel reuse, Daytona auto-stop).
