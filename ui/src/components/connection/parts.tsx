// Pieces the connection form's sections share: field labels, checkboxes,
// secret fields, auth labels, default AI policy, table access rules.
import { useState } from "react";
import type { AiPolicy, AuthMethodKind, ConnectorKind, EnvTag } from "../../lib/types";
import { formatPatterns, parsePatterns, tableAccess } from "../../lib/aiAccess";

export const COLORS = ["#818cf8", "#22d3ee", "#34d399", "#fbbf24", "#f97316", "#f87171", "#e879f9", "#94a3b8"];
export const ENVS: { value: EnvTag; label: string }[] = [
  { value: "none", label: "None" },
  { value: "dev", label: "Dev" },
  { value: "staging", label: "Staging" },
  { value: "prod", label: "Production" },
];
export const rank = (k: ConnectorKind) => (KIND_ORDER.includes(k) ? KIND_ORDER.indexOf(k) : KIND_ORDER.length);
const KIND_ORDER: ConnectorKind[] = ["postgres", "mysql", "sqlite", "mssql", "oracle", "duckdb", "clickhouse", "snowflake", "databricks", "bigquery"];
export const TOP_LEVEL = new Set(["host", "port", "database", "file_path"]);

export const AUTH_LABELS: Record<AuthMethodKind, string> = {
  none: "None",
  password: "Password",
  api_token: "Access token",
  key_pair: "Key pair",
  oauth_browser: "Browser (OAuth)",
  device_code: "Device code",
  client_credentials: "Service principal",
  external_browser: "Browser SSO",
  cloud_cli: "CLI login",
  service_account: "Service account",
};

export const CLI_HELP: Partial<Record<ConnectorKind, string>> = {
  bigquery: "Uses Google Application Default Credentials (run `gcloud auth application-default login`).",
  databricks: "Uses the Databricks CLI login (`databricks auth login --host …`). Profile from ~/.databrickscfg (optional).",
  mssql: "Uses the Azure CLI login (`az login`) to get an Entra ID token.",
  postgres:
    "Gets a temporary Redshift password from the AWS CLI on each connect (`aws redshift get-cluster-credentials-with-iam`, or `aws redshift-serverless get-credentials`): profiles, SSO, MFA and assumed roles work as in the CLI. Sign in with `aws sso login` or `aws configure` first. The cluster or workgroup and region come from the host; the user is your IAM identity's database user unless a Database user is set.",
};

/** Label of the CLI login button per type (default "CLI login"). */
export const CLI_LABEL: Partial<Record<ConnectorKind, string>> = {
  postgres: "IAM (AWS CLI)",
};

/** Postgres connection settings that only Redshift uses. */
export const REDSHIFT_KEYS = ["cluster_id", "workgroup", "aws_region", "db_user"];

/** A Postgres connection is Redshift: its type says so, or its host is a Redshift endpoint. */
export function isRedshift(values: Record<string, string | undefined>): boolean {
  if (values.flavor) return values.flavor === "redshift";
  return /\.redshift(-serverless)?\.amazonaws\.com\.?$/i.test((values.host ?? "").trim());
}

export const TOKEN_LABEL: Partial<Record<ConnectorKind, string>> = {
  snowflake: "Programmatic access token",
  databricks: "Personal access token",
};

export const DEFAULT_POLICY: AiPolicy = {
  ai_enabled: true,
  allowed_providers: [],
  share_metadata: true,
  share_sample_values: false,
  share_result_rows: false,
  run_query: "ask",
  allow_write: false,
  max_rows_to_model: 50,
  pii_columns: [],
  index_schemas: [],
  index_batch: 25,
  mcp_enabled: false,
  auto_approve_notes: false,
  read_allow: [],
  read_deny: [],
  write_allow: [],
  write_deny: [],
  external_access: false,
};

export function Label({ children, htmlFor }: { children: React.ReactNode; htmlFor?: string }) {
  return (
    <label htmlFor={htmlFor} className="mb-1 block text-[11.5px] font-medium text-muted">
      {children}
    </label>
  );
}

export function Check({ checked, onChange, label, help }: { checked: boolean; onChange: (v: boolean) => void; label: string; help?: string }) {
  return (
    <label className="flex items-start gap-2 text-[13px]">
      <input type="checkbox" className="mt-0.5" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>
        {label}
        {help && <span className="block text-[11.5px] text-muted">{help}</span>}
      </span>
    </label>
  );
}

export type Section = "general" | "auth" | "ssh" | "ai";

