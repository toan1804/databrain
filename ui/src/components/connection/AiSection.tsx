// Connection form, AI: what the assistant may see and do on this connection.
import type { AiPolicy } from "../../lib/types";
import { Check, Label, TableAccessFields } from "./parts";
import { ConnectionFormState } from "./useConnectionForm";

export function AiSection({ f }: { f: ConnectionFormState }) {
  const { kind, policy, setPolicy } = f;
  return (
    <div className="space-y-3">
      <Check
        checked={policy.ai_enabled}
        onChange={(v) => setPolicy({ ...policy, ai_enabled: v })}
        label="Enable AI assistant for this connection"
      />
      <fieldset disabled={!policy.ai_enabled} className="space-y-3 disabled:opacity-50">
        <div className="grid grid-cols-2 gap-2">
          <Check checked={policy.share_metadata} onChange={(v) => setPolicy({ ...policy, share_metadata: v })} label="Share schema metadata" help="Table/column names, types, comments" />
          <Check checked={policy.share_sample_values} onChange={(v) => setPolicy({ ...policy, share_sample_values: v })} label="Share sample values" help="A few rows to understand columns" />
          <Check checked={policy.share_result_rows} onChange={(v) => setPolicy({ ...policy, share_result_rows: v })} label="Share result rows" help="Lets the AI analyze query output" />
          <Check checked={policy.allow_write} onChange={(v) => setPolicy({ ...policy, allow_write: v })} label="Allow write statements" help="Always asks; blocked on read-only" />
          <Check
            checked={policy.auto_approve_notes}
            onChange={(v) => setPolicy({ ...policy, auto_approve_notes: v })}
            label="Save AI notes directly"
            help="Notes the AI adds or updates while exploring skip review in Knowledge"
          />
          {kind === "duckdb" && (
            <Check
              checked={!!policy.external_access}
              onChange={(v) => setPolicy({ ...policy, external_access: v })}
              label="Let the AI use files and the network"
              help="Off: its SQL runs in a DuckDB sandbox (no file or network access, no extension loading, settings locked)"
            />
          )}
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <Label htmlFor="ai-run">AI may run queries</Label>
            <select id="ai-run" className="field" value={policy.run_query} onChange={(e) => setPolicy({ ...policy, run_query: e.target.value as AiPolicy["run_query"] })}>
              <option value="never">Never</option>
              <option value="ask">Ask me every time</option>
              <option value="auto_read">Automatically for read-only queries</option>
            </select>
          </div>
          <div>
            <Label htmlFor="ai-rows">Max rows sent to the model</Label>
            <input
              id="ai-rows"
              className="field"
              inputMode="numeric"
              value={policy.max_rows_to_model}
              onChange={(e) => setPolicy({ ...policy, max_rows_to_model: Number(e.target.value.replace(/\D/g, "")) || 0 })}
            />
          </div>
        </div>
        <div>
          <Label htmlFor="ai-pii">Sensitive columns (never sent; patterns, comma separated)</Label>
          <input
            id="ai-pii"
            className="field font-mono text-[12px]"
            value={policy.pii_columns.join(", ")}
            placeholder="email, *phone*, users.ssn"
            onChange={(e) => setPolicy({ ...policy, pii_columns: e.target.value.split(",").map((x) => x.trim()).filter(Boolean) })}
          />
        </div>
        <TableAccessFields policy={policy} setPolicy={setPolicy} />
        <div>
          <Label htmlFor="ai-schemas">Schemas to index for knowledge (empty = ask for large databases, * = all)</Label>
          <input
            id="ai-schemas"
            className="field font-mono text-[12px]"
            value={policy.index_schemas.join(", ")}
            placeholder="public, analytics, main.*"
            onChange={(e) => setPolicy({ ...policy, index_schemas: e.target.value.split(",").map((x) => x.trim()).filter(Boolean) })}
          />
        </div>
        <Check
          checked={policy.mcp_enabled}
          onChange={(v) => setPolicy({ ...policy, mcp_enabled: v })}
          label="Allow external agents (MCP)"
          help="Kiro CLI, Claude Code etc. can use this connection via the local databrain-mcp server, under the rules above. Their own tool approval counts as the approval (DataBrain doesn't ask again); what the rules forbid is still refused."
        />
      </fieldset>
    </div>
  );
}
