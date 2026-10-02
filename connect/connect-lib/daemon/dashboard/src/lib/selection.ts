/**
 * The right-hand pane shows exactly one thing at a time — a single
 * discriminated union rather than separate selected-tunnel/log/peer fields
 * that each had to be cleared by hand.
 */
export type Selection =
  | { kind: 'tunnel'; id: string }
  | { kind: 'log'; name: string }
  | { kind: 'peer-ad'; id: string }
  | { kind: 'peer-conn'; id: string }
  | { kind: 'device' }
  | { kind: 'map' }
  | null;

export function isSelected(selection: Selection, kind: NonNullable<Selection>['kind'], key: string): boolean {
  if (!selection || selection.kind !== kind) return false;
  if (selection.kind === 'device' || selection.kind === 'map') return true;
  return (selection.kind === 'log' ? selection.name : selection.id) === key;
}
