import type { BudgetMeter } from "./types";

const fmt = (n: number, unit: string) => {
  const money = /^(USD|EUR|CNY|GBP)$/i.test(unit);
  const v = money ? n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 }) : n.toLocaleString(undefined, { maximumFractionDigits: n < 10 ? 2 : 0 });
  return money ? `${v} ${unit.toUpperCase()}` : `${v} ${unit}`;
};

/** "6,368 of 10,000 credits (64%)" / "8.75 USD left" / "1.25 USD spent". */
export function meterText(m: BudgetMeter): string {
  if (m.used !== null && m.limit !== null) return `${fmt(m.used, m.unit)} of ${fmt(m.limit, "").trim()} used (${Math.round((m.used / Math.max(m.limit, 1e-9)) * 100)}%)`;
  if (m.remaining !== null) return `${fmt(m.remaining, m.unit)} left`;
  if (m.used !== null) return `${fmt(m.used, m.unit)} spent${m.limit === null ? " (no limit)" : ""}`;
  return "—";
}

