import { Button } from '@datum-cloud/datum-ui/button';
import { useCopyToClipboard } from '@datum-cloud/datum-ui/hooks';
import { toast } from '@datum-cloud/datum-ui/toast';
import { Tooltip } from '@datum-cloud/datum-ui/tooltip';
import { cn } from '@datum-cloud/datum-ui/utils';
import { CheckIcon, CopyIcon } from 'lucide-react';
import { useState } from 'react';

/**
 * A hostname / IP:port with a small inline copy button. Used everywhere one
 * of those is displayed (tunnel list/detail, peer advertisements/
 * connections) so the behavior stays consistent in one place. Adapted from
 * cloud-portal's `components/text-copy/text-copy.tsx`.
 */
export function TextCopy({ value, className }: { value: string; className?: string }) {
  const [, copy] = useCopyToClipboard();
  const [copied, setCopied] = useState(false);

  if (!value) return null;

  return (
    <span className={cn('inline-flex max-w-full min-w-0 items-center gap-1 font-mono', className)}>
      <span className="truncate">{value}</span>
      <Tooltip message={copied ? 'Copied' : 'Copy to clipboard'}>
        <Button
          type="quaternary"
          theme="borderless"
          size="xs"
          htmlType="button"
          aria-label="Copy to clipboard"
          className="size-5 shrink-0 px-0 focus-visible:ring-0 focus-visible:ring-offset-0"
          onClick={(event) => {
            // Don't also trigger a parent row's select handler.
            event.preventDefault();
            event.stopPropagation();
            copy(value)
              .then((ok) => {
                if (!ok) throw new Error('clipboard unavailable');
                setCopied(true);
                setTimeout(() => setCopied(false), 1000);
              })
              .catch((err: Error) => toast.error(`Copy failed: ${err.message}`));
          }}>
          {copied ? <CheckIcon className="text-success size-3" /> : <CopyIcon className="size-3" />}
        </Button>
      </Tooltip>
    </span>
  );
}
