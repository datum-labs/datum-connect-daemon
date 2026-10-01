import { api, type Exchange, type Header, type ReplayResult } from '@/api';
import { CodeBlock } from '@/components/CodeBlock';
import { DetailSection } from '@/components/DetailPane';
import { KeyValueCard } from '@/components/KeyValueCard';
import { ToneBadge } from '@/components/ToneBadge';
import { statusTone } from '@/lib/format';
import { Button } from '@datum-cloud/datum-ui/button';
import { Dialog } from '@datum-cloud/datum-ui/dialog';
import { Skeleton } from '@datum-cloud/datum-ui/skeleton';
import { toast } from '@datum-cloud/datum-ui/toast';
import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { Text } from '@datum-cloud/datum-ui/typography';
import { PlayIcon } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';

function formatHeaders(headers: Header[]): string {
  return headers.map(([k, v]) => `${k}: ${v}`).join('\n');
}

function Headers({ headers }: { headers: Header[] }) {
  if (!headers.length) {
    return (
      <Text size="xs" textColor="muted">
        none
      </Text>
    );
  }
  const text = formatHeaders(headers);
  return <CodeBlock copyValue={text}>{text}</CodeBlock>;
}

function Body({ body }: { body: string }) {
  return <CodeBlock copyValue={body || undefined}>{body || '(empty)'}</CodeBlock>;
}

export function ExchangeDialog({
  tunnelId,
  exchangeId,
  token,
  onClose,
}: {
  tunnelId: string;
  exchangeId: string;
  token: string;
  onClose: () => void;
}) {
  const [exchange, setExchange] = useState<Exchange | null>(null);
  const [replay, setReplay] = useState<ReplayResult | null>(null);
  const [replaying, setReplaying] = useState(false);
  // The parent re-renders on every 3s poll with a fresh onClose; keep the
  // fetch keyed on the exchange only.
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  const replayRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (replay) replayRef.current?.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }, [replay]);
  const base = `/v1/tunnels/${encodeURIComponent(tunnelId)}/traffic/${encodeURIComponent(exchangeId)}`;

  useEffect(() => {
    let current = true;
    api<Exchange>(base, token)
      .then((ex) => current && setExchange(ex))
      .catch((e: Error) => {
        if (!current) return;
        toast.error(e.message);
        onCloseRef.current();
      });
    return () => {
      current = false;
    };
  }, [base, token]);

  const doReplay = async () => {
    setReplaying(true);
    try {
      setReplay(await api<ReplayResult>(`${base}/replay`, token, { method: 'POST' }));
      toast.success('Replayed successfully');
    } catch (e) {
      toast.error(`Replay failed: ${(e as Error).message}`);
    } finally {
      setReplaying(false);
    }
  };

  const truncated = !!exchange?.request.body_truncated;
  const replayButton = (
    <Button
      type="primary"
      theme="solid"
      size="xs"
      disabled={!exchange || truncated}
      loading={replaying}
      icon={<PlayIcon className="size-3.5" />}
      onClick={doReplay}>
      Replay this request
    </Button>
  );

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <Dialog.Content className="sm:max-w-3xl">
        <Dialog.Header
          title={<span className="font-mono text-sm break-all">{exchange ? `${exchange.method} ${exchange.path}` : 'Loading…'}</span>}
          onClose={onClose}
        />
        <Dialog.Body className="flex flex-col gap-5 px-5">
          {!exchange ? (
            <Skeleton className="h-40 w-full rounded-lg" />
          ) : (
            <>
              <KeyValueCard
                rows={[
                  ['Status', <ToneBadge tone={statusTone(exchange.response_status)}>{exchange.response_status ?? '—'}</ToneBadge>],
                  ['Time', new Date(exchange.timestamp_unix_ms).toLocaleString()],
                ]}
              />
              <DetailSection title="Request headers">
                <Headers headers={exchange.request.headers} />
              </DetailSection>
              <DetailSection title={`Request body${exchange.request.body_truncated ? ' (truncated)' : ''}`}>
                <Body body={exchange.request.body} />
              </DetailSection>
              {exchange.response && (
                <>
                  <DetailSection title="Response headers">
                    <Headers headers={exchange.response.headers} />
                  </DetailSection>
                  <DetailSection title={`Response body${exchange.response.body_truncated ? ' (truncated)' : ''}`}>
                    <Body body={exchange.response.body} />
                  </DetailSection>
                </>
              )}
              {replay && (
                <div ref={replayRef}>
                  <DetailSection title={`Replay result — status ${replay.status}`}>
                    <Headers headers={replay.headers} />
                    <Body body={replay.body} />
                  </DetailSection>
                </div>
              )}
            </>
          )}
        </Dialog.Body>
        <Dialog.Footer className="flex justify-end gap-2">
          <Button type="quaternary" theme="outline" size="xs" onClick={onClose}>
            Close
          </Button>
          {truncated ? (
            <Tooltip message="Truncated body cannot be replayed">
              <span tabIndex={0}>{replayButton}</span>
            </Tooltip>
          ) : (
            replayButton
          )}
        </Dialog.Footer>
      </Dialog.Content>
    </Dialog>
  );
}
