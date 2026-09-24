import { api, type LogSource, type LogTail } from '@/api';
import { CodeBlock } from '@/components/CodeBlock';
import { DetailPane } from '@/components/DetailPane';
import { usePoll } from '@/hooks/usePoll';
import { InputGroup, InputGroupAddon, InputGroupInput } from '@datum-cloud/datum-ui/input-group';
import { SearchIcon } from 'lucide-react';
import { useLayoutEffect, useRef, useState } from 'react';

export function LogDetail({
  name,
  source,
  token,
  maxLines,
}: {
  name: string;
  source: LogSource | undefined;
  token: string;
  maxLines: number;
}) {
  const [lines, setLines] = useState<{ name: string; lines: string[] } | null>(null);
  const [search, setSearch] = useState('');
  const preRef = useRef<HTMLPreElement>(null);
  const wasNearBottom = useRef(true);

  // Only fetches the tail for whichever log is actually selected — sources
  // you aren't looking at don't get tailed every poll.
  usePoll(
    async (isCurrent) => {
      try {
        const res = await api<LogTail>(`/v1/logs/${encodeURIComponent(name)}/tail?lines=${maxLines}`, token);
        if (isCurrent()) setLines({ name, lines: res.lines });
      } catch (e) {
        if (isCurrent()) setLines({ name, lines: [(e as Error).message] });
      }
    },
    [name, token, maxLines],
  );

  // Client-side substring filter over whatever's currently loaded (see
  // LOG-TAIL-PLAN.md's search section), so typing needs no round-trip.
  const all = lines?.name === name ? lines.lines : [];
  const term = search.trim().toLowerCase();
  const shown = term ? all.filter((l) => l.toLowerCase().includes(term)) : all;
  const text = shown.length ? shown.join('\n') : lines?.name !== name ? 'Loading…' : term ? '(no matching lines)' : '(empty)';

  // Only stick to the bottom on new content if the viewer was already
  // there — don't yank someone back down mid-scroll-up.
  useLayoutEffect(() => {
    const pre = preRef.current;
    if (pre && wasNearBottom.current) pre.scrollTop = pre.scrollHeight;
  }, [text]);

  return (
    <DetailPane
      title={name}
      description={
        <span className="font-mono text-xs">
          {source?.path}
          {source && !source.exists && ' (no file yet)'}
        </span>
      }>
      <InputGroup className="max-w-md">
        <InputGroupAddon>
          <SearchIcon />
        </InputGroupAddon>
        <InputGroupInput
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder="Filter lines (e.g. Error)…"
          aria-label="Filter log lines"
          className="h-full border-0 bg-transparent shadow-none focus-visible:ring-0 focus-visible:ring-offset-0"
        />
      </InputGroup>
      <CodeBlock
        preRef={preRef}
        copyValue={shown.join('\n')}
        className="max-h-[calc(100vh-16rem)] overflow-y-auto"
        onScroll={(e) => {
          const pre = e.currentTarget;
          wasNearBottom.current = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 30;
        }}>
        {text}
      </CodeBlock>
    </DetailPane>
  );
}
