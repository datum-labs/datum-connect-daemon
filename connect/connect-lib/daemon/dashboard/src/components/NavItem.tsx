import { StatusDot } from '@/components/StatusDot';
import type { Tone } from '@/lib/format';
import { SidebarMenuItem } from '@datum-cloud/datum-ui/sidebar';
import { cn } from '@datum-cloud/datum-ui/utils';

/**
 * A two-line selectable row: status dot + title (+ trailing badge), then a
 * muted subtitle. SidebarMenuButton is fixed-height single-line, so this
 * uses the same sidebar tokens on a plain element — with role/keyboard
 * handling, since the row contains its own copy buttons and so can't itself
 * be a <button>.
 */
export function NavItem({
  active,
  onSelect,
  status,
  title,
  trailing,
  subtitle,
  meta,
}: {
  active: boolean;
  onSelect: () => void;
  status: { tone: Tone; label: string };
  title: React.ReactNode;
  trailing?: React.ReactNode;
  subtitle?: React.ReactNode;
  meta?: React.ReactNode;
}) {
  return (
    <SidebarMenuItem>
      <div
        role="button"
        tabIndex={0}
        aria-current={active || undefined}
        data-active={active}
        onClick={onSelect}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            onSelect();
          }
        }}
        className={cn(
          'flex cursor-pointer flex-col gap-0.5 rounded-md px-2 py-1.5 outline-hidden',
          'hover:bg-sidebar-accent hover:text-sidebar-accent-foreground focus-visible:ring-sidebar-ring focus-visible:ring-2',
          'data-[active=true]:bg-sidebar-accent data-[active=true]:text-sidebar-accent-foreground data-[active=true]:shadow-[inset_2px_0_0_var(--primary)]',
        )}>
        <div className="flex min-w-0 items-center gap-2">
          <StatusDot tone={status.tone} label={status.label} />
          <span className="min-w-0 flex-1 truncate text-sm font-medium">{title}</span>
          {trailing && <span className="flex shrink-0 items-center gap-1">{trailing}</span>}
        </div>
        {subtitle && <div className="text-muted-foreground flex min-w-0 items-center gap-1 pl-4 text-xs">{subtitle}</div>}
        {meta && <div className="text-muted-foreground pl-4 text-[11px]">{meta}</div>}
      </div>
    </SidebarMenuItem>
  );
}
