import { useEffect, useRef } from 'react';

export const POLL_INTERVAL_MS = 3000;

/**
 * Runs `fn` immediately and then every `intervalMs`, restarting whenever
 * `deps` change. `fn` receives an `isCurrent()` check: once deps have
 * changed (or the component unmounted), an in-flight response from the
 * previous run reports false and must not be painted.
 *
 * That guard replaces the old page's manual `logDetailGeneration` counter —
 * found in review 2026-09-07: switching logs quickly, or an overlapping
 * poll, could otherwise let an old response overwrite the currently
 * displayed one.
 */
export function usePoll(
  fn: (isCurrent: () => boolean) => void | Promise<void>,
  deps: React.DependencyList,
  intervalMs: number = POLL_INTERVAL_MS,
) {
  const fnRef = useRef(fn);
  fnRef.current = fn;

  useEffect(() => {
    let current = true;
    const isCurrent = () => current;
    const run = () => void fnRef.current(isCurrent);
    run();
    const handle = setInterval(run, intervalMs);
    return () => {
      current = false;
      clearInterval(handle);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, intervalMs]);
}
