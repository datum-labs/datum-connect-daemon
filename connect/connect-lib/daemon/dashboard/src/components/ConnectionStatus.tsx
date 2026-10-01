import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { cn } from '@datum-cloud/datum-ui/utils';

/** Pulse dot adapted from cloud-portal's `components/status-pulse-dot`. */
export function ConnectionStatus({ connected, error }: { connected: boolean; error?: string }) {
  const label = connected ? 'Connected to daemon' : error ? `Not connected: ${error}` : 'Not connected';
  return (
    <Tooltip message={label}>
      <span className="relative flex size-6 items-center justify-center" role="img" aria-label={label}>
        <span
          className={cn(
            'size-2.5 rounded-full',
            connected ? 'shadow-[0_0_0_3px_rgba(34,197,94,0.4)]' : 'shadow-[0_0_0_3px_rgba(239,68,68,0.4)]',
          )}
        />
        <span
          className={cn('absolute size-2.5 rounded-full', connected ? 'animate-pulse bg-green-500' : 'bg-red-500')}
        />
      </span>
    </Tooltip>
  );
}
