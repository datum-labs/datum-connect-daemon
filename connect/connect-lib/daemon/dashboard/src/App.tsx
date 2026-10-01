import type { Tunnel } from '@/api';
import { AppHeader } from '@/components/AppHeader';
import { DeviceDetail } from '@/components/DeviceDetail';
import { LogDetail } from '@/components/LogDetail';
import { NetworkMap, type StationTarget } from '@/components/NetworkMap';
import { PeerAdDetail, PeerConnDetail } from '@/components/PeerDetail';
import { SidebarPanel, TABS } from '@/components/SidebarPanel';
import { TunnelDetail } from '@/components/TunnelDetail';
import { useDashboard } from '@/hooks/useDashboard';
import { useStoredState } from '@/hooks/useStoredState';
import type { Selection } from '@/lib/selection';
import type { SidebarTab } from '@/lib/sidebar';
import { EmptyContent } from '@datum-cloud/datum-ui/empty-content';
import { SidebarInset, SidebarProvider } from '@datum-cloud/datum-ui/sidebar';
import { useEffect, useState } from 'react';

export function App() {
  const d = useDashboard();
  const [tab, setTab] = useStoredState<SidebarTab>('dc_sidebar_tab', 'tunnels', TABS);
  // The Map tab has no list of its own to pick from, so it is its own
  // selection — including when the page reloads with it remembered.
  const [selection, setSelection] = useState<Selection>(() => (tab === 'map' ? { kind: 'map' } : null));

  const changeTab = (next: SidebarTab) => {
    setTab(next);
    if (next === 'map') setSelection({ kind: 'map' });
  };

  // A map station opens the same view its sidebar row would, and switches
  // to that row's tab so it shows as selected there.
  const openStation = (target: StationTarget) => {
    setTab(target.kind === 'tunnel' ? 'tunnels' : target.kind === 'log' ? 'logs' : 'peers');
    setSelection(target);
  };

  const peerData = d.peers.status === 'ok' ? d.peers.data : null;
  const logSources = d.logSources.status === 'ok' ? d.logSources.data : null;
  const tunnels = d.tunnels.status === 'ok' ? d.tunnels.data : [];

  // The selected advertisement/connection/log vanished (revoked,
  // disconnected, or removed via CLI — possibly from another client) since
  // the last poll: drop back to the empty state rather than showing, or
  // tailing, something that no longer exists.
  useEffect(() => {
    if (!selection) return;
    if (selection.kind === 'peer-ad' && peerData && !peerData.advertisements.some((a) => a.resource_id === selection.id))
      setSelection(null);
    if (selection.kind === 'peer-conn' && peerData && !peerData.connections.some((c) => c.id === selection.id))
      setSelection(null);
    if (selection.kind === 'log' && logSources && !logSources.some((s) => s.name === selection.name)) setSelection(null);
  }, [selection, peerData, logSources]);

  return (
    <div className="flex h-svh w-full flex-col overflow-hidden">
      <AppHeader
        token={d.token}
        onTokenChange={d.setToken}
        connected={d.connected}
        error={d.tunnels.status === 'error' ? d.tunnels.error : undefined}
      />
      <SidebarProvider
        className="min-h-0 flex-1 overflow-hidden"
        style={{ '--sidebar-width': '21rem' } as React.CSSProperties}>
        <SidebarPanel d={d} tab={tab} onTabChange={changeTab} selection={selection} onSelect={setSelection} />
        <SidebarInset className="min-h-0 overflow-y-auto">
          <Detail selection={selection} d={d} tunnels={tunnels} onOpenStation={openStation} />
        </SidebarInset>
      </SidebarProvider>
    </div>
  );
}

function Detail({
  selection,
  d,
  tunnels,
  onOpenStation,
}: {
  selection: Selection;
  d: ReturnType<typeof useDashboard>;
  tunnels: Tunnel[];
  onOpenStation: (target: StationTarget) => void;
}) {
  // The Device page is useful before a token is entered too: its static
  // facts come from the unauthenticated /v1/info.
  if (selection?.kind === 'device') {
    return (
      <DeviceDetail
        info={d.info}
        tunnels={d.tunnels}
        peers={d.peers}
        logSourceCount={d.logSources.status === 'ok' ? d.logSources.data.length : null}
      />
    );
  }
  if (selection && d.token) {
    switch (selection.kind) {
      case 'map':
        return (
          <NetworkMap
            info={d.info}
            tunnels={tunnels}
            peers={d.peers.status === 'ok' ? d.peers.data : null}
            logs={d.logSources.status === 'ok' ? d.logSources.data : []}
            onSelect={onOpenStation}
          />
        );
      case 'tunnel':
        return (
          <TunnelDetail
            key={selection.id}
            id={selection.id}
            tunnel={tunnels.find((t) => t.id === selection.id)}
            token={d.token}
            info={d.info}
          />
        );
      case 'log':
        return (
          <LogDetail
            key={selection.name}
            name={selection.name}
            source={d.logSources.status === 'ok' ? d.logSources.data.find((s) => s.name === selection.name) : undefined}
            token={d.token}
            maxLines={d.logTailMaxLines}
          />
        );
      case 'peer-ad': {
        const ad = d.peers.status === 'ok' ? d.peers.data.advertisements.find((a) => a.resource_id === selection.id) : undefined;
        if (ad) return <PeerAdDetail ad={ad} />;
        break;
      }
      case 'peer-conn': {
        const conn = d.peers.status === 'ok' ? d.peers.data.connections.find((c) => c.id === selection.id) : undefined;
        if (conn) return <PeerConnDetail conn={conn} />;
        break;
      }
    }
  }
  return (
    <div className="flex h-full items-center justify-center p-8">
      <EmptyContent title="select a tunnel, log, or peer connection." />
    </div>
  );
}
