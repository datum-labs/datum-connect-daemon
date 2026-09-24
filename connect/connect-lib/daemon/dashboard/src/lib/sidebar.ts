import type { Advertisement, Info, LogSource, PeerConnection, Tunnel } from '@/api';

export type SidebarTab = 'tunnels' | 'peers' | 'logs';
export type TunnelScope = 'device' | 'all';

export const TUNNEL_SORTS = { name: 'Name', status: 'Status', hostname: 'Hostname' } as const;
export const PEER_SORTS = { name: 'Name', traffic: 'Traffic' } as const;
export const LOG_SORTS = { name: 'Name', size: 'Size' } as const;

export type TunnelSort = keyof typeof TUNNEL_SORTS;
export type PeerSort = keyof typeof PEER_SORTS;
export type LogSort = keyof typeof LOG_SORTS;

const byName = (a: string, b: string) => a.localeCompare(b, undefined, { sensitivity: 'base', numeric: true });

function matches(query: string, ...fields: (string | null | undefined)[]): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return fields.some((f) => f?.toLowerCase().includes(q));
}

// Default to this device's own tunnels — a shared project can accumulate
// tunnels from many machines (other laptops, cloud test boxes), and those
// are noise for "what's running here right now". connector_device is set
// server-side from the same friendly_device_name() this daemon reports at
// /v1/info, so an exact match is a reliable "created from this machine"
// signal without needing per-tunnel local state.
export function isLocalTunnel(info: Info | null, t: Tunnel): boolean {
  return !!info?.device_name && t.connector_device === info.device_name;
}

export function tunnelHost(t: Tunnel): string {
  return t.hostnames[0] || t.endpoint;
}

/** On + connector ready, then on, then off. */
function tunnelRank(t: Tunnel): number {
  if (t.enabled && t.connector_ready) return 0;
  if (t.enabled) return 1;
  return 2;
}

export function visibleTunnels(
  tunnels: Tunnel[],
  opts: { info: Info | null; scope: TunnelScope; query: string; sort: TunnelSort },
): Tunnel[] {
  return tunnels
    .filter((t) => opts.scope === 'all' || isLocalTunnel(opts.info, t))
    .filter((t) => matches(opts.query, t.label, t.endpoint, t.connector_device, ...t.hostnames))
    .sort((a, b) => {
      if (opts.sort === 'status') return tunnelRank(a) - tunnelRank(b) || byName(a.label, b.label);
      if (opts.sort === 'hostname') return byName(tunnelHost(a), tunnelHost(b));
      return byName(a.label, b.label);
    });
}

export function visibleAdvertisements(ads: Advertisement[], opts: { query: string; sort: PeerSort }): Advertisement[] {
  const traffic = (a: Advertisement) => a.bytes_from_origin + a.bytes_to_origin;
  return ads
    .filter((a) => matches(opts.query, a.label, a.endpoint, a.resource_id))
    .sort((a, b) => (opts.sort === 'traffic' ? traffic(b) - traffic(a) : 0) || byName(a.label, b.label));
}

export function visibleConnections(conns: PeerConnection[], opts: { query: string; sort: PeerSort }): PeerConnection[] {
  // Connections carry no byte counters of their own (only the combined
  // connect_metrics), so "traffic" falls back to latency — fastest first.
  const latency = (c: PeerConnection) => c.latency_ms ?? Number.MAX_SAFE_INTEGER;
  return conns
    .filter((c) => matches(opts.query, c.target, c.bound_addr, c.remote_endpoint_id, c.conn_type))
    .sort((a, b) => (opts.sort === 'traffic' ? latency(a) - latency(b) : 0) || byName(a.target, b.target));
}

export function visibleLogs(sources: LogSource[], opts: { query: string; sort: LogSort }): LogSource[] {
  return sources
    .filter((s) => matches(opts.query, s.name, s.path))
    .sort((a, b) => (opts.sort === 'size' ? (b.size_bytes ?? 0) - (a.size_bytes ?? 0) : 0) || byName(a.name, b.name));
}
