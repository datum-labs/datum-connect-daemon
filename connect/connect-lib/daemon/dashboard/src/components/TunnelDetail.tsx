import { api, type ExchangeSummary, type Info, type Metrics, type Progress, type Tunnel } from '@/api';
import { DetailPane, DetailSection, StatRow } from '@/components/DetailPane';
import { ExchangeDialog } from '@/components/ExchangeDialog';
import { StatCard } from '@/components/StatCard';
import { TextCopy } from '@/components/TextCopy';
import { ToneBadge } from '@/components/ToneBadge';
import { usePoll } from '@/hooks/usePoll';
import { humanizeBytes, isAgentActor, portalUrl, statusTone } from '@/lib/format';
import { Alert, AlertDescription } from '@datum-cloud/datum-ui/alert';
import { LinkButton } from '@datum-cloud/datum-ui/button';
import { Card } from '@datum-cloud/datum-ui/card';
import { Skeleton } from '@datum-cloud/datum-ui/skeleton';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@datum-cloud/datum-ui/table';
import { Text } from '@datum-cloud/datum-ui/typography';
import { BotIcon, ExternalLinkIcon } from 'lucide-react';
import { useState } from 'react';

interface Detail {
  progress: Progress;
  traffic: ExchangeSummary[];
  metrics: Metrics | null;
}

export function TunnelDetail({
  id,
  tunnel,
  token,
  info,
}: {
  id: string;
  tunnel: Tunnel | undefined;
  token: string;
  info: Info | null;
}) {
  const [detail, setDetail] = useState<{ id: string; data: Detail } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [openExchange, setOpenExchange] = useState<string | null>(null);

  usePoll(
    async (isCurrent) => {
      try {
        const base = `/v1/tunnels/${encodeURIComponent(id)}`;
        const [progress, traffic, metrics] = await Promise.all([
          api<Progress>(`${base}/progress`, token),
          api<ExchangeSummary[]>(`${base}/traffic`, token),
          // Not running → no metrics; that's a state, not an error.
          api<Metrics>(`${base}/metrics`, token).catch(() => null),
        ]);
        if (!isCurrent()) return;
        setDetail({ id, data: { progress, traffic, metrics } });
        setError(null);
      } catch (e) {
        if (isCurrent()) setError((e as Error).message);
      }
    },
    [id, token],
  );

  if (error && detail?.id !== id) {
    return (
      <div className="p-8">
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      </div>
    );
  }

  const data = detail?.id === id ? detail.data : null;
  const url = portalUrl(info, tunnel);
  const hostname = data?.progress.hostnames?.[0] ?? tunnel?.hostnames[0] ?? '';

  return (
    <DetailPane
      title={tunnel ? tunnel.label : id}
      description={
        <div className="flex flex-col gap-2 pt-1">
          {hostname && <TextCopy value={hostname} className="text-sm" />}
          {tunnel?.enabled && isAgentActor(tunnel.last_start_actor) && (
            <div>
              <ToneBadge tone="warning" icon={<BotIcon className="size-3" />}>
                Started by agent ({tunnel.last_start_actor})
              </ToneBadge>
            </div>
          )}
        </div>
      }
      actions={
        url && (
          <LinkButton
            href={url}
            target="_blank"
            rel="noopener"
            type="secondary"
            theme="outline"
            size="xs"
            icon={<ExternalLinkIcon className="size-3.5" />}
            iconPosition="right">
            Open in cloud.datum.net
          </LinkButton>
        )
      }>
      {!data ? (
        <DetailSkeleton />
      ) : (
        <>
          <DetailSection title="Progress">
            <div className="flex flex-wrap gap-2">
              {data.progress.steps.map((s) => (
                <Card key={s.kind} size="sm" className="min-w-[10rem] gap-1.5 px-4 py-3">
                  <Text size="xs" textColor="muted" className="font-mono uppercase">
                    {s.kind}
                  </Text>
                  <div>
                    <ToneBadge tone={s.status === 'ready' ? 'success' : 'warning'}>{s.status}</ToneBadge>
                  </div>
                </Card>
              ))}
            </div>
          </DetailSection>

          <DetailSection title="Network">
            {data.metrics ? (
              <StatRow>
                <StatCard label="Sent" value={humanizeBytes(data.metrics.bytes_to_origin)} />
                <StatCard label="Received" value={humanizeBytes(data.metrics.bytes_from_origin)} />
                <StatCard label="Active conns" value={data.metrics.active_iroh_connections} />
                <StatCard label="Total conns" value={data.metrics.total_iroh_connections} />
                <StatCard label="Active reqs" value={data.metrics.active_requests} />
                <StatCard label="Failed reqs" value={data.metrics.failed_requests} bad={!!data.metrics.failed_requests} />
                <StatCard label="Denied reqs" value={data.metrics.denied_requests} bad={!!data.metrics.denied_requests} />
              </StatRow>
            ) : (
              <Alert variant="outline">
                <AlertDescription>Tunnel isn't running — no live network stats</AlertDescription>
              </Alert>
            )}
          </DetailSection>

          <DetailSection title="Captured traffic">
            {data.traffic.length ? (
              <Card size="sm" sectioned className="overflow-hidden py-0">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead className="w-24">Method</TableHead>
                      <TableHead>Path</TableHead>
                      <TableHead className="w-24">Status</TableHead>
                      <TableHead className="w-32">Time</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {data.traffic
                      .slice()
                      .reverse()
                      .map((ex) => (
                        <TableRow key={ex.id} className="cursor-pointer" onClick={() => setOpenExchange(ex.id)}>
                          <TableCell className="font-mono font-semibold">{ex.method}</TableCell>
                          <TableCell className="max-w-0 truncate font-mono text-xs">{ex.path}</TableCell>
                          <TableCell>
                            <ToneBadge tone={statusTone(ex.response_status)}>{ex.response_status ?? '—'}</ToneBadge>
                          </TableCell>
                          <TableCell className="text-muted-foreground text-xs">
                            {new Date(ex.timestamp_unix_ms).toLocaleTimeString()}
                          </TableCell>
                        </TableRow>
                      ))}
                  </TableBody>
                </Table>
              </Card>
            ) : (
              <Alert variant="outline">
                <AlertDescription>No traffic captured yet</AlertDescription>
              </Alert>
            )}
          </DetailSection>
        </>
      )}
      {openExchange && (
        <ExchangeDialog tunnelId={id} exchangeId={openExchange} token={token} onClose={() => setOpenExchange(null)} />
      )}
    </DetailPane>
  );
}

function DetailSkeleton() {
  return (
    <div className="flex flex-col gap-4">
      <Skeleton className="h-16 w-full max-w-xl rounded-lg" />
      <Skeleton className="h-20 w-full rounded-lg" />
      <Skeleton className="h-48 w-full rounded-lg" />
    </div>
  );
}
