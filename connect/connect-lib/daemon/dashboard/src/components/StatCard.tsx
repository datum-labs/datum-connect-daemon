import { Card, CardContent } from '@datum-cloud/datum-ui/card';
import { Text } from '@datum-cloud/datum-ui/typography';
import { cn } from '@datum-cloud/datum-ui/utils';

export function StatCard({
  label,
  value,
  bad,
}: {
  label: string;
  value: React.ReactNode;
  /** Highlights a nonzero failure/denial count. */
  bad?: boolean;
}) {
  return (
    <Card size="sm" className="min-w-[8.5rem] gap-1 py-3">
      <CardContent className="flex flex-col gap-1 px-4">
        <Text size="xs" textColor="muted" className="tracking-wide uppercase">
          {label}
        </Text>
        <div className={cn('font-mono text-base font-semibold', bad && 'text-destructive')}>{value}</div>
      </CardContent>
    </Card>
  );
}
