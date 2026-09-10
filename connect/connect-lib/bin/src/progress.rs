//! Binary-only tunnel progress rendering. The lib is println!-free; all
//! presentation logic lives here.
//!
//! Three responsibilities:
//! * `format_terminal_failure` — humanises a failed `ProgressStep` into an
//!   actionable, multi-line error message. The canonical case is the iroh-DNS
//!   owner-collision (`IrohDnsPublished: Pending` with `DeferredToOwner`).
//! * `render_progress_step` / `render_verify` — mode-aware callbacks that emit
//!   text-mode log lines on stderr or JSON event objects on stdout.
//! * `await_tunnel_progress` / `verify_endpoints` — async drivers that own the
//!   polling loop and HTTP probes, invoking the callbacks above on transitions.
//!
//! Mode-routing rule:
//!  - `Mode::Text` writes to stderr (so stdout stays clean for shell composition)
//!  - `Mode::Json` writes JSON event objects to stdout (so the Go supervisor's
//!    line-oriented stdin reader sees one event per line)
//!
//! # JSON EVENT CONTRACT (emitted by this module)
//!
//! | Event type           | When                          | Fields                                                          |
//! |----------------------|-------------------------------|-----------------------------------------------------------------|
//! | `tunnel_progress`    | per step status transition    | `step` (snake_case kind), `status`, `resource` (Option<String>) |
//! | `tunnel_verifying`   | start of HTTP probe per URL   | `url`                                                           |
//! | `tunnel_verified`    | HTTP probe success per URL    | `url`                                                           |
//!
//! All events go to stdout (one JSON object per line) when `Mode::Json` is
//! selected. In `Mode::Text`, transitions are printed to stderr in human form.
//! The Go supervisor (`connect/tunnel/listen/main.go`) acknowledges all three
//! types via explicit case arms but currently no-ops them — only
//! `tunnel_ready` (emitted from main.rs) drives `gotReady`.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::time::Duration;

use connect_lib::{
    normalize_endpoint, ProgressStep, ProgressStepKind, StepStatus, TunnelProgress, TunnelService,
};
use n0_error::Result;
use tokio::time::{sleep, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
}

// --- format_terminal_failure ---

pub fn format_terminal_failure(step: &ProgressStep) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Tunnel setup failed at step: {} ({})\n",
        step.kind.label(),
        step.kind.resource_kind()
    ));
    out.push_str(&format!(
        "  resource: {}\n",
        step.resource.as_deref().unwrap_or("(none)")
    ));
    if let Some(r) = &step.reason {
        out.push_str(&format!("  reason: {}\n", r));
    }
    if let Some(m) = &step.message {
        out.push_str(&format!("  message: {}\n", m));
    }
    if matches!(step.kind, ProgressStepKind::IrohDnsPublished)
        && step.status == StepStatus::Pending
        && step.reason.as_deref() == Some("DeferredToOwner")
    {
        out.push_str(
            "\nAnother connector with the same iroh key owns the DNS record \
             for this tunnel. Most likely this means you are running two \
             connectors against the same listen_key store. Stop the other \
             connector or use a different repo directory.\n",
        );
    }
    out
}

// --- step-name + status-name helpers (used by JSON callback) ---

pub(crate) fn step_kind_to_str(k: ProgressStepKind) -> &'static str {
    match k {
        ProgressStepKind::ProxyAccepted => "proxy_accepted",
        ProgressStepKind::CertificatesReady => "certificates_ready",
        ProgressStepKind::ConnectorReady => "connector_ready",
        ProgressStepKind::IrohDnsPublished => "iroh_dns_published",
        ProgressStepKind::ProxyProgrammed => "proxy_programmed",
        ProgressStepKind::ConnectorMetadataProgrammed => "connector_metadata_programmed",
    }
}

pub(crate) fn status_to_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Unknown => "unknown",
        StepStatus::Pending => "pending",
        StepStatus::Ready => "ready",
    }
}

// --- callbacks ---

pub fn render_progress_step(mode: Mode, step: &ProgressStep, _prev: StepStatus, elapsed: Duration) {
    if step.status == StepStatus::Ready {
        let _ = writeln!(
            std::io::stderr(),
            "  \u{2713} {} ({:.1}s) [{}]",
            step.kind.label(),
            elapsed.as_secs_f64(),
            step.resource.as_deref().unwrap_or(""),
        );
        let _ = std::io::stderr().flush();
    }
    if mode == Mode::Json {
        let v = serde_json::json!({
            "type": "tunnel_progress",
            "step": step_kind_to_str(step.kind),
            "status": status_to_str(step.status),
            "resource": step.resource,
        });
        println!("{}", v);
    }
}

