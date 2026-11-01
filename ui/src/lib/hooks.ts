import { useCallback, useLayoutEffect, useRef } from "react";

/**
 * A stable function that always calls the latest `fn` (React's planned
 * `useEffectEvent`). For effects that should re-run when a key changes but
 * read everything else fresh: list the key in the deps, call this inside.
 * Don't call it during render.
 */
export function useEvent<A extends unknown[], R>(fn: (...args: A) => R): (...args: A) => R {
  const ref = useRef(fn);
  useLayoutEffect(() => {
    ref.current = fn;
  });
  return useCallback((...args: A) => ref.current(...args), []);
}
