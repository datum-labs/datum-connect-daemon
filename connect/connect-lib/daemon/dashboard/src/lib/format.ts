import type { Info, Tunnel } from '@/api';

export type Tone = 'success' | 'warning' | 'danger' | 'muted';

export function humanizeBytes(bytes: number | null | undefined): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  if (!bytes) return '0 B';
  let size = bytes;
  let i = 0;
  while (size >= 1024 && i < units.length - 1) {
    size /= 1024;
    i++;
  }
  return `${size.toFixed(1)} ${units[i]}`;
}

/** `operate:<token_id>` actors are agents/scripts holding a scoped token. */
export function isAgentActor(actor: string | null | undefined): boolean {
  return !!actor && actor.startsWith('operate:');
}

export function statusTone(code: number | null): Tone {
  if (code == null) return 'muted';
  if (code >= 200 && code < 300) return 'success';
  if (code >= 400) return 'danger';
  return 'muted';
}

/**
 * conn_type comes straight from iroh (Endpoint::conn_type) — "direct" means
 * a real hole-punched peer-to-peer path, "relay"/"mixed" mean it's bouncing
 * through (or partly through) a relay server.
 */
export function connTypeTone(connType: string | null | undefined): Tone {
  if (connType === 'direct') return 'success';
  if (connType === 'relay' || connType === 'mixed') return 'warning';
  return 'muted';
}

export function portalUrl(info: Info | null, tunnel: Tunnel | undefined): string | null {
  if (!info || !tunnel) return null;
  return `${info.portal_base_url}/project/${info.project_id}/edge/${tunnel.id}/overview`;
}

export function shortId(id: string): string {
  return `${id.slice(0, 16)}…`;
}

// os_info's names are mostly display-ready; "Mac OS" is the one that isn't.
const OS_NAME_FIXUPS: Record<string, string> = { 'Mac OS': 'macOS' };

/** "macOS 15.5.0", "Ubuntu 24.04 (noble)", "Windows 11 Pro" — falls back to the bare OS family. */
export function osLabel(info: Pick<Info, 'os' | 'os_name' | 'os_version' | 'os_codename' | 'os_edition'>): string {
  if (!info.os_name || info.os_name === 'Unknown') return info.os;
  const name = info.os_edition || OS_NAME_FIXUPS[info.os_name] || info.os_name;
  const version = info.os_version && info.os_version !== 'Unknown' ? ` ${info.os_version}` : '';
  const codename = info.os_codename ? ` (${info.os_codename})` : '';
  return `${name}${version}${codename}`;
}

export function humanizeDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m`;
  return `${s}s`;
}