pub fn render_verify(mode: Mode, label: &str, url: &str, elapsed: Duration, status: Option<u16>) {
    let status_str = match status {
        Some(s) => format!(": HTTP {}", s),
        None => String::new(),
    };
    let _ = writeln!(
        std::io::stderr(),
        "  \u{2713} {} ({:.1}s) [{}]{}",
        label,
        elapsed.as_secs_f64(),
        url,
        status_str,
    );
    let _ = std::io::stderr().flush();
    if mode == Mode::Json {
        let json_type = match status {
            Some(_) => "tunnel_verified",
            None => "tunnel_verifying",
        };
        println!(
            "{}",
            serde_json::json!({ "type": json_type, "url": url })
        );
    }
}

// --- URL builder for verify_endpoints ---

pub fn build_probe_urls(endpoint: &str, hostname: &str) -> (String, String) {
    let origin = normalize_endpoint(endpoint);
    let proxy = format!("https://{}", hostname);
    (origin, proxy)
}

// --- await_tunnel_progress ---

/// Poll `service.get_active_progress(tunnel_id)` on a 250ms cadence; emit a
/// transition callback for every step whose status changed since the previous
/// poll. Returns the final `TunnelProgress` when all steps are Ready, returns
/// an error formatted via `format_terminal_failure` when a terminal-failure
/// step is observed, and returns an error if the tunnel disappears upstream
/// during setup. Prints a status line to stderr every 10s for any step that
/// has been Pending for at least 10s.
pub async fn await_tunnel_progress<F>(
    service: &TunnelService,
    tunnel_id: &str,
    progress_cb: F,
) -> Result<TunnelProgress>
where
    F: Fn(&ProgressStep, StepStatus),
{
    let mut last_seen: HashMap<ProgressStepKind, StepStatus> = HashMap::new();
    let mut pending_since: HashMap<ProgressStepKind, Instant> = HashMap::new();
    let mut last_status_print: HashMap<ProgressStepKind, u64> = HashMap::new();

    loop {
        let progress_opt = service
            .get_active_progress(tunnel_id)
            .await
            .map_err(|e| n0_error::anyerr!("polling tunnel {tunnel_id} progress: {e}"))?;
        let Some(progress) = progress_opt else {
            return Err(n0_error::anyerr!(
                "Tunnel {tunnel_id} disappeared during setup"
            ));
        };

        // Diff and emit transitions.
        for step in &progress.steps {
            let prev = last_seen
                .get(&step.kind)
                .copied()
                .unwrap_or(StepStatus::Unknown);
            if prev != step.status {
                progress_cb(step, prev);
                last_seen.insert(step.kind, step.status);
            }
            // Track Pending duration; print status every 10s.
            if step.status == StepStatus::Pending {
                pending_since.entry(step.kind).or_insert_with(Instant::now);
                if let Some(start) = pending_since.get(&step.kind) {
                    let secs = start.elapsed().as_secs();
                    let last_print = last_status_print.get(&step.kind).copied().unwrap_or(0);
                    if secs >= 10 && secs - last_print >= 10 {
                        let _ = writeln!(
                            std::io::stderr(),
                            "  \u{25CB} waiting for {} ({:.0}s) [{}]",
                            step.kind.label(),
                            start.elapsed().as_secs_f64(),
                            step.resource.as_deref().unwrap_or("")
                        );
                        let _ = std::io::stderr().flush();
                        last_status_print.insert(step.kind, secs);
                    }
                }
            } else {
                pending_since.remove(&step.kind);
                last_status_print.remove(&step.kind);
            }
        }

        // Check terminal failure.
        if let Some(failed) = progress.terminal_failure() {
            return Err(n0_error::anyerr!("{}", format_terminal_failure(failed)));
        }

        if progress.all_ready() {
            return Ok(progress);
        }

        sleep(Duration::from_millis(250)).await;
    }
}

// --- verify_endpoints ---

