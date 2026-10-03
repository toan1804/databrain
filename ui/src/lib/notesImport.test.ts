import { describe, expect, it } from "vitest";
import { aiConflicts, aiMergePrompt, defaultChoices, exportFileName, mergeTexts, previewSummary, toActions } from "./notesImport";
import type { ImportItem, KnNote } from "./types";

const kn = (id: string, body: string, target: string | null = null): KnNote => ({ id, connection_id: "c", target, body, author: "user", status: "approved", created_at: 0 });
const items: ImportItem[] = [
  { incoming: { body: "Orders with status 9 are tests", target: "public.orders" }, kind: "new", existing: [] },
  { incoming: { body: "MRR = monthly recurring revenue" }, kind: "same", existing: [] },
  { incoming: { body: "Active customer: ordered in 90 days" }, kind: "conflict", existing: [kn("e1", "Active customer: logged in this month")] },
  { incoming: { body: "Churn: no order for 180 days" }, kind: "conflict", existing: [kn("e2", "Churn: cancelled subscription")] },
];

describe("notesImport", () => {
  it("defaults to adding new notes and keeping mine on conflicts", () => {
    const c = defaultChoices(items);
    expect(toActions(items, c)).toEqual([{ incoming: items[0].incoming, action: "add", existing_ids: [] }]);
    expect(previewSummary(items)).toEqual({ new: 1, same: 1, conflict: 2 });
  });

  it("maps each conflict choice to an action; AI merges are sent to the assistant", () => {
    const c = defaultChoices(items);
    c.conflicts[2] = "merge";
    c.conflicts[3] = "ai";
    const acts = toActions(items, c);
    expect(acts[1]).toMatchObject({ action: "merge", existing_ids: ["e1"], body: "Active customer: logged in this month\nActive customer: ordered in 90 days" });
    expect(acts).toHaveLength(2);
    c.conflicts[2] = "theirs";
    expect(toActions(items, c)[1]).toMatchObject({ action: "replace", existing_ids: ["e1"] });
    c.conflicts[2] = "both";
    expect(toActions(items, c)[1]).toMatchObject({ action: "add", existing_ids: [] });
    const forAi = aiConflicts(items, c);
    expect(forAi.map((i) => i.existing[0].id)).toEqual(["e2"]);
    const prompt = aiMergePrompt(forAi, "Prod");
    expect(prompt).toContain('from "Prod"');
    expect(prompt).toContain("update_knowledge_note");
    expect(prompt).toContain("existing (id e2): Churn: cancelled subscription");
    expect(prompt).toContain("imported: Churn: no order for 180 days");
  });

  it("merges texts without repeating lines and names export files", () => {
    expect(mergeTexts(["a\nb"], "B\nc")).toBe("a\nb\nc");
    expect(exportFileName("Shop DB (prod)")).toBe("shop-db-prod.databrain-notes.json");
    expect(exportFileName("???")).toBe("notes.databrain-notes.json");
  });
});
