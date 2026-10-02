import type { Advertisement, PeerConnection } from '@/api';
import { NavGroupLabel } from '@/components/NavText';
import { NavItem } from '@/components/NavItem';
import { TextCopy } from '@/components/TextCopy';
import { connTypeTone, humanizeBytes } from '@/lib/format';
import { isSelected, type Selection } from '@/lib/selection';
import { SidebarMenu } from '@datum-cloud/datum-ui/sidebar';
import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { ArrowDownIcon, ArrowUpIcon, RefreshCwIcon } from 'lucide-react';

// Both advertisements and connections open in the right-hand detail pane
// on click, same list-then-detail pattern as tunnels and logs.
export function PeerList({
  advertisements,
  connections,
  selection,
  onSelect,
}: {
  advertisements: Advertisement[];
  connections: PeerConnection[];
  selection: Selection;
  onSelect: (kind: 'peer-ad' | 'peer-conn', id: string) => void;
}) {
  return (
    <>
      {advertisements.length > 0 && (
        <>
          <NavGroupLabel>Advertised · {advertisements.length}</NavGroupLabel>
          <SidebarMenu className="gap-0.5">
            {advertisements.map((a) => (
              <NavItem
                key={a.resource_id}
                active={isSelected(selection, 'peer-ad', a.resource_id)}
                onSelect={() => onSelect('peer-ad', a.resource_id)}
                status={a.enabled ? { tone: 'success', label: 'Advertising' } : { tone: 'muted', label: 'Revoked' }}
                title={a.label}
                trailing={
                  <span className="text-muted-foreground flex items-center gap-0.5 font-mono text-[11px]">
                    <ArrowUpIcon className="size-3" />
                    {humanizeBytes(a.bytes_from_origin)}
                  </span>
                }
                subtitle={<TextCopy value={a.endpoint} />}
              />
            ))}
          </SidebarMenu>
        </>
      )}
      {connections.length > 0 && (
        <>
          <NavGroupLabel>Connected · {connections.length}</NavGroupLabel>
          <SidebarMenu className="gap-0.5">
            {connections.map((c) => {
              const type = c.conn_type || 'unknown';
              return (
                <NavItem
                  key={c.id}
                  active={isSelected(selection, 'peer-conn', c.id)}
                  onSelect={() => onSelect('peer-conn', c.id)}
                  status={{ tone: connTypeTone(type), label: `${type} connection` }}
                  title={
                    <span className="flex items-center gap-1">
                      <ArrowDownIcon className="text-muted-foreground size-3 shrink-0" />
                      <span className="truncate font-mono text-xs">{c.target}</span>
                    </span>
                  }
                  trailing={
                    <>
                      {/* transition_count > 0 means this connection has flipped between
                          direct/relay at least once since it was established — each
                          flip is also a `peer_connection_<type>` audit event. */}
                      {c.transition_count > 0 && (
                        <Tooltip message={`${c.transition_count} type change${c.transition_count === 1 ? '' : 's'}`}>
                          <span className="text-warning flex items-center gap-0.5 text-[11px]">
                            <RefreshCwIcon className="size-3" />
                            {c.transition_count}
                          </span>
                        </Tooltip>
                      )}
                      <span className="text-muted-foreground font-mono text-[11px]">
                        {c.latency_ms != null ? `${c.latency_ms}ms` : type}
                      </span>
                    </>
                  }
                  subtitle={
                    <>
                      <span className="shrink-0">via</span>
                      <TextCopy value={c.bound_addr} />
                    </>
                  }
                />
              );
            })}
          </SidebarMenu>
        </>
      )}
    </>
  );
}