/// Probe the origin endpoint (HTTP, best-effort) and then the tunnel proxy URL
/// (HTTPS, indefinite). Origin is bounded by `budget` and is non-fatal on
/// failure. The proxy URL is checked on a fixed 10-second interval until it
/// returns a non-5xx response, printing a status line on each attempt so the
/// user sees progress during settling time.
pub async fn verify_endpoints<F, R>(
    origin_endpoint: &str,
    hostname: &str,
    budget: Duration,
    verify_cb: F,
    mut refresh_cb: R,
) -> Result<()>
where
    F: Fn(&str, &str, Duration, Option<u16>),
    R: FnMut(),
{
    let (origin_url, proxy_url) = build_probe_urls(origin_endpoint, hostname);

    let per_attempt_timeout = Duration::from_secs(5);
    let client = reqwest::Client::builder()
        .timeout(per_attempt_timeout)
        .danger_accept_invalid_certs(false)
        .build()
        .map_err(|e| n0_error::anyerr!("building reqwest client for verify_endpoints: {e}"))?;

    // Origin probe — best-effort with budget, non-fatal on failure.
    match probe_until_reachable(&client, &origin_url, budget / 2).await {
        Ok((elapsed, status)) => {
            verify_cb("origin reachable", &origin_url, elapsed, Some(status));
        }
        Err(_e) => {
            let _ = writeln!(
                std::io::stderr(),
                "warning: origin {} did not respond within budget — continuing",
                origin_url
            );
            let _ = std::io::stderr().flush();
        }
    }

    // Proxy probe — fixed 10s interval, indefinite, until non-5xx.
    let start = Instant::now();
    loop {
        let result = probe_url_with_dns_fallback(
            &client,
            &proxy_url,
            per_attempt_timeout,
        )
        .await;
        match result {
            Ok(status) => {
                if status < 500 {
                    verify_cb("proxy responding", &proxy_url, start.elapsed(), Some(status));
                    return Ok(());
                }
                let _ = writeln!(
                    std::io::stderr(),
                    "  \u{25CB} waiting for tunnel [{}] ({:.0}s) ... HTTP {}",
                    proxy_url,
                    start.elapsed().as_secs_f64(),
                    status,
                );
                let _ = std::io::stderr().flush();
            }
            Err(e) => {
                let mut cause = e.to_string();
                let mut source = std::error::Error::source(&e);
                while let Some(s) = source {
                    cause = s.to_string();
                    source = s.source();
                }
                let _ = writeln!(
                    std::io::stderr(),
                    "  \u{25CB} waiting for tunnel [{}] ({:.0}s) ... {}",
                    proxy_url,
                    start.elapsed().as_secs_f64(),
                    cause,
                );
                let _ = std::io::stderr().flush();
            }
        }
        sleep(Duration::from_secs(10)).await;
        // Nudge the replicator → Envoy xDS propagation chain. The initial
        // refresh_connection_details call may have raced with the
        // replicator capturing Ready:False; re-patching here re-triggers
        // the mirror so Envoy eventually picks up the iroh cluster config.
        refresh_cb();
    }
}

/// Read nameserver IPs from /etc/resolv.conf.
fn system_nameservers() -> Vec<std::net::IpAddr> {
    let content = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let mut ips = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver ") {
            if let Ok(ip) = rest.trim().parse::<std::net::IpAddr>() {
                ips.push(ip);
            }
        }
    }
    ips
}

/// Create a ResolverConfig using system DNS servers from /etc/resolv.conf,
/// falling back to Google public DNS (UDP) as a last resort.
fn system_resolver_config() -> hickory_resolver::config::ResolverConfig {
    use hickory_resolver::config::{NameServerConfig, ResolverConfig};
    use hickory_resolver::proto::xfer::Protocol;
    let sys_ips = system_nameservers();
    let mut config = ResolverConfig::new();
    for ip in sys_ips {
        config.add_name_server(NameServerConfig::new(
            std::net::SocketAddr::new(ip, 53),
            Protocol::Udp,
        ));
    }
    if config.name_servers().is_empty() {
        for ip in &[
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 4, 4)),
        ] {
            config.add_name_server(NameServerConfig::new(
                std::net::SocketAddr::new(*ip, 53),
                Protocol::Udp,
            ));
        }
    }
    config
}

/// Create a ResolverConfig pointing at the given authoritative NS IPs
/// on port 53 UDP.
fn auth_ns_config(ns_ips: &[std::net::IpAddr]) -> hickory_resolver::config::ResolverConfig {
    use hickory_resolver::config::{NameServerConfig, ResolverConfig};
    use hickory_resolver::proto::xfer::Protocol;
    let mut config = ResolverConfig::new();
    for ip in ns_ips {
        config.add_name_server(NameServerConfig::new(
            std::net::SocketAddr::new(*ip, 53),
            Protocol::Udp,
        ));
    }
    config
}