/** Read and write table rules for the AI (LIKE patterns), with a "try a table" check. */
export function TableAccessFields({ policy, setPolicy }: { policy: AiPolicy; setPolicy: (p: AiPolicy) => void }) {
  // Kept as typed while editing; parsed into the policy on every change.
  const [text, setText] = useState(() => ({
    read_allow: formatPatterns(policy.read_allow),
    read_deny: formatPatterns(policy.read_deny),
    write_allow: formatPatterns(policy.write_allow),
    write_deny: formatPatterns(policy.write_deny),
  }));
  const [probe, setProbe] = useState("");
  type Key = keyof typeof text;
  const field = (key: Key, label: string, placeholder: string, disabled = false) => (
    <div>
      <Label htmlFor={`ai-${key}`}>{label}</Label>
      <textarea
        id={`ai-${key}`}
        className="field min-h-[52px] font-mono text-[12px]"
        value={text[key]}
        placeholder={placeholder}
        disabled={disabled}
        spellCheck={false}
        onChange={(e) => {
          setText({ ...text, [key]: e.target.value });
          setPolicy({ ...policy, [key]: parsePatterns(e.target.value) });
        }}
      />
    </div>
  );
  const list = (k: Key) => policy[k] ?? [];
  const read = probe.trim() ? tableAccess(list("read_allow"), list("read_deny"), probe.trim()) : null;
  const write = probe.trim() ? tableAccess(list("write_allow"), list("write_deny"), probe.trim()) : null;
  const any = (["read_allow", "read_deny", "write_allow", "write_deny"] as Key[]).some((k) => list(k).length > 0);
  return (
    <div className="space-y-2 rounded-lg border border-line p-3">
      <div className="text-[12.5px] font-medium">Tables the AI may use</div>
      <p className="text-[11.5px] text-muted">
        SQL LIKE patterns, comma separated (<span className="font-mono">%</span> any text, <span className="font-mono">_</span> one character, not case-sensitive), matched against{" "}
        <span className="font-mono">catalog.schema.table</span>, <span className="font-mono">schema.table</span> and <span className="font-mono">table</span>. Unqualified names in SQL use the
        connection&apos;s default database and schema. Empty &quot;like&quot; = every table; &quot;not like&quot; always wins. Applies to the assistant&apos;s queries, sample rows, results it reads
        (also from your own queries) and jobs it builds or runs, and to MCP clients.
      </p>
      <div className="grid grid-cols-2 gap-3">
        {field("read_allow", "AI read access: like", "'catalog_read%', '%schema_can_read%'")}
        {field("read_deny", "AI read access: not like", "'cannot_read%', '%.hr.%'")}
        {field("write_allow", "AI write access: like", "'%.scratch.%'", !policy.allow_write)}
        {field("write_deny", "AI write access: not like", "'%.prod_%'", !policy.allow_write)}
      </div>
      {!policy.allow_write && <p className="text-[11px] text-muted">Write rules apply once &quot;Allow write statements&quot; is on.</p>}
      {any && (
        <p className="text-[11px] text-muted">
          While rules are set, the assistant can&apos;t run SQL whose tables can&apos;t be told (procedure calls, unparsed SQL, Neo4j Cypher) or that changes the default schema (USE, SET search_path).
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <input
          className="field max-w-[260px] font-mono text-[12px]"
          placeholder="Try a table: catalog.schema.table"
          aria-label="Try a table name against the rules"
          value={probe}
          spellCheck={false}
          onChange={(e) => setProbe(e.target.value)}
        />
        {read && write && (
          <span className="text-[11.5px]" role="status">
            <span className={read.ok ? "text-success" : "text-danger"}>read {read.ok ? "allowed" : "denied"}</span>
            <span className="text-muted"> ({read.why})</span>
            {" · "}
            <span className={write.ok && policy.allow_write ? "text-success" : "text-danger"}>write {write.ok && policy.allow_write ? "allowed" : "denied"}</span>
            <span className="text-muted"> ({policy.allow_write ? write.why : "writes are off"})</span>
          </span>
        )}
      </div>
    </div>
  );
}

export function SecretField({
  label,
  value,
  onChange,
  placeholder,
  canClear,
  clear,
  onClear,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder: string;
  canClear?: boolean;
  clear?: boolean;
  onClear?: (v: boolean) => void;
}) {
  return (
    <div>
      <Label>{label}</Label>
      <input type="password" className="field" value={value} autoComplete="new-password" placeholder={placeholder} onChange={(e) => onChange(e.target.value)} />
      {canClear && onClear && (
        <label className="mt-1 flex items-center gap-1.5 text-[11.5px] text-muted">
          <input type="checkbox" checked={!!clear} onChange={(e) => onClear(e.target.checked)} />
          Remove saved secret
        </label>
      )}
    </div>
  );
}
