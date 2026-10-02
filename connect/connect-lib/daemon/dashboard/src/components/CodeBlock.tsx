import { useCopyToClipboard } from '@datum-cloud/datum-ui/hooks';
import { cn } from '@datum-cloud/datum-ui/utils';
import { CheckIcon, CopyIcon } from 'lucide-react';
import { useState } from 'react';

/**
 * Monospace block for headers, bodies and log tails. datum-ui has no code
 * block component; this follows cloud-portal's `SnippetBlock`
 * (features/service-account/components/key-reveal-panel.tsx).
 */
export function CodeBlock({
  children,
  copyValue,
  className,
  preRef,
  onScroll,
}: {
  children: React.ReactNode;
  copyValue?: string;
  className?: string;
  preRef?: React.Ref<HTMLPreElement>;
  onScroll?: React.UIEventHandler<HTMLPreElement>;
}) {
  const [, copy] = useCopyToClipboard();
  const [copied, setCopied] = useState(false);

  return (
    <div className="bg-muted relative rounded-md border">
      <pre
        ref={preRef}
        onScroll={onScroll}
        className={cn(
          'overflow-x-auto p-3 font-mono text-xs leading-relaxed break-all whitespace-pre-wrap',
          copyValue != null && 'pr-10',
          className,
        )}>
        {children}
      </pre>
      {copyValue != null && (
        <button
          type="button"
          aria-label={copied ? 'Copied' : 'Copy'}
          className="focus-visible:ring-ring absolute top-2 right-2 rounded-md p-1.5 transition-colors hover:bg-white/10 focus-visible:ring-2 focus-visible:outline-none"
          onClick={() =>
            copy(copyValue, { withToast: true, toastMessage: 'Copied to clipboard' }).then((ok) => {
              if (!ok) return;
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            })
          }>
          {copied ? (
            <CheckIcon className="text-success size-4" />
          ) : (
            <CopyIcon className="text-muted-foreground size-4" />
          )}
        </button>
      )}
    </div>
  );
}
