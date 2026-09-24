import { PageTitle } from '@datum-cloud/datum-ui/page-title';
import { Text } from '@datum-cloud/datum-ui/typography';

/** Shared shell for every right-hand detail view. */
export function DetailPane({
  title,
  description,
  actions,
  children,
}: {
  title: string;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="mx-auto flex w-full max-w-6xl flex-col gap-6 p-6 md:p-8">
      <PageTitle
        title={title}
        titleClassName="text-2xl break-all"
        description={description}
        descriptionClassName="text-muted-foreground max-w-none"
        actions={actions}
        actionsClassName="shrink-0"
      />
      {children}
    </div>
  );
}

export function DetailSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <Text as="h3" size="xs" weight="medium" textColor="muted" className="tracking-wider uppercase">
        {title}
      </Text>
      {children}
    </section>
  );
}

export function StatRow({ children }: { children: React.ReactNode }) {
  return <div className="flex flex-wrap gap-3">{children}</div>;
}
