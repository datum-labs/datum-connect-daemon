// Wire types for the daemon's /v1 API, mirroring the Rust structs in
// daemon/src/{main,inspector,peer,logs}.rs and connect-lib's TunnelSummary.

export interface Info {
  project_id: string;
  portal_base_url: string;
  log_tail_max_lines: number;
  device_name: string;
  hostname: string;
  /** Rust's std::env::consts::OS — "macos", "linux", "windows", … */
  os: string;
  arch: string;
  /** From the os_info crate, e.g. "Mac OS", "Ubuntu", "Windows". */
  os_name: string;
  /** e.g. "15.5.0", "24.04"; "Unknown" when undetectable. */
  os_version: string;
  /** e.g. "noble" on Ubuntu; null where the OS doesn't report one. */
  os_codename: string | null;
  /** e.g. "Windows 11 Pro"; null where the OS doesn't report one. */
  os_edition: string | null;
  daemon_version: string;
  started_at_unix_ms: number;
  max_tunnel_hours: number;
}

export interface Tunnel {
  id: string;
  label: string;
  endpoint: string;
  hostnames: string[];
  enabled: boolean;
  accepted: boolean;
  programmed: boolean;
  connector_metadata_programmed: boolean;
  connector_ready: boolean;
  connector_name: string | null;
  connector_device: string | null;
  /** `setup` (a human), `operate:<token_id>` (an agent), or null. */
  last_start_actor: string | null;
}

export interface ProgressStep {
  kind: string;
  status: string;
}

export interface Progress {
  hostnames: string[];
  steps: ProgressStep[];
  ready: boolean;
}

export interface Metrics {
  bytes_to_origin: number;
  bytes_from_origin: number;
  accepted_requests: number;
  denied_requests: number;
  failed_requests: number;
  active_requests: number;
  active_iroh_connections: number;
  total_iroh_connections: number;
}

export interface ExchangeSummary {
  id: string;
  timestamp_unix_ms: number;
  method: string;
  path: string;
  response_status: number | null;
}

export type Header = [string, string];

export interface CapturedMessage {
  headers: Header[];
  body: string;
  body_truncated: boolean;
}

export interface Exchange extends ExchangeSummary {
  request: CapturedMessage;
  response: CapturedMessage | null;
}

export interface ReplayResult {
  status: number;
  headers: Header[];
  body: string;
}

export interface Advertisement {
  resource_id: string;
  label: string;
  endpoint: string;
  enabled: boolean;
  bytes_to_origin: number;
  bytes_from_origin: number;
}

export interface PeerConnection {
  id: string;
  bound_addr: string;
  remote_endpoint_id: string;
  target: string;
  conn_type: string;
  conn_detail: string | null;
  latency_ms: number | null;
  transition_count: number;
}

export interface ConnectMetrics {
  bytes_to_upstream: number;
  bytes_from_upstream: number;
  active_iroh_connections: number;
  total_iroh_connections: number;
}

export interface PeerData {
  endpoint_id: string | null;
  advertisements: Advertisement[];
  connections: PeerConnection[];
  connect_metrics: ConnectMetrics | null;
}

export interface LogSource {
  name: string;
  path: string;
  exists: boolean;
  size_bytes: number | null;
}

export interface LogTail {
  name: string;
  lines: string[];
}

export async function api<T>(path: string, token: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: { Authorization: `Bearer ${token}`, ...init?.headers },
  });
  if (!res.ok) {
    let body: { error?: string } = {};
    try {
      body = await res.json();
    } catch {
      // non-JSON error body — fall through to the status code
    }
    throw new Error(body.error || `HTTP ${res.status}`);
  }
  return res.json();
}

/** Unauthenticated — see `get_info` in daemon/src/main.rs. */
export async function fetchInfo(): Promise<Info> {
  const res = await fetch('/v1/info');
  return res.json();
}