/// Query the system resolver for NS records of `domain`, then resolve
/// each NS hostname to IP addresses.
async fn resolve_ns_ips(
    system_resolver: &hickory_resolver::TokioResolver,
    domain: &str,
) -> Vec<std::net::IpAddr> {
    use hickory_resolver::proto::rr::RData;
    let ns_lookup = match system_resolver.ns_lookup(domain).await {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    let mut ns_names: Vec<String> = Vec::new();
    for record in ns_lookup.as_lookup().records() {
        if let RData::NS(name) = record.data() {
            ns_names.push(name.to_string().trim_end_matches('.').to_string());
        }
    }
    let mut ns_ips = BTreeSet::new();
    for name in &ns_names {
        if let Ok(ip_lookup) = system_resolver.lookup_ip(name).await {
            for ip in ip_lookup.iter() {
                ns_ips.insert(ip);
            }
        }
    }
    ns_ips.into_iter().collect()
}

/// Walk the hostname labels from right to left, querying NS records at each
/// level until one is found. Returns the NS IPs and the domain where they
/// were found.
async fn discover_ns_authority(
    system_resolver: &hickory_resolver::TokioResolver,
    hostname: &str,
) -> (Vec<std::net::IpAddr>, String) {
    let labels: Vec<&str> = hostname.split('.').collect();
    // Start from the full hostname and work inward.
    for n in (1..labels.len()).rev() {
        let domain: String = labels[n - 1..].join(".");
        let ns_ips = resolve_ns_ips(system_resolver, &domain).await;
        if !ns_ips.is_empty() {
            return (ns_ips, domain);
        }
    }
    (Vec::new(), String::new())
}

/// Resolve the proxy hostname via authoritative DNS as a visible progress step.
/// Used between controller-condition polling and HTTP verification so the user
/// sees a clear "DNS provisioned" step and we fail fast if resolution fails.
/// Retries every 5 seconds until success or timeout.
pub async fn resolve_hostname_dns(
    hostname: &str,
) -> Result<Vec<std::net::IpAddr>> {
    let start = Instant::now();
    let max_duration = Duration::from_secs(120);
    let retry_interval = Duration::from_secs(5);

    let sys_resolver = hickory_resolver::Resolver::builder_with_config(
        system_resolver_config(),
        hickory_resolver::name_server::TokioConnectionProvider::default(),
    )
    .build();

    let (ns_ips, ns_domain) = discover_ns_authority(&sys_resolver, hostname).await;
    if ns_ips.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "  \u{2717} checking for NS ({:.1}s) [{}]: no authoritative server found",
            start.elapsed().as_secs_f64(),
            hostname,
        );
        let _ = std::io::stderr().flush();
        return Err(n0_error::anyerr!(
            "no authoritative NS found for {hostname}"
        ));
    }

    let ns_ip_str: Vec<String> = ns_ips.iter().map(|ip| ip.to_string()).collect();
    let _ = writeln!(
        std::io::stderr(),
        "  \u{2713} checking for NS ({:.1}s) [{}]: {}",
        start.elapsed().as_secs_f64(),
        ns_domain,
        ns_ip_str.join(", "),
    );
    let _ = std::io::stderr().flush();

    let auth_config = auth_ns_config(&ns_ips);
    let mut last_print = Instant::now();
    loop {
        let auth_resolver = hickory_resolver::Resolver::builder_with_config(
            auth_config.clone(),
            hickory_resolver::name_server::TokioConnectionProvider::default(),
        )
        .build();
        let mut ips: Vec<std::net::IpAddr> = Vec::new();
        if let Ok(lookup) = auth_resolver.ipv4_lookup(hostname).await {
            for record in lookup.as_lookup().records() {
                if let hickory_resolver::proto::rr::RData::A(addr) = record.data() {
                    ips.push(std::net::IpAddr::V4(std::net::Ipv4Addr::from(*addr)));
                }
            }
        }
        if ips.is_empty() {
            if let Ok(lookup) = auth_resolver.ipv6_lookup(hostname).await {
                for record in lookup.as_lookup().records() {
                    if let hickory_resolver::proto::rr::RData::AAAA(addr) = record.data() {
                        ips.push(std::net::IpAddr::V6(std::net::Ipv6Addr::from(*addr)));
                    }
                }
            }
        }

        if !ips.is_empty() {
            let ip_str: Vec<String> = ips.iter().map(|ip| ip.to_string()).collect();
            let _ = writeln!(
                std::io::stderr(),
                "  \u{2713} DNS provisioned ({:.1}s) [{}]: {}",
                start.elapsed().as_secs_f64(),
                hostname,
                ip_str.join(", "),
            );
            let _ = std::io::stderr().flush();
            return Ok(ips);
        }

        if start.elapsed() >= max_duration {
            let _ = writeln!(
                std::io::stderr(),
                "  \u{2717} DNS provisioned ({:.1}s) [{}]: resolution failed",
                start.elapsed().as_secs_f64(),
                hostname,
            );
            let _ = std::io::stderr().flush();
            return Err(n0_error::anyerr!(
                "could not resolve {hostname} via authoritative DNS after {}s",
                max_duration.as_secs(),
            ));
        }

        if last_print.elapsed() >= Duration::from_secs(5) {
            let _ = writeln!(
                std::io::stderr(),
                "  \u{25CB} waiting for A record ({:.0}s) [{}]",
                start.elapsed().as_secs_f64(),
                hostname,
            );
            let _ = std::io::stderr().flush();
            last_print = Instant::now();
        }

        tokio::time::sleep(retry_interval).await;
    }
}

