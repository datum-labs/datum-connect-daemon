import type { Info, Tunnel } from '@/api';
import { NavItem } from '@/components/NavItem';
import { TextCopy } from '@/components/TextCopy';
import { isAgentActor } from '@/lib/format';
import { isSelected, type Selection } from '@/lib/selection';
import { isLocalTunnel } from '@/lib/sidebar';
import { SidebarMenu } from '@datum-cloud/datum-ui/sidebar';
import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { BotIcon, StickyNoteIcon } from 'lucide-react';

function tunnelStatus(t: Tunnel) {
  if (t.enabled && t.connector_ready) return { tone: 'success', label: 'On · connector ready' } as const;
  if (t.enabled) return { tone: 'warning', label: 'On · connector not ready' } as const;
  return { tone: 'muted', label: 'Off' } as const;
}

export function TunnelList({
  tunnels,
  info,
  selection,
  onSelect,
}: {
  tunnels: Tunnel[];
  info: Info | null;
  selection: Selection;
  onSelect: (id: string) => void;
}) {
  return (
    <SidebarMenu className="gap-0.5">
      {tunnels.map((t) => {
        const host = t.hostnames[0] || t.endpoint;
        return (
          <NavItem
            key={t.id}
            active={isSelected(selection, 'tunnel', t.id)}
            onSelect={() => onSelect(t.id)}
            status={tunnelStatus(t)}
            title={t.label}
            trailing={
              t.enabled &&
              isAgentActor(t.last_start_actor) && (
                <Tooltip message={`Started by agent (${t.last_start_actor})`}>
                  <span className="text-warning flex items-center" aria-label="Started by agent">
                    <BotIcon className="size-3.5" />
                  </span>
                </Tooltip>
              )
            }
            subtitle={host ? <TextCopy value={host} /> : '(no hostname yet)'}
            meta={
              (t.note || !isLocalTunnel(info, t)) && (
                <div className="flex min-w-0 flex-col gap-0.5">
                  {t.note && (
                    <span className="flex min-w-0 items-center gap-1">
                      <StickyNoteIcon className="size-3 shrink-0" />
                      <span className="truncate">{t.note}</span>
                    </span>
                  )}
                  {!isLocalTunnel(info, t) && <span>on {t.connector_device || 'unknown device'}</span>}
                </div>
              )
            }
          />
        );
      })}
    </SidebarMenu>
  );
}
