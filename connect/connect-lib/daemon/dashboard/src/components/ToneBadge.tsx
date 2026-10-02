import type { Tone } from '@/lib/format';
import { Badge } from '@datum-cloud/datum-ui/badge';
import { cn } from '@datum-cloud/datum-ui/utils';

/**
 * datum-ui Badge keyed by semantic tone, with cloud-portal's BadgeStatus
 * treatment (compact, uppercase, tracked) for status labels.
 */
export function ToneBadge({
  tone,
  children,
  className,
  icon,
}: {
  tone: Tone;
  children: React.ReactNode;
  className?: string;
  icon?: React.ReactNode;
}) {
  return (
    <Badge
      type={tone}
      theme="light"
      className={cn(
        'gap-1 px-1.5 py-0.5 font-mono text-[10px] font-semibold tracking-[0.03em] uppercase',
        className,
      )}>
      {icon}
      {children}
    </Badge>
  );
}