/// Probe a URL, falling back to the authoritative NS if the system resolver
/// returns a DNS error. When the system resolver returns NXDOMAIN (e.g.
/// systemd-resolved caching a stale negative entry), we bypass it entirely:
/// discover the authoritative name servers for the domain, query them directly
/// for A/AAAA records, and connect to the resolved IPs with the Host header
/// preserved.
async fn probe_url_with_dns_fallback(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> std::result::Result<u16, reqwest::Error> {
    match client.get(url).send().await {
        Ok(resp) => return Ok(resp.status().as_u16()),
        Err(e) if !is_dns_error(&e) => return Err(e),
        Err(dns_err) => {
            let _ = writeln!(
                std::io::stderr(),
                "  \u{26A0} system DNS failed ({}), querying authoritative NS...",
                dns_err,
            );
            let _ = std::io::stderr().flush();
        }
    }

    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return client.get(url).send().await.map(|r| r.status().as_u16()),
    };
    let Some(host) = parsed.host_str() else {
        return client.get(url).send().await.map(|r| r.status().as_u16());
    };
    let port = parsed.port_or_known_default().unwrap_or(443);
    let scheme = parsed.scheme().to_string();

    let sys_resolver = hickory_resolver::Resolver::builder_with_config(
        system_resolver_config(),
        hickory_resolver::name_server::TokioConnectionProvider::default(),
    )
    .build();

    let (ns_ips, _ns_domain) = discover_ns_authority(&sys_resolver, host).await;
    if ns_ips.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "  \u{26A0} no NS records found for {}, falling back to system DNS",
            host,
        );
        let _ = std::io::stderr().flush();
        return client.get(url).send().await.map(|r| r.status().as_u16());
    }

    let auth_resolver = hickory_resolver::Resolver::builder_with_config(
        auth_ns_config(&ns_ips),
        hickory_resolver::name_server::TokioConnectionProvider::default(),
    )
    .build();

    let mut ips: Vec<std::net::IpAddr> = Vec::new();
    let a_result = auth_resolver.ipv4_lookup(host).await;
    match &a_result {
        Ok(lookup) => {
            for record in lookup.as_lookup().records() {
                if let hickory_resolver::proto::rr::RData::A(addr) = record.data() {
                    ips.push(std::net::IpAddr::V4(std::net::Ipv4Addr::from(*addr)));
                }
            }
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "  \u{26A0} auth NS A lookup failed: {e}");
            let _ = std::io::stderr().flush();
        }
    }
    if ips.is_empty() {
        let aaaa_result = auth_resolver.ipv6_lookup(host).await;
        match &aaaa_result {
            Ok(lookup) => {
                for record in lookup.as_lookup().records() {
                    if let hickory_resolver::proto::rr::RData::AAAA(addr) = record.data() {
                        ips.push(std::net::IpAddr::V6(std::net::Ipv6Addr::from(*addr)));
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "  \u{26A0} auth NS AAAA lookup failed: {e}");
                let _ = std::io::stderr().flush();
            }
        }
    }
    if ips.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "  \u{26A0} auth NS returned no IPs for {}",
            host,
        );
        let _ = std::io::stderr().flush();
        return client.get(url).send().await.map(|r| r.status().as_u16());
    }

    for ip in ips {
        let ip_url = format!("{scheme}://{ip}:{port}/");
        let req = client.get(&ip_url).header("host", host);
        match tokio::time::timeout(timeout, req.send()).await {
            Ok(Ok(resp)) => return Ok(resp.status().as_u16()),
            Ok(Err(e)) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "  \u{26A0} auth NS connect to {ip} failed: {e}",
                );
                let _ = std::io::stderr().flush();
                continue;
            }
            Err(_) => continue,
        }
    }

    client.get(url).send().await.map(|r| r.status().as_u16())
}

