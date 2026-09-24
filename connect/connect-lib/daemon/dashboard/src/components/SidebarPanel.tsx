import { ListToolbar } from '@/components/ListToolbar';
import { LogList } from '@/components/LogList';
import { NavEmpty } from '@/components/NavText';
import { PeerList } from '@/components/PeerList';
import { TunnelList } from '@/components/TunnelList';
import type { Loadable, useDashboard } from '@/hooks/useDashboard';
import { useStoredState } from '@/hooks/useStoredState';
import type { Info } from '@/api';
import { osLabel } from '@/lib/format';
import type { Selection } from '@/lib/selection';
import {
  isLocalTunnel,
  LOG_SORTS,
  type LogSort,
  PEER_SORTS,
  type PeerSort,
  type SidebarTab,
  TUNNEL_SORTS,
  type TunnelScope,
  type TunnelSort,
  visibleAdvertisements,
  visibleConnections,
  visibleLogs,
  visibleTunnels,
} from '@/lib/sidebar';
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarHeader,
  SidebarMenuSkeleton,
} from '@datum-cloud/datum-ui/sidebar';
import { Tabs, TabsList, TabsTrigger } from '@datum-cloud/datum-ui/tabs';
import { Text } from '@datum-cloud/datum-ui/typography';
import { cn } from '@datum-cloud/datum-ui/utils';
import { ChevronRightIcon, LaptopIcon } from 'lucide-react';
import { useState } from 'react';

type Dashboard = ReturnType<typeof useDashboard>;

const TABS = ['tunnels', 'peers', 'logs'] as const;

/**
 * Gate shared by all three tabs: no token → prompt, first poll in flight →
 * skeleton, poll failed → the error. Only renders `children` once data is in.
 */
function Gate<T>({
  hasToken,
  data,
  children,
}: {
  hasToken: boolean;
  data: Loadable<T>;
  children: (data: T) => React.ReactNode;
}) {
  if (!hasToken) return <NavEmpty title="Enter a viewer token above to connect." />;
  if (data.status === 'idle')
    return (
      <div className="flex flex-col gap-1">
        <SidebarMenuSkeleton showIcon />
        <SidebarMenuSkeleton showIcon />
        <SidebarMenuSkeleton showIcon />
      </div>
    );
  if (data.status === 'error') return <NavEmpty title={data.error} error />;
  return <>{children(data.data)}</>;
}

function count<T>(data: Loadable<T>, n: (d: T) => number): number | null {
  return data.status === 'ok' ? n(data.data) : null;
}

