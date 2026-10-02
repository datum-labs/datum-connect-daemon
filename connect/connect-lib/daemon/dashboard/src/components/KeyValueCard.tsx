import { Card, CardContent, CardField, CardFieldLabel, CardFieldValue } from '@datum-cloud/datum-ui/card';

export function KeyValueCard({ rows }: { rows: [label: string, value: React.ReactNode][] }) {
  return (
    <Card size="sm" sectioned>
      <CardContent padding="none">
        {rows.map(([label, value]) => (
          <CardField key={label}>
            <CardFieldLabel>{label}</CardFieldLabel>
            <CardFieldValue className="font-mono text-xs break-all">{value}</CardFieldValue>
          </CardField>
        ))}
      </CardContent>
    </Card>
  );
}
