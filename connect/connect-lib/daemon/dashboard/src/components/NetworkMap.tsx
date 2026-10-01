import type { Info, LogSource, PeerData, Tunnel } from '@/api';
import { DetailPane } from '@/components/DetailPane';
import { humanizeBytes } from '@/lib/format';
import { isLocalTunnel } from '@/lib/sidebar';
import { Card } from '@datum-cloud/datum-ui/card';
import { EmptyContent } from '@datum-cloud/datum-ui/empty-content';
import { Text } from '@datum-cloud/datum-ui/typography';
import { cn } from '@datum-cloud/datum-ui/utils';
import { useEffect, useRef } from 'react';

/**
 * Line colors. Success/warning match StatusDot's green/amber so a station
 * reads the same as its row in the sidebar; ALB tunnels and the hub keep
 * the sky/violet they had in the original subway-map demo pages.
 */
type LineColor = 'hub' | 'good' | 'warn' | 'alb' | 'text' | 'dim';

const STROKE: Record<LineColor, string> = {
  hub: 'stroke-violet-400',
  good: 'stroke-green-500',
  warn: 'stroke-amber-400',
  alb: 'stroke-sky-400',
  text: 'stroke-foreground',
  dim: 'stroke-muted-foreground',
};

const SWATCH: Record<LineColor, string> = {
  hub: 'bg-violet-400',
  good: 'bg-green-500',
  warn: 'bg-amber-400',
  alb: 'bg-sky-400',
  text: 'bg-foreground',
  dim: 'bg-muted-foreground',
};

const LEGEND: [LineColor, string][] = [
  ['hub', 'This daemon'],
  ['good', 'Direct P2P / advertising'],
  ['warn', 'P2P via relay'],
  ['alb', 'ALB tunnel — Application Load Balancer (via Datum Cloud)'],
  ['text', 'Log source'],
  ['dim', 'Off / revoked / missing'],
];

/** Where clicking a station goes — the same detail view its sidebar row opens. */
export type StationTarget =
  | { kind: 'tunnel'; id: string }
  | { kind: 'peer-ad'; id: string }
  | { kind: 'peer-conn'; id: string }
  | { kind: 'log'; name: string };

interface Station {
  key: string;
  label: string;
  caption: string;
  color: LineColor;
  target: StationTarget;
}

interface Block {
  kind: 'spoke' | 'interchange';
  side: 'left' | 'right';
  items: Station[];
  interchangeLabel?: string;
  top: number;
  midY: number;
}

function peerConnColor(connType: string): LineColor {
  if (connType === 'direct') return 'good';
  if (connType === 'relay' || connType === 'mixed') return 'warn';
  return 'dim';
}

/**
 * A live "subway map" of this daemon's own tunnels, peer connections and
 * logs, adapted from the hand-drawn demo pages that used the same
 * stations/lines/interchange convention with hardcoded data. Everything is
 * computed from what the sidebar already polls — no extra endpoint.
 */