export function SidebarPanel({
  d,
  selection,
  onSelect,
}: {
  d: Dashboard;
  selection: Selection;
  onSelect: (selection: Selection) => void;
}) {
  const hasToken = !!d.token;
  const [tab, setTab] = useStoredState<SidebarTab>('dc_sidebar_tab', 'tunnels', TABS);
  const [tunnelSort, setTunnelSort] = useStoredState<TunnelSort>('dc_sort_tunnels', 'status', ['name', 'status', 'hostname']);
  const [peerSort, setPeerSort] = useStoredState<PeerSort>('dc_sort_peers', 'name', ['name', 'traffic']);
  const [logSort, setLogSort] = useStoredState<LogSort>('dc_sort_logs', 'name', ['name', 'size']);
  const [scope, setScope] = useStoredState<TunnelScope>('dc_tunnel_scope', 'device', ['device', 'all']);
  const [queries, setQueries] = useState<Record<SidebarTab, string>>({ tunnels: '', peers: '', logs: '' });
  const query = queries[tab];
  const setQuery = (q: string) => setQueries((prev) => ({ ...prev, [tab]: q }));

  const tunnelCount = count(d.tunnels, (t) => (scope === 'all' ? t.length : t.filter((x) => isLocalTunnel(d.info, x)).length));
  const peerCount = count(d.peers, (p) => p.advertisements.length + p.connections.length);
  const logCount = count(d.logSources, (l) => l.length);

  return (
    <Sidebar collapsible="none" className="h-full">
      <SidebarHeader className="gap-3 border-b p-0 pt-3">
        <Tabs value={tab} onValueChange={(v) => setTab(v as SidebarTab)} className="px-2">
          <TabsList variant="line" className="w-full">
            <TabTrigger value="tunnels" label="Tunnels" count={tunnelCount} />
            <TabTrigger value="peers" label="Peers" count={peerCount} />
            <TabTrigger value="logs" label="Logs" count={logCount} />
          </TabsList>
        </Tabs>
        {hasToken && tab === 'tunnels' && (
          <ListToolbar
            query={query}
            onQueryChange={setQuery}
            placeholder="Search tunnels…"
            filter={{
              value: scope,
              onChange: setScope,
              label: 'Which tunnels to show',
              options: {
                device: 'This device',
                all: `All devices${d.tunnels.status === 'ok' ? ` (${d.tunnels.data.length})` : ''}`,
              },
            }}
            sort={{ value: tunnelSort, onChange: setTunnelSort, options: TUNNEL_SORTS, label: 'Sort tunnels' }}
          />
        )}
        {hasToken && tab === 'peers' && (
          <ListToolbar
            query={query}
            onQueryChange={setQuery}
            placeholder="Search peers…"
            sort={{ value: peerSort, onChange: setPeerSort, options: PEER_SORTS, label: 'Sort peers' }}
          />
        )}
        {hasToken && tab === 'logs' && (
          <ListToolbar
            query={query}
            onQueryChange={setQuery}
            placeholder="Search logs…"
            sort={{ value: logSort, onChange: setLogSort, options: LOG_SORTS, label: 'Sort logs' }}
          />
        )}
      </SidebarHeader>

      <SidebarContent className="px-2 py-2">
        {tab === 'tunnels' && (
          <Gate hasToken={hasToken} data={d.tunnels}>
            {(all) => {
              if (!all.length) return <NavEmpty title="No tunnels in this project." />;
              const shown = visibleTunnels(all, { info: d.info, scope, query, sort: tunnelSort });
              if (shown.length) {
                return (
                  <TunnelList
                    tunnels={shown}
                    info={d.info}
                    selection={selection}
                    onSelect={(id) => onSelect({ kind: 'tunnel', id })}
                  />
                );
              }
              if (query) return <NavEmpty title={`No tunnels match “${query}”.`} />;
              return <NavEmpty title="No tunnels on this device — switch to All devices to see the rest of the project." />;
            }}
          </Gate>
        )}
        {tab === 'peers' && (
          <Gate hasToken={hasToken} data={d.peers}>
            {(p) => {
              if (!p.advertisements.length && !p.connections.length)
                return <NavEmpty title="No peer advertisements or connections." />;
              const ads = visibleAdvertisements(p.advertisements, { query, sort: peerSort });
              const conns = visibleConnections(p.connections, { query, sort: peerSort });
              if (!ads.length && !conns.length) return <NavEmpty title={`No peers match “${query}”.`} />;
              return (
                <PeerList
                  advertisements={ads}
                  connections={conns}
                  selection={selection}
                  onSelect={(kind, id) => onSelect({ kind, id })}
                />
              );
            }}
          </Gate>
        )}
        {tab === 'logs' && (
          <Gate hasToken={hasToken} data={d.logSources}>
            {(sources) => {
              if (!sources.length) return <NavEmpty title="No log sources registered." />;
              const shown = visibleLogs(sources, { query, sort: logSort });
              if (!shown.length) return <NavEmpty title={`No logs match “${query}”.`} />;
              return (
                <LogList sources={shown} selection={selection} onSelect={(name) => onSelect({ kind: 'log', name })} />
              );
            }}
          </Gate>
        )}
      </SidebarContent>

      <SidebarFooter className="gap-2 border-t px-2 py-2">
        <FooterNote d={d} tab={tab} />
        <DeviceButton
          info={d.info}
          active={selection?.kind === 'device'}
          onSelect={() => onSelect({ kind: 'device' })}
        />
      </SidebarFooter>
    </Sidebar>
  );
}

function TabTrigger({ value, label, count }: { value: SidebarTab; label: string; count: number | null }) {
  return (
    <TabsTrigger value={value} className="flex-1 gap-1.5 text-xs">
      {label}
      {count != null && <span className="text-muted-foreground font-mono text-[11px] tabular-nums">{count}</span>}
    </TabsTrigger>
  );
}

function FooterNote({ d, tab }: { d: Dashboard; tab: SidebarTab }) {
  const note =
    tab === 'peers'
      ? 'Peer tunnels are P2P — no Datum involved.'
      : tab === 'logs'
        ? `Showing up to ${d.logTailMaxLines} lines. If a log file rotates, you may lose the earlier lines from this view.`
        : null;
  if (!note) return null;
  return (
    <Text as="p" size="xs" textColor="muted" className="px-2 leading-snug">
      {note}
    </Text>
  );
}

/** Always-visible entry to the Device page, pinned to the sidebar footer. */
function DeviceButton({ info, active, onSelect }: { info: Info | null; active: boolean; onSelect: () => void }) {
  if (!info) return null;
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-current={active || undefined}
      data-active={active}
      className={cn(
        'flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left outline-hidden',
        'hover:bg-sidebar-accent focus-visible:ring-sidebar-ring focus-visible:ring-2',
        'data-[active=true]:bg-sidebar-accent data-[active=true]:shadow-[inset_2px_0_0_var(--primary)]',
      )}>
      <LaptopIcon className="text-muted-foreground size-4 shrink-0" />
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-sm font-medium">{info.device_name}</span>
        <span className="text-muted-foreground truncate text-[11px]">
          {osLabel(info)} · {info.arch}
        </span>
      </span>
      <ChevronRightIcon className="text-muted-foreground size-4 shrink-0" />
    </button>
  );
}
