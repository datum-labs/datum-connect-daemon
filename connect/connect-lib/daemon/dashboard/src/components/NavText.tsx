import { Text } from '@datum-cloud/datum-ui/typography';

/**
 * Sidebar-sized empty/error line. datum-ui's EmptyContent (greeting +
 * illustration) is reserved for page-level empty states, as in cloud-portal.
 */
export function NavEmpty({ title, error }: { title: string; error?: boolean }) {
  return (
    <Text as="p" size="xs" textColor={error ? 'destructive' : 'muted'} className="px-2 py-3 leading-snug break-words">
      {title}
    </Text>
  );
}

export function NavGroupLabel({ children }: { children: React.ReactNode }) {
  return (
    <Text as="h4" size="xs" weight="medium" textColor="muted" className="px-2 pt-3 pb-1 tracking-wider uppercase first:pt-1">
      {children}
    </Text>
  );
}
