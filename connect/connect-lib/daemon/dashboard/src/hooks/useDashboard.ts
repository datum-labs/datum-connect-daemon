import { api, fetchInfo, type Info, type LogSource, type PeerData, type Tunnel } from '@/api';
import { isAgentActor } from '@/lib/format';
import { usePoll } from '@/hooks/usePoll';
import { toast } from '@datum-cloud/datum-ui/toast';
import { useEffect, useRef, useState } from 'react';

const TOKEN_KEY = 'dc_token';

/** Used only until /v1/info resolves. */
export const FALLBACK_LOG_TAIL_MAX_LINES = 1000;

export type Loadable<T> = { status: 'idle' } | { status: 'ok'; data: T } | { status: 'error'; error: string };

function readStoredToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) || '';
  } catch {
    return '';
  }
}

/**
 * Everything the left-hand navigation needs, polled every 3s: tunnels,
 * peer advertisements/connections, and registered log sources. Detail
 * panes poll their own data (see the components under `detail/`).
 */
export function useDashboard() {
  const [token, setTokenState] = useState(readStoredToken);
  const [info, setInfo] = useState<Info | null>(null);
  const [tunnels, setTunnels] = useState<Loadable<Tunnel[]>>({ status: 'idle' });
  const [peers, setPeers] = useState<Loadable<PeerData>>({ status: 'idle' });
  const [logSources, setLogSources] = useState<Loadable<LogSource[]>>({ status: 'idle' });
  const previousTunnels = useRef<Tunnel[]>([]);

  useEffect(() => {
    fetchInfo().then(setInfo).catch(() => {});
  }, []);

  const setToken = (next: string) => {
    try {
      localStorage.setItem(TOKEN_KEY, next);
    } catch {
      // storage blocked — the token still works for this session
    }
    setTokenState(next);
  };

  usePoll(
    async (isCurrent) => {
      if (!token) {
        setTunnels({ status: 'idle' });
        return;
      }
      try {
        const next = await api<Tunnel[]>('/v1/tunnels', token);
        if (!isCurrent()) return;
        // Visible signal: notice the moment an agent (an operate token, not
        // a human's setup token) turns a tunnel on — compared against the
        // previous poll so this fires once per start, not every 3s for as
        // long as it stays on. The persistent "started by agent" badge (see
        // TunnelList) is what answers "wait, why is that on" after the toast
        // is gone. See NOTES.md's "agent-friendly, human stays in control"
        // discussion, 2026-09-08.
        for (const t of next) {
          const was = previousTunnels.current.find((p) => p.id === t.id);
          const justTurnedOn = t.enabled && (!was || !was.enabled);
          if (justTurnedOn && isAgentActor(t.last_start_actor)) {
            toast.warning(`Agent started tunnel "${t.label}"`, { duration: 10000 });
          }
        }
        previousTunnels.current = next;
        setTunnels({ status: 'ok', data: next });
      } catch (e) {
        if (isCurrent()) setTunnels({ status: 'error', error: (e as Error).message });
      }
    },
    [token],
  );

  usePoll(
    async (isCurrent) => {
      if (!token) {
        setPeers({ status: 'idle' });
        return;
      }
      try {
        const next = await api<PeerData>('/v1/peers', token);
        if (isCurrent()) setPeers({ status: 'ok', data: next });
      } catch (e) {
        if (isCurrent()) setPeers({ status: 'error', error: (e as Error).message });
      }
    },
    [token],
  );

  usePoll(
    async (isCurrent) => {
      if (!token) {
        setLogSources({ status: 'idle' });
        return;
      }
      try {
        const next = await api<LogSource[]>('/v1/logs', token);
        if (isCurrent()) setLogSources({ status: 'ok', data: next });
      } catch (e) {
        if (isCurrent()) setLogSources({ status: 'error', error: (e as Error).message });
      }
    },
    [token],
  );

  // How many log lines are shown is the daemon's own configured cap
  // (--log-tail-max-lines / DATUM_LOG_TAIL_MAX_LINES, via /v1/info) — not a
  // UI input, and not a separate client-side constant either, so there's
  // exactly one number governing "how much history you can see".
  const logTailMaxLines = info?.log_tail_max_lines || FALLBACK_LOG_TAIL_MAX_LINES;

  return {
    token,
    setToken,
    info,
    tunnels,
    peers,
    logSources,
    logTailMaxLines,
    connected: tunnels.status === 'ok',
  };
}
