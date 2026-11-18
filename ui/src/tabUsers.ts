// Background work that runs on a tab's database session: a full export or an
// "every row" load runs the result's query again there, and closing the tab
// stops it. The tabs store asks before closing such a tab, and tells the
// work why it stopped; the stores that run it register here (so the tabs
// store doesn't import them).

export interface TabUser {
  /** What runs on tab `tabId`'s session now ("Export to all.csv"). */
  uses: (tabId: string) => string[];
  /** Tab `tabId` closes: what runs on it stops, and should say why. */
  closing: (tabId: string) => void;
}

const users: TabUser[] = [];

export function registerTabUser(u: TabUser) {
  users.push(u);
}

export const tabUses = (tabId: string): string[] => users.flatMap((u) => u.uses(tabId));

export function tabClosing(tabId: string) {
  for (const u of users) u.closing(tabId);
}
