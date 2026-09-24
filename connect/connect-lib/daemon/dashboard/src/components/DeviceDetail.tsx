import type { Info, PeerData, Tunnel } from '@/api';
import { DetailPane, DetailSection, StatRow } from '@/components/DetailPane';
import { KeyValueCard } from '@/components/KeyValueCard';
import { StatCard } from '@/components/StatCard';
import { TextCopy } from '@/components/TextCopy';
import type { Loadable } from '@/hooks/useDashboard';
import { humanizeBytes, humanizeDuration, isAgentActor, osLabel } from '@/lib/format';
import { isLocalTunnel } from '@/lib/sidebar';
import { Alert, AlertDescription } from '@datum-cloud/datum-ui/alert';
import { LinkButton } from '@datum-cloud/datum-ui/button';
import { Skeleton } from '@datum-cloud/datum-ui/skeleton';
import { ExternalLinkIcon } from 'lucide-react';

/**
 * Everything the dashboard knows about the machine this daemon runs on:
 * the static facts from /v1/info (unauthenticated) plus live counts derived
 * from the tunnel and peer polls the sidebar already makes.
 */
export function DeviceDetail({
  info,
  tunnels,
  peers,
  logSourceCount,
}: {
  info: Info | null;
  tunnels: Loadable<Tunnel[]>;
  peers: Loadable<PeerData>;
  logSourceCount: number | null;
}) {
  if (!info) {
    return (
      <div className="p-8">
        <Skeleton className="h-40 w-full rounded-lg" />
      </div>
    );
  }

  const projectUrl = `${info.portal_base_url}/project/${info.project_id}`;
  const local = tunnels.status === 'ok' ? tunnels.data.filter((t) => isLocalTunnel(info, t)) : null;
  const peerData = peers.status === 'ok' ? peers.data : null;
  const m = peerData?.connect_metrics;
  const platform = `${osLabel(info)} · ${info.arch}`;

  return (
    <DetailPane
      title={info.device_name}
      description={
        <span className="text-sm">
          {platform} · daemon v{info.daemon_version} · up {humanizeDuration(Date.now() - info.started_at_unix_ms)}
        </span>
      }
      actions={
        <div className="flex gap-2">
          <LinkButton
            href={`${projectUrl}/connectors`}
            target="_blank"
            rel="noopener"
            type="secondary"
            theme="outline"
            size="xs"
            icon={<ExternalLinkIcon className="size-3.5" />}
            iconPosition="right">
            Connectors
          </LinkButton>
          <LinkButton
            href={projectUrl}
            target="_blank"
            rel="noopener"
            type="secondary"
            theme="outline"
            size="xs"
            icon={<ExternalLinkIcon className="size-3.5" />}
            iconPosition="right">
            Open project
          </LinkButton>
        </div>
      }>
      <DetailSection title="Tunnels on this device">
        {local ? (
          <StatRow>
            <StatCard label="Total" value={local.length} />
            <StatCard label="On" value={local.filter((t) => t.enabled).length} />
            <StatCard label="Connector ready" value={local.filter((t) => t.connector_ready).length} />
            <StatCard
              label="Started by agent"
              value={local.filter((t) => t.enabled && isAgentActor(t.last_start_actor)).length}
            />
            <StatCard label="Log sources" value={logSourceCount ?? '—'} />
          </StatRow>
        ) : (
          <Alert variant="outline">
            <AlertDescription>
              {tunnels.status === 'error' ? tunnels.error : 'Connect with a viewer token to see tunnel counts.'}
            </AlertDescription>
          </Alert>
        )}
      </DetailSection>

      <DetailSection title="Peer-to-peer">
        {peerData ? (
          <>
            <StatRow>
              <StatCard label="Advertised" value={peerData.advertisements.length} />
              <StatCard label="Connected" value={peerData.connections.length} />
              {m && (
                <>
                  <StatCard label="Sent" value={humanizeBytes(m.bytes_to_upstream)} />
                  <StatCard label="Received" value={humanizeBytes(m.bytes_from_upstream)} />
                  <StatCard label="Active conns" value={m.active_iroh_connections} />
                  <StatCard label="Total conns" value={m.total_iroh_connections} />
                </>
              )}
            </StatRow>
            {peerData.endpoint_id && (
              <KeyValueCard rows={[['Peer id', <TextCopy value={peerData.endpoint_id} className="text-xs" />]]} />
            )}
          </>
        ) : (
          <Alert variant="outline">
            <AlertDescription>Connect with a viewer token to see peer activity.</AlertDescription>
          </Alert>
        )}
      </DetailSection>

      <DetailSection title="Device & daemon">
        <KeyValueCard
          rows={[
            ['Device name', info.device_name],
            ['Hostname', info.hostname],
            ['Operating system', osLabel(info)],
            ['OS version', info.os_version],
            ['Architecture', info.arch],
            ['Daemon version', info.daemon_version],
            ['Started', new Date(info.started_at_unix_ms).toLocaleString()],
            ['Project', <TextCopy value={info.project_id} className="text-xs" />],
            ['Tunnel auto-stop', `after ${info.max_tunnel_hours}h enabled`],
            ['Log tail cap', `${info.log_tail_max_lines} lines`],
          ]}
        />
      </DetailSection>
    </DetailPane>
  );
}
