import { useCallback, useLayoutEffect, useRef, type RefObject } from "react";

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

/** Within this many pixels of the bottom counts as "at the bottom". */
export const STICK_SLOP_PX = 40;

/**
 * Keep a scroll container at the bottom as content grows (a chat), but only
 * while the user is there: scrolling up to read stops it, scrolling back
 * down (or `pin()`, e.g. on sending a message) starts it again.
 * `content` is what grows (the chat's items).
 */
export function useStickToBottom(ref: RefObject<HTMLElement | null>, content: unknown): { pin: () => void; onScroll: () => void } {
  const pinned = useRef(true);
  const onScroll = useCallback(() => {
    const el = ref.current;
    if (el) pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_SLOP_PX;
  }, [ref]);
  const pin = useCallback(() => {
    pinned.current = true;
    const el = ref.current;
    el?.scrollTo({ top: el.scrollHeight });
  }, [ref]);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && pinned.current) el.scrollTo({ top: el.scrollHeight });
  }, [ref, content]);
  return { pin, onScroll };
}
