import type { LogSource } from '@/api';
import { NavItem } from '@/components/NavItem';
import { humanizeBytes } from '@/lib/format';
import { isSelected, type Selection } from '@/lib/selection';
import { SidebarMenu } from '@datum-cloud/datum-ui/sidebar';

// Which files are tailable is a setup-only decision made outside the
// dashboard entirely (CLI/API); the dashboard is a pure viewer, same as
// every other section here. See LOG-TAIL-PLAN.md.
export function LogList({
  sources,
  selection,
  onSelect,
}: {
  sources: LogSource[];
  selection: Selection;
  onSelect: (name: string) => void;
}) {
  return (
    <SidebarMenu className="gap-0.5">
      {sources.map((src) => (
        <NavItem
          key={src.name}
          active={isSelected(selection, 'log', src.name)}
          onSelect={() => onSelect(src.name)}
          status={src.exists ? { tone: 'success', label: 'File present' } : { tone: 'muted', label: 'No file yet' }}
          title={src.name}
          trailing={
            <span className="text-muted-foreground font-mono text-[11px]">
              {src.exists ? humanizeBytes(src.size_bytes) : '—'}
            </span>
          }
          subtitle={<span className="truncate font-mono">{src.path}</span>}
        />
      ))}
    </SidebarMenu>
  );
}