export function NetworkMap({
  info,
  tunnels,
  peers,
  logs,
  onSelect,
}: {
  info: Info | null;
  tunnels: Tunnel[];
  peers: PeerData | null;
  logs: LogSource[];
  onSelect: (target: StationTarget) => void;
}) {
  // Always this device's own tunnels, never the sidebar's "All devices"
  // scope: the map is this daemon's view of what it's forwarding, and
  // project-wide tunnels from other machines would misrepresent that.
  const local = tunnels.filter((t) => isLocalTunnel(info, t));

  const peerItems: Station[] = [
    ...(peers?.advertisements ?? []).map(
      (a): Station => ({
        key: `peer-ad:${a.resource_id}`,
        label: a.label,
        caption: a.enabled ? 'advertised — reachable by peers with a ticket' : 'revoked',
        color: a.enabled ? 'good' : 'dim',
        target: { kind: 'peer-ad', id: a.resource_id },
      }),
    ),
    ...(peers?.connections ?? []).map(
      (c): Station => ({
        key: `peer-conn:${c.id}`,
        label: c.target,
        caption: `via ${c.bound_addr}${c.latency_ms != null ? ` · ${c.latency_ms}ms` : ''}`,
        color: peerConnColor(c.conn_type),
        target: { kind: 'peer-conn', id: c.id },
      }),
    ),
  ];

  const tunnelItems = local.map(
    (t): Station => ({
      key: `tunnel:${t.id}`,
      label: t.label,
      // The note (when set via the CLI) says *why* the tunnel exists — with
      // 10+ tunnels running that's more useful here than the hostname.
      caption: t.note || t.hostnames[0] || t.endpoint || '(no hostname yet)',
      color: t.enabled ? 'alb' : 'dim',
      target: { kind: 'tunnel', id: t.id },
    }),
  );

  const logItems = logs.map(
    (s): Station => ({
      key: `log:${s.name}`,
      label: s.name,
      caption: s.exists ? humanizeBytes(s.size_bytes) : '(no file yet)',
      color: s.exists ? 'text' : 'dim',
      target: { kind: 'log', name: s.name },
    }),
  );

  const flash = useChangeFlash([...peerItems, ...tunnelItems, ...logItems]);

  if (!peerItems.length && !tunnelItems.length && !logItems.length) {
    return (
      <DetailPane title="Network map">
        <EmptyContent title="No tunnels, peer connections, or logs on this device yet — create one to see it here." />
      </DetailPane>
    );
  }

  const ROW_H = 68;
  const TOP_PAD = 46;
  const BLOCK_GAP = 40;
  const HUB_X = 90;
  const INTERCHANGE_X = 330;
  const STATION_X = 620;
  const LABEL_X = 645;
  // Logs aren't traffic in or out of the daemon, so they go out the *other*
  // side of the hub — a log source should never be mistaken for a network
  // path.
  const LOG_STATION_X = -140;
  const LOG_LABEL_X = -165;
  const VIEW_MIN_X = -280;
  const VIEW_MAX_X = 900;

  // Stacked peers, tunnels, logs. Laid out generically so a future block
  // (e.g. VPC-landed tunnels, see GVPC-EDGE-PLAN.md) is just another entry.
  const blocks: Block[] = [];
  const push = (b: Omit<Block, 'top' | 'midY'>) => b.items.length && blocks.push({ ...b, top: 0, midY: 0 });
  push({ kind: 'spoke', side: 'right', items: peerItems });
  push({ kind: 'interchange', side: 'right', items: tunnelItems, interchangeLabel: 'Datum Cloud ALB' });
  push({ kind: 'spoke', side: 'left', items: logItems });

  // Each side stacks and centers on the hub independently.
  const sideHeight = (side: Block['side']) => {
    const bs = blocks.filter((b) => b.side === side);
    return Math.max(bs.reduce((h, b) => h + b.items.length * ROW_H + BLOCK_GAP, 0) - BLOCK_GAP, 0);
  };
  const totalH = Math.max(Math.max(sideHeight('right'), sideHeight('left')) + TOP_PAD * 2, 160);
  const hubY = totalH / 2;
  for (const side of ['left', 'right'] as const) {
    let y = hubY - sideHeight(side) / 2;
    for (const b of blocks.filter((x) => x.side === side)) {
      b.top = y;
      b.midY = y + (b.items.length * ROW_H) / 2;
      y += b.items.length * ROW_H + BLOCK_GAP;
    }
  }

  const stationY = (b: Block, i: number) => b.top + i * ROW_H + ROW_H / 2;

  return (
    <DetailPane title="Network map" description="Live view of this daemon's own tunnels, peer connections, and logs">
      <Card size="sm" className="gap-4 p-4">
        <div className="flex flex-wrap gap-x-4 gap-y-1.5">
          {LEGEND.map(([color, label]) => (
            <span key={color} className="flex items-center gap-1.5">
              <span className={cn('inline-block h-1 w-4 rounded-full', SWATCH[color])} />
              <Text size="xs" textColor="muted">
                {label}
              </Text>
            </span>
          ))}
        </div>
        <svg
          viewBox={`${VIEW_MIN_X} 0 ${VIEW_MAX_X - VIEW_MIN_X} ${totalH}`}
          preserveAspectRatio="xMidYMid meet"
          className="block h-auto w-full"
          role="img"
          aria-label="Network map">
          {/* Lines first, so stations sit on top of them. */}
          {blocks.map((b) => {
            const stationX = b.side === 'left' ? LOG_STATION_X : STATION_X;
            const fromX = b.kind === 'interchange' ? INTERCHANGE_X : HUB_X;
            const fromY = b.kind === 'interchange' ? b.midY : hubY;
            return (
              <g key={`lines:${b.side}:${b.kind}`}>
                {b.kind === 'interchange' && (
                  <polyline
                    points={`${HUB_X},${hubY} ${INTERCHANGE_X},${b.midY}`}
                    className={STROKE.alb}
                    strokeWidth={5}
                    fill="none"
                    strokeLinecap="round"
                  />
                )}
                {b.items.map((item, i) => (
                  <polyline
                    key={`${item.key}:${item.color}`}
                    points={`${fromX},${fromY} ${stationX},${stationY(b, i)}`}
                    className={cn(STROKE[item.color], flash(item) && 'map-flash-line')}
                    strokeWidth={5}
                    fill="none"
                    strokeLinecap="round"
                    opacity={item.color === 'dim' ? 0.4 : 1}
                  />
                ))}
              </g>
            );
          })}

          <circle cx={HUB_X} cy={hubY} r={10} className={cn('fill-background', STROKE.hub)} strokeWidth={4} />
          <text x={HUB_X} y={hubY - 20} textAnchor="middle" className="fill-violet-400 text-xs font-semibold">
            This daemon
          </text>

          {blocks.map((b) => (
            <g key={`stations:${b.side}:${b.kind}`}>
              {b.kind === 'interchange' && (
                // Double ring = interchange stop, same convention as the demo pages.
                <>
                  <circle cx={INTERCHANGE_X} cy={b.midY} r={11} className={cn('fill-card', STROKE.alb)} strokeWidth={4} />
                  <circle cx={INTERCHANGE_X} cy={b.midY} r={5} className="fill-sky-400" />
                  <text x={INTERCHANGE_X} y={b.midY - 20} textAnchor="middle" className="fill-foreground text-xs">
                    {b.interchangeLabel}
                  </text>
                </>
              )}
              {b.items.map((item, i) => {
                const y = stationY(b, i);
                const left = b.side === 'left';
                const labelX = left ? LOG_LABEL_X : LABEL_X;
                const anchor = left ? 'end' : 'start';
                return (
                  <g
                    key={`${item.key}:${item.color}`}
                    role="button"
                    tabIndex={0}
                    aria-label={`Open ${item.label}`}
                    className={cn('group cursor-pointer outline-hidden', flash(item) && 'map-flash-station')}
                    onClick={() => onSelect(item.target)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        onSelect(item.target);
                      }
                    }}>
                    <circle
                      cx={left ? LOG_STATION_X : STATION_X}
                      cy={y}
                      r={9}
                      className={cn('fill-background group-hover:brightness-125', STROKE[item.color])}
                      strokeWidth={4}
                    />
                    <text
                      x={labelX}
                      y={y - 6}
                      textAnchor={anchor}
                      className="fill-foreground group-hover:fill-sky-400 group-focus-visible:fill-sky-400 text-xs">
                      {item.label}
                    </text>
                    <text x={labelX} y={y + 12} textAnchor={anchor} className="fill-muted-foreground font-mono text-[10px]">
                      {item.caption}
                    </text>
                  </g>
                );
              })}
            </g>
          ))}
        </svg>
      </Card>
    </DetailPane>
  );
}

/**
 * One-shot flash when a station's color actually changes between polls
 * (relay → direct, a tunnel turning off, a log file appearing) — never on
 * first sight. Stations are keyed by `key:color`, so a change remounts the
 * element and the CSS animation runs exactly once; the flagged `key:color`
 * stays in `flashed` so later re-renders keep the class without restarting
 * it. `lastColor` only advances after commit, so StrictMode's double render
 * can't swallow a change.
 */
function useChangeFlash(stations: Station[]) {
  const lastColor = useRef(new Map<string, LineColor>());
  const flashed = useRef(new Set<string>());

  for (const s of stations) {
    const prev = lastColor.current.get(s.key);
    if (prev !== undefined && prev !== s.color) flashed.current.add(`${s.key}:${s.color}`);
  }

  useEffect(() => {
    for (const s of stations) lastColor.current.set(s.key, s.color);
  });

  return (s: Station) => flashed.current.has(`${s.key}:${s.color}`);
}
