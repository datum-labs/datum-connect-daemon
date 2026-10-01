import type { Advertisement, PeerConnection } from '@/api';
import { DetailPane, StatRow } from '@/components/DetailPane';
import { KeyValueCard } from '@/components/KeyValueCard';
import { StatCard } from '@/components/StatCard';
import { TextCopy } from '@/components/TextCopy';
import { ToneBadge } from '@/components/ToneBadge';
import { connTypeTone, humanizeBytes } from '@/lib/format';
import { Alert, AlertDescription } from '@datum-cloud/datum-ui/alert';

// Peer details are rendered from the sidebar's 3s /v1/peers poll, so
// bytes/latency stay live without a second request.

export function PeerAdDetail({ ad }: { ad: Advertisement }) {
  return (
    <DetailPane
      title={ad.label}
      description={
        <div className="flex flex-wrap items-center gap-2 pt-1">
          <TextCopy value={ad.endpoint} className="text-sm" />
          <ToneBadge tone={ad.enabled ? 'success' : 'muted'}>{ad.enabled ? 'Advertising' : 'Revoked'}</ToneBadge>
        </div>
      }>
      <StatRow>
        <StatCard label="Sent" value={humanizeBytes(ad.bytes_from_origin)} />
        <StatCard label="Received" value={humanizeBytes(ad.bytes_to_origin)} />
      </StatRow>
      <KeyValueCard rows={[['Resource id', ad.resource_id]]} />
    </DetailPane>
  );
}

export function PeerConnDetail({ conn }: { conn: PeerConnection }) {
  const type = conn.conn_type || 'unknown';
  return (
    <DetailPane
      title="P2P connection"
      description={
        <div className="flex flex-wrap items-center gap-2 pt-1 text-sm">
          <TextCopy value={conn.bound_addr} />
          <span className="text-muted-foreground">→</span>
          <TextCopy value={conn.target} />
        </div>
      }>
      <StatRow>
        <StatCard label="Type" value={<ToneBadge tone={connTypeTone(type)}>{type}</ToneBadge>} />
        <StatCard label="Latency" value={conn.latency_ms != null ? `${conn.latency_ms}ms` : '—'} />
        <StatCard label="Type changes" value={conn.transition_count} />
      </StatRow>
      <KeyValueCard
        rows={[
          ['Remote peer id', conn.remote_endpoint_id],
          ...(conn.conn_detail ? ([['Path', conn.conn_detail]] as [string, string][]) : []),
        ]}
      />
      {conn.transition_count > 0 && (
        <Alert variant="info">
          <AlertDescription>
            Each type change (e.g. relay → direct once hole-punching succeeds) is also recorded in the audit log.
          </AlertDescription>
        </Alert>
      )}
    </DetailPane>
  );
}
