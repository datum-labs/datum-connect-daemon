import { ConnectionStatus } from '@/components/ConnectionStatus';
import { Button } from '@datum-cloud/datum-ui/button';
import { Input } from '@datum-cloud/datum-ui/input';
import { Logo } from '@datum-cloud/datum-ui/logo';
import { Text } from '@datum-cloud/datum-ui/typography';
import { useState } from 'react';

export function AppHeader({
  token,
  onTokenChange,
  connected,
  error,
}: {
  token: string;
  onTokenChange: (token: string) => void;
  connected: boolean;
  error?: string;
}) {
  const [draft, setDraft] = useState(token);

  return (
    <header className="bg-background flex h-14 shrink-0 items-center gap-4 border-b px-4">
      <div className="flex items-center gap-3">
        <Logo.Flat tone="mono-light" className="h-6 w-auto" />
        <span className="bg-border h-5 w-px" aria-hidden />
        <Text size="sm" weight="medium">
          Connect
        </Text>
        <Text size="xs" textColor="muted" className="hidden sm:inline">
          dashboard (read-only)
        </Text>
      </div>
      <form
        className="ml-auto flex items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          onTokenChange(draft.trim());
        }}>
        <ConnectionStatus connected={connected} error={error} />
        <Input
          type="password"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Viewer token (tunnel api viewer-token create)"
          aria-label="Viewer token"
          autoComplete="off"
          className="h-7 w-72 font-mono text-xs"
        />
        <Button type="primary" theme="solid" size="xs" htmlType="submit">
          Connect
        </Button>
      </form>
    </header>
  );
}
