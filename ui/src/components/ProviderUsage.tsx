import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ExternalLink, Gauge, Loader2, RefreshCw } from "lucide-react";
import { api } from "../lib/api";
import { useAi } from "../aiStore";
import type { Budget, BudgetMeter, ProviderUsage } from "../lib/types";
import { formatCount } from "../lib/util";
import { meterText } from "../lib/budget";

/** The meter shown in the panel footer: the first one with a limit, else the first. */
function mainMeter(b: Budget): BudgetMeter | undefined {
  return b.meters.find((m) => m.limit !== null && m.used !== null) ?? b.meters[0];
}

function Bar({ m }: { m: BudgetMeter }) {
  if (m.used === null || m.limit === null || m.limit <= 0) return null;
  const pct = Math.min(100, (m.used / m.limit) * 100);
  return (
    <div className="h-1 w-full overflow-hidden rounded bg-line" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(pct)} aria-label={m.label}>
      <div className={`h-full ${pct >= 90 ? "bg-danger" : pct >= 75 ? "bg-warning" : "bg-accent"}`} style={{ width: `${pct}%` }} />
    </div>
  );
}

const resetText = (d: string) => {
  const t = new Date(`${d}T00:00:00`);
  return Number.isNaN(t.getTime()) ? d : t.toLocaleDateString(undefined, { month: "short", day: "numeric" });
};

/** Budget, token totals and links, for Settings and the panel popover. */
export function UsageDetails({ providerId, usage, error, loading }: { providerId: string; usage?: ProviderUsage; error?: string; loading: boolean }) {
  const load = useAi((s) => s.loadUsage);
  const b = usage?.budget;
  const tok = (t: [number, number]) => `${formatCount(t[0])} in · ${formatCount(t[1])} out`;
  return (
    <div className="space-y-2 text-[12px]">
      <div className="flex items-center gap-2">
        <span className="font-medium">{b?.plan ?? "Usage"}</span>
        {b?.resets_on && <span className="text-[11px] text-muted">resets {resetText(b.resets_on)}</span>}
        <button className="icon-btn ml-auto" title="Refresh" aria-label="Refresh usage" disabled={loading} onClick={() => void load(providerId, true)}>
          {loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
        </button>
      </div>
      {b?.meters.map((m) => (
        <div key={m.label} className="space-y-0.5">
          <div className="flex justify-between gap-2">
            <span className="text-muted">{m.label}</span>
            <span className="font-mono text-[11.5px]">{meterText(m)}</span>
          </div>
          <Bar m={m} />
        </div>
      ))}
      {usage && !usage.budget_supported && <p className="text-[11px] text-muted">This provider doesn't report credits or a balance for an API key; check its billing page.</p>}
      {(usage?.budget_error || error) && <p className="text-[11px] text-danger">Couldn't read the budget: {usage?.budget_error ?? error}</p>}
      {usage && (
        <div className="space-y-0.5 border-t border-line pt-1.5 text-[11.5px]">
          <div className="flex justify-between">
            <span className="text-muted">Tokens today</span>
            <span className="font-mono">{tok(usage.tokens_today)}</span>
          </div>
          <div className="flex justify-between">
            <span className="text-muted">Tokens this month</span>
            <span className="font-mono">{tok(usage.tokens_month)}</span>
          </div>
          <p className="text-[10.5px] text-muted">Counted from DataBrain's conversations, as the provider reports them (Kiro reports credits, not tokens).</p>
        </div>
      )}
      {b?.manage_url && (
        <button className="inline-flex items-center gap-1 text-[11px] text-accent hover:underline" onClick={() => void api.openLink(b.manage_url!).catch(() => {})}>
          Manage plan / buy credits <ExternalLink size={10} />
        </button>
      )}
    </div>
  );
}

/** Footer of the AI panel: this chat's tokens/credits and the provider's
 *  budget; click for details. */
export function UsageFooter() {
  const providerId = useAi((s) => s.providerId);
  const providerKind = useAi((s) => s.providers.find((p) => p.id === s.providerId)?.kind);
  const tokens = useAi((s) => s.tokens);
  const credits = useAi((s) => s.credits);
  const context = useAi((s) => s.contextPercent);
  const entry = useAi((s) => (providerId ? s.usage[providerId] : undefined));
  const load = useAi((s) => s.loadUsage);
  const [open, setOpen] = useState(false);
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  // Providers with a budget API: load it once the panel shows.
  const hasBudget = providerKind === "kiro" || providerKind === "openrouter";
  useEffect(() => {
    if (providerId && (hasBudget || open)) void load(providerId);
  }, [providerId, hasBudget, open, load]);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => !pop.current?.contains(e.target as Node) && !btn.current?.contains(e.target as Node) && setOpen(false);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);
  if (!providerId) return null;
  const chat = [
    tokens.input || tokens.output ? `${formatCount(tokens.input)} in · ${formatCount(tokens.output)} out tokens` : null,
    credits > 0 ? `${credits.toFixed(2)} credits` : null,
    context !== null ? `context ${Math.round(context)}%` : null,
  ].filter(Boolean);
  const m = entry?.data?.budget ? mainMeter(entry.data.budget) : undefined;
  const r = btn.current?.getBoundingClientRect();
  return (
    <div className="mt-1 flex items-center gap-2 text-[10.5px] text-muted">
      {chat.length > 0 && <span title="This conversation">{chat.join(" · ")}</span>}
      <button
        ref={btn}
        type="button"
        className="ml-auto flex min-w-0 items-center gap-1 rounded px-1 hover:bg-hover hover:text-fg"
        aria-expanded={open}
        aria-haspopup="dialog"
        title="Budget and token use of this provider"
        onClick={() => setOpen((o) => !o)}
      >
        <Gauge size={11} className="shrink-0" />
        {m ? (
          <>
            <span className="truncate">{meterText(m)}</span>
            {m.limit !== null && m.used !== null && (
              <span className="w-12 shrink-0">
                <Bar m={m} />
              </span>
            )}
          </>
        ) : (
          <span>Usage</span>
        )}
        {entry?.loading && <Loader2 size={10} className="shrink-0 animate-spin" />}
      </button>
      {open &&
        r &&
        createPortal(
          <div
            ref={pop}
            role="dialog"
            aria-label="Provider usage"
            className="fixed z-[70] w-72 rounded-md border border-line bg-panel p-2.5 text-fg shadow-lg"
            style={{ right: Math.max(4, window.innerWidth - r.right), bottom: window.innerHeight - r.top + 4 }}
          >
            <UsageDetails providerId={providerId} usage={entry?.data} error={entry?.error} loading={!!entry?.loading} />
          </div>,
          document.body,
        )}
    </div>
  );
}