/// Returns true if the reqwest error is DNS-related (resolution failure),
/// as opposed to a connection timeout, TLS error, etc. Walks the full
/// error source chain because reqwest wraps the real cause.
fn is_dns_error(e: &reqwest::Error) -> bool {
    let mut current: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = current {
        let msg = err.to_string().to_lowercase();
        if msg.contains("dns")
            || msg.contains("name or service not known")
            || msg.contains("nodomain")
            || msg.contains("failed to lookup")
            || msg.contains("no such host")
        {
            return true;
        }
        current = err.source();
    }
    false
}

async fn probe_until_reachable(
    client: &reqwest::Client,
    url: &str,
    budget: Duration,
) -> Result<(Duration, u16)> {
    let start = Instant::now();
    let mut backoff = Duration::from_millis(250);
    loop {
        if start.elapsed() >= budget {
            return Err(n0_error::anyerr!("probe budget exhausted"));
        }
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status < 500 {
                    return Ok((start.elapsed(), status));
                }
            }
            Err(_e) => {}
        }
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(n0_error::anyerr!("probe budget exhausted"));
        }
        sleep(std::cmp::min(backoff, remaining)).await;
        backoff = std::cmp::min(backoff * 2, Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: ProgressStepKind, status: StepStatus, reason: Option<&str>) -> ProgressStep {
        ProgressStep {
            kind,
            status,
            reason: reason.map(String::from),
            message: None,
            resource: Some(format!("{}/x", kind.resource_kind())),
        }
    }

    #[test]
    fn terminal_failure_iroh_owner_collision_includes_actionable_message() {
        let s = step(
            ProgressStepKind::IrohDnsPublished,
            StepStatus::Pending,
            Some("DeferredToOwner"),
        );
        let out = format_terminal_failure(&s);
        assert!(out.contains("Tunnel setup failed at step"));
        assert!(out.contains("Another connector with the same iroh key"));
    }

    #[test]
    fn terminal_failure_generic_still_has_header_and_resource() {
        let s = step(
            ProgressStepKind::ProxyAccepted,
            StepStatus::Pending,
            Some("Whatever"),
        );
        let out = format_terminal_failure(&s);
        assert!(out.contains("Tunnel setup failed at step"));
        assert!(out.contains("resource: HTTPProxy/x"));
        assert!(!out.contains("Another connector with the same iroh key"));
    }

    #[test]
    fn build_probe_urls_adds_http_prefix_to_bare_endpoint() {
        let (origin, proxy) = build_probe_urls("localhost:8080", "x.example.com");
        assert_eq!(origin, "http://localhost:8080");
        assert_eq!(proxy, "https://x.example.com");
    }

    #[test]
    fn build_probe_urls_keeps_scheme_when_present() {
        let (origin, _) = build_probe_urls("https://api.example.com", "x.example.com");
        assert_eq!(origin, "https://api.example.com");
    }

    #[test]
    fn json_progress_event_parses_back_to_expected_fields() {
        // We can't directly capture println output in a unit test trivially;
        // instead, reconstruct the same json! body and re-parse.
        let s = step(ProgressStepKind::ProxyAccepted, StepStatus::Ready, None);
        let v = serde_json::json!({
            "type": "tunnel_progress",
            "step": step_kind_to_str(s.kind),
            "status": status_to_str(s.status),
            "resource": s.resource,
        });
        let parsed: serde_json::Value = serde_json::from_str(&v.to_string()).unwrap();
        assert_eq!(parsed["type"], "tunnel_progress");
        assert_eq!(parsed["step"], "proxy_accepted");
        assert_eq!(parsed["status"], "ready");
        assert!(parsed["resource"].is_string());
    }
}
