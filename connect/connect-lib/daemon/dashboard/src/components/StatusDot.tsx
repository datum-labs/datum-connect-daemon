import type { Tone } from '@/lib/format';
import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { cn } from '@datum-cloud/datum-ui/utils';

const TONE_CLASS: Record<Tone, string> = {
  success: 'bg-green-500',
  warning: 'bg-amber-400',
  danger: 'bg-red-500',
  muted: 'bg-muted-foreground/50',
};

export function StatusDot({ tone, label }: { tone: Tone; label: string }) {
  return (
    <Tooltip message={label} side="right">
      <span role="img" aria-label={label} className={cn('inline-block size-2 shrink-0 rounded-full', TONE_CLASS[tone])} />
    </Tooltip>
  );
}
