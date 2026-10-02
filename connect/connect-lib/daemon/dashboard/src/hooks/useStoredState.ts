import { useState } from 'react';

/**
 * useState that remembers its value in localStorage — for per-viewer view
 * preferences (active tab, sort order) only. Storage can be blocked, so
 * every access is guarded and the default is always a valid fallback.
 */
export function useStoredState<T extends string>(key: string, fallback: T, allowed: readonly T[]) {
  const [value, setValue] = useState<T>(() => {
    try {
      const stored = localStorage.getItem(key) as T | null;
      return stored && allowed.includes(stored) ? stored : fallback;
    } catch {
      return fallback;
    }
  });

  const set = (next: T) => {
    setValue(next);
    try {
      localStorage.setItem(key, next);
    } catch {
      // storage unavailable — keep the in-memory value
    }
  };

  return [value, set] as const;
}
