import { useEffect } from "react";
import { api } from "../lib/api";
import type { FindPlan } from "../lib/find";
import { useEvent } from "../lib/hooks";
import type { FindResult, ViewSpec } from "../lib/types";

export const FIND_DEBOUNCE_MS = 200;
export const FIND_LIMIT = 10_000;

/**
 * Find in a result's current view, debounced. Only the answer to the latest
 * search is reported: a slow response for an older term, view or result
 * arrives after the new one and is dropped.
 */
export function useFindInResult({
  open,
  plan,
  wholeWord,
  view,
  resultId,
  onResult,
  onError,
}: {
  open: boolean;
  plan: FindPlan;
  wholeWord: boolean;
  view: ViewSpec;
  resultId: string;
  onResult: (r: FindResult) => void;
  onError: (e: unknown) => void;
}) {
  const report = useEvent(onResult);
  const fail = useEvent(onError);
  useEffect(() => {
    if (!open || !plan.term) {
      report({ matches: [], truncated: false });
      return;
    }
    let stale = false;
    const t = setTimeout(async () => {
      try {
        const r = await api.findInResult(resultId, view, plan.term, FIND_LIMIT, plan.columns, wholeWord);
        if (!stale) report(r);
      } catch (e) {
        if (!stale) fail(e);
      }
    }, FIND_DEBOUNCE_MS);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [open, plan, wholeWord, view, resultId, report, fail]);
}
