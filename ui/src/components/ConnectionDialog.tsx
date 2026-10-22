import { useEffect, useMemo, useRef, useState } from "react";
import { traitsOf } from "../lib/sourceTraits";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { CheckCircle2, Copy, FilePlus2, FolderOpen, FolderPlus, Globe, Info, Loader2, LogOut, Plus, Square, Trash2, XCircle } from "lucide-react";
import { api, toError } from "../lib/api";
import type {
  AiPolicy,
  AuthMethod,
  AuthMethodKind,
  AuthStatus,
  ConnectionProfile,
  ConnectorKind,
  EnvTag,
  OAuthParams,
  SshConfig,
  SslMode,
} from "../lib/types";
import { useStore, type ConnectionPreset } from "../store";
import { OracleAgentPanel, OracleClientPanel } from "./OracleClient";
import { AskAiAboutError, connectionErrorQuestion } from "./AskAiError";
import { Modal } from "./ui";
import { STORAGE_PROVIDERS, bucketNoun, providerOf, usesSecret, type StorageAuth } from "../lib/storage";
import type { StorageBucket } from "../lib/types";

const COLORS = ["#818cf8", "#22d3ee", "#34d399", "#fbbf24", "#f97316", "#f87171", "#e879f9", "#94a3b8"];
const ENVS: { value: EnvTag; label: string }[] = [
  { value: "none", label: "None" },
  { value: "dev", label: "Dev" },
  { value: "staging", label: "Staging" },
  { value: "prod", label: "Production" },
];
const rank = (k: ConnectorKind) => (KIND_ORDER.includes(k) ? KIND_ORDER.indexOf(k) : KIND_ORDER.length);
const KIND_ORDER: ConnectorKind[] = ["postgres", "mysql", "sqlite", "mssql", "oracle", "duckdb", "snowflake", "databricks", "bigquery"];
const TOP_LEVEL = new Set(["host", "port", "database", "file_path"]);

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

const CLI_HELP: Partial<Record<ConnectorKind, string>> = {
  bigquery: "Uses Google Application Default Credentials (run `gcloud auth application-default login`).",
  databricks: "Uses the Databricks CLI login (`databricks auth login --host …`). Profile from ~/.databrickscfg (optional).",
  mssql: "Uses the Azure CLI login (`az login`) to get an Entra ID token.",
};

const TOKEN_LABEL: Partial<Record<ConnectorKind, string>> = {
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
};

function Label({ children, htmlFor }: { children: React.ReactNode; htmlFor?: string }) {
  return (
    <label htmlFor={htmlFor} className="mb-1 block text-[11.5px] font-medium text-muted">
      {children}
    </label>
  );
}

function Check({ checked, onChange, label, help }: { checked: boolean; onChange: (v: boolean) => void; label: string; help?: string }) {
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

export function ConnectionDialog() {
  const { open, profile, folderId, preset } = useStore((s) => s.connectionDialog);
  if (!open) return null;
  return <ConnectionForm key={profile?.id ?? "new"} initial={profile ?? null} folderId={folderId ?? null} preset={profile ? undefined : preset} />;
}

type Section = "general" | "auth" | "storage" | "ssh" | "ai";

function ConnectionForm({ initial, folderId, preset }: { initial: ConnectionProfile | null; folderId: string | null; preset?: ConnectionPreset }) {
  const connectors = useStore((s) => s.connectors);
  const closeDialog = useStore((s) => s.closeConnectionDialog);
  /** Id of the running Test, so Cancel (or closing the dialog) can stop it. */
  const testId = useRef<string | null>(null);
  const [testSecs, setTestSecs] = useState(0);
  const close = () => {
    if (testId.current) void api.cancelTestConnection(testId.current).catch(() => {});
    closeDialog();
  };
  const toast = useStore((s) => s.toast);
  const isEdit = !!initial;
  const sorted = useMemo(
    // Known kinds in this order, new ones after them by name.
    () => [...connectors].sort((a, b) => rank(a.kind) - rank(b.kind) || a.display_name.localeCompare(b.display_name)),
    [connectors],
  );

  const [section, setSection] = useState<Section>(preset?.section ?? "general");
  const [kind, setKind] = useState<ConnectorKind>(initial?.config.kind ?? preset?.kind ?? sorted[0]?.kind ?? "sqlite");
  const info = connectors.find((c) => c.kind === kind);
  const [name, setName] = useState(initial?.name ?? "");
  const [values, setValues] = useState<Record<string, string>>(() => {
    const c = initial?.config;
    return {
      host: c?.host ?? "",
      port: c?.port ? String(c.port) : "",
      database: c?.database ?? "",
      file_path: c?.file_path ?? "",
      ...(c?.options ?? preset?.options ?? {}),
    };
  });
  const setValue = (k: string, v: string) => setValues((s) => ({ ...s, [k]: v }));

  // ---- auth
  const initialAuth = initial?.config.auth;
  const [authKind, setAuthKind] = useState<AuthMethodKind>(initialAuth?.method ?? info?.auth_methods[0] ?? "none");
  const [user, setUser] = useState(() => {
    const a = initialAuth as { user?: string | null } | undefined;
    return a?.user ?? "";
  });
  const [oauth, setOauth] = useState<OAuthParams>(() => {
    if (initialAuth && ["oauth_browser", "device_code", "client_credentials"].includes(initialAuth.method)) {
      const rest: OAuthParams & { method?: string } = { ...(initialAuth as OAuthParams) };
      delete rest.method;
      return rest;
    }
    return {};
  });
  const [cliProfile, setCliProfile] = useState(initialAuth?.method === "cloud_cli" ? (initialAuth.profile ?? "") : "");
  const [secret, setSecret] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [clearSecret, setClearSecret] = useState(false);
  const [authStatus, setAuthStatus] = useState<AuthStatus | null>(null);

  // ---- ssh
  const [sshOn, setSshOn] = useState(!!initial?.config.ssh);
  const [ssh, setSsh] = useState<SshConfig>(
    initial?.config.ssh ?? { host: "", port: 22, user: "", auth: { method: "agent" }, host_key_fingerprint: null },
  );
  const [sshSecret, setSshSecret] = useState("");

  const [ssl, setSsl] = useState<SslMode>(initial?.config.ssl_mode ?? "prefer");
  const [readOnly, setReadOnly] = useState(initial?.config.read_only ?? false);
  const [color, setColor] = useState(initial?.color ?? COLORS[0]);
  const [env, setEnv] = useState<EnvTag>(initial?.env ?? "none");
  const [policy, setPolicy] = useState<AiPolicy>({ ...DEFAULT_POLICY, ...(initial?.ai_policy ?? {}) });
  const [busy, setBusy] = useState<"test" | "save" | "signin" | null>(null);
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);

  const interactive = ["oauth_browser", "device_code", "external_browser"].includes(authKind);
  useEffect(() => {
    if (!initial || !interactive) return;
    api.authStatus(initial.id).then(setAuthStatus).catch(() => setAuthStatus(null));
  }, [initial, interactive]);

  const changeKind = (k: ConnectorKind) => {
    setKind(k);
    setTestResult(null);
    const ci = connectors.find((c) => c.kind === k);
    setAuthKind(ci?.auth_methods[0] ?? "none");
    setValues((v) => ({ ...v, host: k === "databricks" ? "" : v.host || "localhost", port: "" }));
    if (!ci?.capabilities.ssh) setSshOn(false);
  };

  const usesFile = info?.uses_file ?? false;
  const fields = info?.fields ?? [];

  // DuckDB: the object storage access key id is the auth user, its secret the keychain secret.
  const storageProvider = kind === "duckdb" ? providerOf(values.storage) : undefined;
  const storageAuth: StorageAuth = (values.storage_auth as StorageAuth) || "keys";
  // S3 family keys: access key id (user) + secret. Azure secrets (account key,
  // SAS, connection string, client secret) are a token.
  const storageSecret = !!storageProvider && usesSecret(storageAuth);
  const storageKeys = storageSecret && storageProvider?.id !== "azure";

  const buildAuth = (): AuthMethod => {
    const u = user.trim();
    if (kind === "duckdb") return storageKeys ? { method: "password", user: u } : storageSecret ? { method: "api_token", user: null } : { method: "none" };
    const o: OAuthParams = Object.fromEntries(
      Object.entries(oauth).filter(([, v]) => v !== undefined && v !== null && String(v).trim() !== ""),
    );
    switch (authKind) {
      case "password":
        return { method: "password", user: u };
      case "api_token":
        return { method: "api_token", user: u || null };
      case "key_pair":
        return { method: "key_pair", user: u };
      case "oauth_browser":
      case "device_code":
      case "client_credentials":
        return { method: authKind, ...o, ...(u && authKind === "oauth_browser" ? { user: u } : {}) };
      case "external_browser":
        return { method: "external_browser", user: u };
      case "cloud_cli":
        return { method: "cloud_cli", profile: cliProfile.trim() || null };
      case "service_account":
        return { method: "service_account" };
      default:
        return { method: "none" };
    }
  };

  const buildProfile = (): ConnectionProfile => {
    const options: Record<string, string> = {};
    for (const f of fields) {
      if (TOP_LEVEL.has(f.key)) continue;
      const v = (values[f.key] ?? "").trim();
      if (v) options[f.key] = v;
    }
    const top = (k: string) => (fields.some((f) => f.key === k) ? (values[k] ?? "").trim() || null : null);
    return {
      id: initial?.id ?? "",
      name: name.trim() || defaultName(),
      config: {
        kind,
        host: top("host"),
        port: top("port") ? Number(values.port) : null,
        database: top("database"),
        file_path: top("file_path"),
        auth: buildAuth(),
        ssl_mode: ssl,
        read_only: readOnly,
        options,
        ssh: sshOn ? { ...ssh, host: ssh.host.trim(), user: ssh.user.trim() } : null,
      },
      color,
      env,
      folder_id: initial ? (initial.folder_id ?? null) : folderId,
      has_secret: initial?.has_secret ?? false,
      ai_policy: policy,
      created_at: initial?.created_at ?? 0,
      updated_at: initial?.updated_at ?? 0,
    };
  };

  const defaultName = () => {
    if (kind === "duckdb") {
      const first = (values.files ?? "").split("\n").find((l) => l.trim());
      if (storageProvider && !values.file_path?.trim() && !first) {
        const bucket = (values.storage_bucket ?? "").replace(/^[a-z0-9]+:\/\//i, "").split("/")[0];
        return `${storageProvider.label.split(" (")[0]}${bucket ? ` · ${bucket}` : ""}`;
      }
      return values.file_path?.trim() ? values.file_path.split(/[\\/]/).pop()! : first ? `DuckDB · ${first.split(/[\\/]/).pop()}` : "DuckDB";
    }
    if (usesFile) return values.file_path?.split(/[\\/]/).pop() || info?.display_name || kind;
    const main = values.account || values.project || values.host || "localhost";
    return `${values.database || values.catalog || values.dataset || info?.display_name || kind}@${main}`;
  };

  const needsUser = kind === "duckdb" ? storageKeys : ["password", "key_pair", "external_browser"].includes(authKind);
  const needsClientId =
    ["device_code", "client_credentials"].includes(authKind) || (authKind === "oauth_browser" && kind !== "databricks");
  const portValid = !values.port || (/^\d+$/.test(values.port) && +values.port > 0 && +values.port < 65536);
  const missing = fields.filter((f) => f.required && !(values[f.key] ?? "").trim()).map((f) => f.label);
  if (needsUser && !user.trim()) missing.push(kind === "duckdb" ? "Access key ID" : "User");
  if (storageProvider?.id === "s3_compatible" && !(values.storage_endpoint ?? "").trim()) missing.push("Endpoint");
  if (storageProvider?.id === "azure" && storageAuth !== "connection_string" && !(values.storage_account ?? "").trim()) missing.push("Storage account");
  if (storageProvider?.id === "azure" && storageAuth === "service_principal" && !(values.storage_tenant ?? "").trim()) missing.push("Tenant ID");
  if (storageProvider?.id === "azure" && storageAuth === "service_principal" && !(values.storage_client_id ?? "").trim()) missing.push("Client ID");
  if (storageProvider?.id === "r2" && !(values.storage_account_id ?? "").trim() && !(values.storage_endpoint ?? "").trim()) missing.push("Cloudflare account ID");
  if (needsClientId && !oauth.client_id?.trim()) missing.push("OAuth client ID");
  if (sshOn && (!ssh.host.trim() || !ssh.user.trim())) missing.push("SSH host/user");
  const canSubmit = missing.length === 0 && portValid;

  const secretSlots = (): Record<string, string> => {
    const extra: Record<string, string> = {};
    if (sshSecret) extra.ssh = sshSecret;
    if (!sshOn && initial?.config.ssh) extra.ssh = "";
    if (authKind === "key_pair" && passphrase) extra.passphrase = passphrase;
    return extra;
  };

  const test = async () => {
    const id = crypto.randomUUID();
    testId.current = id;
    setBusy("test");
    setTestResult(null);
    setTestSecs(0);
    const started = Date.now();
    const tick = setInterval(() => setTestSecs(Math.floor((Date.now() - started) / 1000)), 1000);
    try {
      const p = buildProfile();
      const r = await api.testConnection(p.config, secret || null, isEdit && !clearSecret ? p.id : null, sshSecret || null, id);
      if (testId.current === id) setTestResult({ ok: true, text: `Connected in ${r.latency_ms} ms — ${r.server_version}` });
    } catch (e) {
      const err = toError(e);
      if (testId.current === id) setTestResult({ ok: false, text: err.kind === "cancelled" ? "Test cancelled" : err.message });
    } finally {
      clearInterval(tick);
      if (testId.current === id) {
        testId.current = null;
        setBusy(null);
      }
    }
  };
  const cancelTest = () => {
    const id = testId.current;
    if (!id) return;
    testId.current = null;
    void api.cancelTestConnection(id).catch(() => {});
    setBusy(null);
    setTestResult({ ok: false, text: "Test cancelled" });
  };

  const persist = async (): Promise<ConnectionProfile | null> => {
    try {
      // DuckDB without storage keys: drop a saved secret access key.
      const dropSecret = kind === "duckdb" && !storageSecret && !!initial?.has_secret;
      const saved = await api.saveConnection(buildProfile(), dropSecret ? null : secret || null, dropSecret || (clearSecret && !secret), secretSlots());
      const st = useStore.getState();
      await st.refreshConnections();
      useStore.setState((s) => {
        const drop = <T,>(m: Record<string, T>) => Object.fromEntries(Object.entries(m).filter(([k]) => !k.startsWith(saved.id)));
        return {
          schemas: drop(s.schemas),
          objects: drop(s.objects),
          columns: drop(s.columns),
          catalogs: drop(s.catalogs),
          catalogSchemas: drop(s.catalogSchemas),
          explorer: drop(s.explorer),
        };
      });
      const tab = st.tabs.find((t) => t.id === st.activeTabId);
      if (tab && !tab.connection_id && !tab.notebook_id) st.updateTab(tab.id, { connection_id: saved.id });
      return saved;
    } catch (e) {
      toast(toError(e).message, "error");
      return null;
    }
  };

  const save = async () => {
    setBusy("save");
    const saved = await persist();
    setBusy(null);
    if (saved) {
      toast(`${isEdit ? "Updated" : "Added"} ${saved.name}`, "success");
      close();
    }
  };

  /** Save first (sign-in works on saved profiles), then sign in. */
  const signIn = async () => {
    setBusy("signin");
    const saved = await persist();
    if (saved) {
      const ok = await useStore.getState().signInConnection(saved.id);
      if (ok) {
        setAuthStatus(await api.authStatus(saved.id).catch(() => null));
        if (!isEdit) close();
        else useStore.getState().openConnectionDialog({ ...saved, connected: false });
      }
    }
    setBusy(null);
  };

  const signOut = async () => {
    if (!initial) return;
    await api.signOut(initial.id).catch((e) => toast(toError(e).message, "error"));
    setAuthStatus(await api.authStatus(initial.id).catch(() => null));
  };

  const remove = async () => {
    if (!initial) return;
    if (await useStore.getState().deleteConnection(initial.id)) close();
  };

  const browseDb = async (create: boolean) => {
    const filters =
      kind === "duckdb"
        ? [{ name: "DuckDB database", extensions: ["duckdb", "db", "ddb"] }]
        : [{ name: "SQLite database", extensions: ["db", "sqlite", "sqlite3", "db3"] }];
    const path = create
      ? await saveDialog({ filters, defaultPath: kind === "duckdb" ? "analytics.duckdb" : "database.db" })
      : await openDialog({ multiple: false, directory: false, filters: [...filters, { name: "All files", extensions: ["*"] }] });
    if (typeof path === "string") setValue("file_path", path);
  };

  const addFiles = async (directory: boolean) => {
    const picked = await openDialog({
      multiple: true,
      directory,
      filters: directory
        ? undefined
        : [{ name: "Data files", extensions: ["csv", "tsv", "txt", "parquet", "json", "ndjson", "jsonl", "xlsx", "gz"] }],
    });
    const list = Array.isArray(picked) ? picked : typeof picked === "string" ? [picked] : [];
    if (list.length === 0) return;
    const cur = (values.files ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
    setValue("files", [...new Set([...cur, ...list])].join("\n"));
  };

  const sectionBtn = (id: Section, label: string, show = true) =>
    show && (
      <button
        type="button"
        role="tab"
        aria-selected={section === id}
        onClick={() => setSection(id)}
        className={`rounded-md px-2.5 py-1 text-[12.5px] ${section === id ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
      >
        {label}
      </button>
    );

  const secretPlaceholder = initial?.has_secret && !clearSecret ? "•••••••• (saved in keychain)" : "";

  return (
    <Modal
      title={isEdit ? "Edit connection" : "New connection"}
      onClose={close}
      width={640}
      footer={
        <>
          {isEdit && (
            <button className="btn-ghost mr-auto text-danger" onClick={remove}>
              <Trash2 size={14} /> Delete
            </button>
          )}
          {!canSubmit && missing.length > 0 && <span className="mr-auto truncate text-[11.5px] text-muted">Required: {missing.join(", ")}</span>}
          {busy === "test" ? (
            <button className="btn-ghost" onClick={cancelTest} title="Stop the connection test">
              <Loader2 size={14} className="animate-spin" /> Testing{testSecs ? ` ${testSecs}s` : "…"} <Square size={11} className="ml-1" /> Stop
            </button>
          ) : (
            <button className="btn-ghost" onClick={test} disabled={!canSubmit || !!busy}>
              Test
            </button>
          )}
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={save} disabled={!canSubmit || !!busy}>
            {busy === "save" && <Loader2 size={14} className="animate-spin" />}
            {isEdit ? "Save" : "Add connection"}
          </button>
        </>
      }
    >
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (canSubmit) void save();
        }}
      >
        {!isEdit && (
          <div>
            <Label>Database type</Label>
            <div className="grid grid-cols-3 gap-2">
              {sorted.map((c) => (
                <button
                  type="button"
                  key={c.kind}
                  onClick={() => changeKind(c.kind)}
                  aria-pressed={kind === c.kind}
                  className={`rounded-lg border px-3 py-2 text-left text-[13px] transition-colors ${
                    kind === c.kind ? "border-accent bg-accent/10" : "border-line hover:bg-hover"
                  }`}
                >
                  {c.display_name}
                </button>
              ))}
            </div>
          </div>
        )}

        <div className="flex gap-1 border-b border-line pb-2" role="tablist">
          {sectionBtn("general", "General")}
          {sectionBtn("auth", "Authentication", !usesFile)}
          {sectionBtn("storage", storageProvider ? `Object storage · ${storageProvider.label.split(" (")[0]}` : "Object storage", kind === "duckdb")}
          {sectionBtn("ssh", "SSH tunnel", !!info?.capabilities.ssh)}
          {sectionBtn("ai", "AI")}
        </div>

        {info?.note && section !== "ai" && (
          <div className="flex items-start gap-2 rounded-lg bg-panel-2 px-3 py-2 text-[12px] text-muted">
            <Info size={13} className="mt-0.5 shrink-0" /> {info.note}
          </div>
        )}

        {section === "general" && (
          <>
            <div className="grid grid-cols-[1fr_auto] gap-3">
              <div>
                <Label htmlFor="c-name">Name</Label>
                <input id="c-name" className="field" value={name} placeholder={defaultName()} onChange={(e) => setName(e.target.value)} />
              </div>
              <div>
                <Label>Color</Label>
                <div className="flex h-[30px] items-center gap-1" role="radiogroup" aria-label="Color">
                  {COLORS.map((c) => (
                    <button
                      type="button"
                      key={c}
                      role="radio"
                      aria-checked={color === c}
                      aria-label={c}
                      onClick={() => setColor(c)}
                      className={`h-5 w-5 rounded-full ${color === c ? "ring-2 ring-fg ring-offset-2 ring-offset-panel" : ""}`}
                      style={{ background: c }}
                    />
                  ))}
                </div>
              </div>
            </div>

            <div className="grid grid-cols-2 gap-3">
              {fields.map((f) => {
                const id = `c-f-${f.key}`;
                // Shown under Object storage.
                if (f.key.startsWith("storage")) return null;
                if (f.key === "file_path")
                  return (
                    <div key={f.key} className="col-span-2">
                      <Label htmlFor={id}>
                        {f.label}
                        {f.required ? "" : " (optional)"}
                      </Label>
                      <div className="flex gap-2">
                        <input id={id} className="field font-mono" value={values.file_path ?? ""} placeholder={f.placeholder} onChange={(e) => setValue("file_path", e.target.value)} />
                        <button type="button" className="btn-ghost shrink-0 border border-line" onClick={() => browseDb(false)}>
                          <FolderOpen size={14} /> Open
                        </button>
                        <button type="button" className="btn-ghost shrink-0 border border-line" onClick={() => browseDb(true)}>
                          <Plus size={14} /> New
                        </button>
                      </div>
                    </div>
                  );
                if (f.key === "files")
                  return (
                    <div key={f.key} className="col-span-2">
                      <Label htmlFor={id}>{f.label}</Label>
                      <textarea
                        id={id}
                        className="field min-h-[84px] font-mono text-[12px]"
                        value={values.files ?? ""}
                        placeholder={f.placeholder}
                        onChange={(e) => setValue("files", e.target.value)}
                      />
                      <div className="mt-1.5 flex items-center gap-2">
                        <button type="button" className="btn-ghost border border-line py-1" onClick={() => addFiles(false)}>
                          <FilePlus2 size={13} /> Add files…
                        </button>
                        <button type="button" className="btn-ghost border border-line py-1" onClick={() => addFiles(true)}>
                          <FolderPlus size={13} /> Add folder (Delta / Iceberg / Parquet)…
                        </button>
                        {f.help && <span className="text-[11.5px] text-muted">{f.help}</span>}
                      </div>
                    </div>
                  );
                if (kind === "oracle" && f.key === "driver")
                  return (
                    <div key={f.key} className="col-span-2">
                      <Label htmlFor={id}>{f.label}</Label>
                      <select id={id} className="field" value={values.driver === "instant_client" ? "instant_client" : ""} onChange={(e) => setValue("driver", e.target.value)}>
                        <option value="">Thin (no Oracle software; downloaded on first use, 5 MB)</option>
                        <option value="instant_client">Oracle Instant Client</option>
                      </select>
                      {f.help && <p className="mt-1 text-[11px] text-muted">{f.help}</p>}
                    </div>
                  );
                // Each Oracle driver has its own settings.
                if (kind === "oracle" && f.key === "client_lib_dir" && values.driver !== "instant_client") return null;
                if (kind === "oracle" && f.key === "driver_options" && values.driver === "instant_client") return null;
                if (f.key === "result_transfer")
                  return (
                    <div key={f.key}>
                      <Label htmlFor={id}>{f.label}</Label>
                      <select id={id} className="field" value={values.result_transfer === "inline" ? "inline" : ""} onChange={(e) => setValue("result_transfer", e.target.value)}>
                        <option value="">Auto (large results from cloud storage)</option>
                        <option value="inline">Inline only (25 MiB limit)</option>
                      </select>
                      {f.help && <p className="mt-1 text-[11.5px] text-muted">{f.help}</p>}
                    </div>
                  );
                if (f.key === "excel_sheets")
                  return (
                    <div key={f.key} className="col-span-2">
                      <Check
                        checked={values.excel_sheets === "all"}
                        onChange={(v) => setValue("excel_sheets", v ? "all" : "")}
                        label="Read every sheet of Excel files"
                        help="Off: each .xlsx becomes one view of its first sheet. On: one view per sheet with data (the first keeps the file's name, others are <file>_<sheet>)."
                      />
                    </div>
                  );
                if (f.key === "trust_cert")
                  return (
                    <div key={f.key} className="col-span-2">
                      <Check
                        checked={values.trust_cert === "true"}
                        onChange={(v) => setValue("trust_cert", v ? "true" : "")}
                        label="Trust server certificate"
                        help="Skip TLS certificate validation (self-signed dev servers only)."
                      />
                    </div>
                  );
                const wide = ["host", "account", "http_path", "connect_string", "client_lib_dir", "authenticator", "driver_options"].includes(f.key);
                const invalid = f.key === "port" && !portValid;
                return (
                  <div key={f.key} className={wide ? "col-span-2" : ""}>
                    <Label htmlFor={id}>
                      {f.label}
                      {f.required ? " *" : ""}
                    </Label>
                    <input
                      id={id}
                      className={`field ${invalid ? "border-danger" : ""}`}
                      value={values[f.key] ?? ""}
                      inputMode={f.key === "port" ? "numeric" : undefined}
                      placeholder={f.key === "port" ? String(info?.default_port ?? f.placeholder ?? "") : f.placeholder}
                      onChange={(e) => setValue(f.key, f.key === "port" ? e.target.value.trim() : e.target.value)}
                    />
                    {f.help && <p className="mt-1 text-[11px] text-muted">{f.help}</p>}
                  </div>
                );
              })}
            </div>

            {kind === "oracle" && values.driver !== "instant_client" && <OracleAgentPanel />}

            {kind === "oracle" && values.driver === "instant_client" && (
              <OracleClientPanel libDir={values.client_lib_dir || null} onUseDir={(d) => setValue("client_lib_dir", d)} />
            )}

            {traitsOf(kind).ssl_setting && (
              <div>
                <Label htmlFor="c-ssl">SSL</Label>
                <select id="c-ssl" className="field" value={ssl} onChange={(e) => setSsl(e.target.value as SslMode)}>
                  <option value="disable">Disable</option>
                  <option value="prefer">Prefer (encrypt if available, no verification)</option>
                  <option value="require">Require (encrypt, no verification)</option>
                  <option value="verify_full">Verify full (encrypt, verify certificate + host)</option>
                </select>
              </div>
            )}

            <div className="grid grid-cols-2 gap-3">
              <div>
                <Label htmlFor="c-env">Environment</Label>
                <select id="c-env" className="field" value={env} onChange={(e) => setEnv(e.target.value as EnvTag)}>
                  {ENVS.map((x) => (
                    <option key={x.value} value={x.value}>
                      {x.label}
                    </option>
                  ))}
                </select>
                {env === "prod" && <p className="mt-1 text-[11.5px] text-muted">Statements that change data or schema ask for confirmation.</p>}
              </div>
              <div className="mt-5">
                <Check checked={readOnly} onChange={setReadOnly} label="Read-only" help="Block INSERT/UPDATE/DELETE/DDL for this connection." />
              </div>
            </div>
          </>
        )}

        {section === "auth" && !usesFile && (
          <div className="space-y-3">
            <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Authentication method">
              {(info?.auth_methods ?? []).map((m) => (
                <button
                  type="button"
                  key={m}
                  role="radio"
                  aria-checked={authKind === m}
                  onClick={() => setAuthKind(m)}
                  className={`rounded-md border px-2.5 py-1 text-[12.5px] ${authKind === m ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
                >
                  {m === "api_token" ? (TOKEN_LABEL[kind] ?? AUTH_LABELS[m]) : AUTH_LABELS[m]}
                </button>
              ))}
            </div>

            {(needsUser || authKind === "api_token") && (
              <div>
                <Label htmlFor="c-user">
                  {kind === "snowflake" ? "Login name" : "User"}
                  {authKind === "api_token" ? " (optional)" : ""}
                </Label>
                <input
                  id="c-user"
                  className="field"
                  value={user}
                  autoComplete="off"
                  placeholder={kind === "mssql" ? "sa or DOMAIN\\user" : ""}
                  onChange={(e) => setUser(e.target.value)}
                />
              </div>
            )}

            {["password", "api_token"].includes(authKind) && (
              <SecretField
                label={authKind === "password" ? "Password" : (TOKEN_LABEL[kind] ?? "Token")}
                value={secret}
                onChange={setSecret}
                placeholder={secretPlaceholder}
                canClear={!!initial?.has_secret && !secret}
                clear={clearSecret}
                onClear={setClearSecret}
              />
            )}

            {authKind === "key_pair" && (
              <>
                <div>
                  <Label htmlFor="c-pem">Private key (PEM, PKCS#8)</Label>
                  <textarea
                    id="c-pem"
                    className="field min-h-[96px] font-mono text-[11px]"
                    value={secret}
                    placeholder={secretPlaceholder || "-----BEGIN PRIVATE KEY-----"}
                    onChange={(e) => setSecret(e.target.value)}
                  />
                </div>
                <SecretField label="Key passphrase (if encrypted)" value={passphrase} onChange={setPassphrase} placeholder="" />
              </>
            )}

            {authKind === "service_account" && (
              <div>
                <Label htmlFor="c-sa">Service account key (JSON)</Label>
                <textarea
                  id="c-sa"
                  className="field min-h-[110px] font-mono text-[11px]"
                  value={secret}
                  placeholder={secretPlaceholder || '{ "type": "service_account", … }'}
                  onChange={(e) => setSecret(e.target.value)}
                />
                <p className="mt-1 text-[11px] text-muted">Stored in the OS keychain.</p>
              </div>
            )}

            {["oauth_browser", "device_code", "client_credentials"].includes(authKind) && (
              <div className="space-y-3">
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <Label htmlFor="c-cid">OAuth client ID{needsClientId ? " *" : " (optional)"}</Label>
                    <input
                      id="c-cid"
                      className="field font-mono text-[12px]"
                      value={oauth.client_id ?? ""}
                      placeholder={kind === "databricks" ? "databricks-cli (default)" : ""}
                      onChange={(e) => setOauth({ ...oauth, client_id: e.target.value })}
                    />
                  </div>
                  {kind === "mssql" && (
                    <div>
                      <Label htmlFor="c-tenant">Tenant</Label>
                      <input id="c-tenant" className="field" value={oauth.tenant ?? ""} placeholder="organizations" onChange={(e) => setOauth({ ...oauth, tenant: e.target.value })} />
                    </div>
                  )}
                  {authKind === "oauth_browser" && kind !== "mssql" && (
                    <div>
                      <Label htmlFor="c-hint">Login hint / user (optional)</Label>
                      <input id="c-hint" className="field" value={user} onChange={(e) => setUser(e.target.value)} />
                    </div>
                  )}
                </div>
                {authKind === "client_credentials" && (
                  <SecretField
                    label="Client secret"
                    value={secret}
                    onChange={setSecret}
                    placeholder={secretPlaceholder}
                    canClear={!!initial?.has_secret && !secret}
                    clear={clearSecret}
                    onClear={setClearSecret}
                  />
                )}
                <details className="rounded-lg border border-line px-3 py-2">
                  <summary className="cursor-pointer text-[12px] text-muted">Advanced OAuth settings</summary>
                  <div className="mt-2 grid grid-cols-2 gap-3">
                    <div className="col-span-2">
                      <Label>Scopes (space separated)</Label>
                      <input className="field font-mono text-[12px]" value={oauth.scopes ?? ""} placeholder="(provider default)" onChange={(e) => setOauth({ ...oauth, scopes: e.target.value })} />
                    </div>
                    <div>
                      <Label>Redirect port</Label>
                      <input
                        className="field"
                        inputMode="numeric"
                        value={oauth.redirect_port ?? ""}
                        placeholder="(auto)"
                        onChange={(e) => setOauth({ ...oauth, redirect_port: /^\d+$/.test(e.target.value) ? Number(e.target.value) : undefined })}
                      />
                    </div>
                    <div />
                    <div className="col-span-2">
                      <Label>Authorize URL (custom IdP)</Label>
                      <input className="field font-mono text-[12px]" value={oauth.authorize_url ?? ""} onChange={(e) => setOauth({ ...oauth, authorize_url: e.target.value })} />
                    </div>
                    <div className="col-span-2">
                      <Label>Token URL (custom IdP)</Label>
                      <input className="field font-mono text-[12px]" value={oauth.token_url ?? ""} onChange={(e) => setOauth({ ...oauth, token_url: e.target.value })} />
                    </div>
                  </div>
                </details>
              </div>
            )}

            {authKind === "cloud_cli" && (
              <div className="space-y-2">
                <p className="text-[12px] text-muted">{CLI_HELP[kind] ?? "Uses your local CLI login."}</p>
                {kind === "databricks" && (
                  <div>
                    <Label htmlFor="c-prof">CLI profile (optional)</Label>
                    <input id="c-prof" className="field" value={cliProfile} placeholder="DEFAULT" onChange={(e) => setCliProfile(e.target.value)} />
                  </div>
                )}
              </div>
            )}

            {interactive && (
              <div className="flex items-center gap-2 rounded-lg border border-line bg-panel-2 p-2.5">
                <Globe size={15} className="text-accent" />
                <div className="min-w-0 flex-1 text-[12.5px]">
                  {authStatus?.signed_in ? (
                    <>
                      Signed in{authStatus.identity ? ` as ${authStatus.identity}` : ""}
                      {authStatus.expires_at ? (
                        <span className="text-muted"> · token until {new Date(authStatus.expires_at * 1000).toLocaleTimeString()}</span>
                      ) : null}
                    </>
                  ) : (
                    <span className="text-muted">
                      {authKind === "device_code"
                        ? "You'll get a code to enter in the browser."
                        : "A browser window opens for sign-in. You can also sign in on first connect."}
                    </span>
                  )}
                </div>
                {authStatus?.signed_in && (
                  <button type="button" className="btn-ghost py-1" onClick={signOut}>
                    <LogOut size={12} /> Sign out
                  </button>
                )}
                <button type="button" className="btn-primary py-1" onClick={signIn} disabled={!canSubmit || !!busy}>
                  {busy === "signin" && <Loader2 size={12} className="animate-spin" />}
                  {authStatus?.signed_in ? "Sign in again" : "Save & sign in"}
                </button>
              </div>
            )}
          </div>
        )}

        {section === "storage" && kind === "duckdb" && (
          <StorageSection
            values={values}
            setValue={setValue}
            user={user}
            setUser={setUser}
            secret={secret}
            setSecret={setSecret}
            secretPlaceholder={secretPlaceholder}
            canClear={!!initial?.has_secret && !secret && storageSecret}
            listBuckets={() => api.storageBuckets(buildProfile().config, secret || null, isEdit && !clearSecret ? (initial?.id ?? null) : null)}
            clear={clearSecret}
            onClear={setClearSecret}
          />
        )}

        {section === "ssh" && info?.capabilities.ssh && (
          <div className="space-y-3">
            <Check checked={sshOn} onChange={setSshOn} label="Connect through an SSH tunnel" help="The database host/port are resolved from the SSH server." />
            {sshOn && (
              <>
                <div className="grid grid-cols-[1fr_90px] gap-3">
                  <div>
                    <Label htmlFor="s-host">SSH host *</Label>
                    <input id="s-host" className="field" value={ssh.host} placeholder="bastion.example.com" onChange={(e) => setSsh({ ...ssh, host: e.target.value })} />
                  </div>
                  <div>
                    <Label htmlFor="s-port">Port</Label>
                    <input
                      id="s-port"
                      className="field"
                      inputMode="numeric"
                      value={ssh.port}
                      onChange={(e) => setSsh({ ...ssh, port: Number(e.target.value.replace(/\D/g, "")) || 22 })}
                    />
                  </div>
                </div>
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <Label htmlFor="s-user">SSH user *</Label>
                    <input id="s-user" className="field" value={ssh.user} onChange={(e) => setSsh({ ...ssh, user: e.target.value })} />
                  </div>
                  <div>
                    <Label htmlFor="s-auth">Authentication</Label>
                    <select
                      id="s-auth"
                      className="field"
                      value={ssh.auth.method}
                      onChange={(e) =>
                        setSsh({
                          ...ssh,
                          auth: e.target.value === "key" ? { method: "key", path: "~/.ssh/id_ed25519" } : { method: e.target.value as "agent" | "password" },
                        })
                      }
                    >
                      <option value="agent">SSH agent</option>
                      <option value="key">Private key file</option>
                      <option value="password">Password</option>
                    </select>
                  </div>
                </div>
                {ssh.auth.method === "key" && (
                  <div>
                    <Label htmlFor="s-key">Private key file</Label>
                    <div className="flex gap-2">
                      <input
                        id="s-key"
                        className="field font-mono"
                        value={ssh.auth.path}
                        onChange={(e) => setSsh({ ...ssh, auth: { method: "key", path: e.target.value } })}
                      />
                      <button
                        type="button"
                        className="btn-ghost shrink-0 border border-line"
                        onClick={async () => {
                          const p = await openDialog({ multiple: false, directory: false });
                          if (typeof p === "string") setSsh({ ...ssh, auth: { method: "key", path: p } });
                        }}
                      >
                        <FolderOpen size={14} /> Browse
                      </button>
                    </div>
                  </div>
                )}
                {ssh.auth.method !== "agent" && (
                  <SecretField
                    label={ssh.auth.method === "key" ? "Key passphrase (if any)" : "SSH password"}
                    value={sshSecret}
                    onChange={setSshSecret}
                    placeholder={initial?.config.ssh ? "(unchanged if empty)" : ""}
                  />
                )}
                <div className="rounded-lg bg-panel-2 px-3 py-2 text-[12px]">
                  <span className="text-muted">Host key: </span>
                  {ssh.host_key_fingerprint ? (
                    <>
                      <span className="font-mono">{ssh.host_key_fingerprint}</span>
                      <button type="button" className="ml-2 text-accent underline" onClick={() => setSsh({ ...ssh, host_key_fingerprint: null })}>
                        Forget
                      </button>
                    </>
                  ) : (
                    <span className="text-muted">pinned on first connect (trust on first use)</span>
                  )}
                </div>
              </>
            )}
          </div>
        )}

        {section === "ai" && (
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
        )}

        {testResult && (
          <div
            className={`flex flex-wrap items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px] ${
              testResult.ok ? "border-success/40 bg-success/10" : "border-danger/40 bg-danger/10"
            }`}
            role="status"
          >
            {testResult.ok ? <CheckCircle2 size={15} className="mt-px shrink-0 text-success" /> : <XCircle size={15} className="mt-px shrink-0 text-danger" />}
            <span className="min-w-0 flex-1 break-words select-text">{testResult.text}</span>
            {!testResult.ok && testResult.text !== "Test cancelled" && (
              <AskAiAboutError error={testResult.text} connectionId={initial?.id ?? null} question={() => connectionErrorQuestion(testResult.text, buildProfile().config)} />
            )}
            {!testResult.ok && (
              <button
                type="button"
                className="icon-btn -my-1 h-6 w-6 shrink-0"
                aria-label="Copy the error"
                title="Copy the error"
                onClick={() =>
                  void writeText(testResult.text)
                    .then(() => useStore.getState().toast("Error copied", "success"))
                    .catch((e) => useStore.getState().toast(toError(e).message, "error"))
                }
              >
                <Copy size={13} />
              </button>
            )}
          </div>
        )}
        <button type="submit" className="hidden" />
      </form>
    </Modal>
  );
}

/** DuckDB → Object storage: where the bucket is and how to sign in. */
function StorageSection({
  values,
  setValue,
  user,
  setUser,
  secret,
  setSecret,
  secretPlaceholder,
  canClear,
  clear,
  onClear,
  listBuckets,
}: {
  listBuckets: () => Promise<StorageBucket[]>;
  values: Record<string, string>;
  setValue: (k: string, v: string) => void;
  user: string;
  setUser: (v: string) => void;
  secret: string;
  setSecret: (v: string) => void;
  secretPlaceholder: string;
  canClear: boolean;
  clear: boolean;
  onClear: (v: boolean) => void;
}) {
  const provider = providerOf(values.storage);
  const auth: StorageAuth = (values.storage_auth as StorageAuth) || "keys";
  const isS3 = provider?.id === "s3" || provider?.id === "s3_compatible";
  const azure = provider?.id === "azure";
  const auths: { id: StorageAuth; label: string }[] = azure
    ? [
        { id: "keys", label: "Account key" },
        { id: "connection_string", label: "Connection string" },
        { id: "sas", label: "SAS token" },
        { id: "service_principal", label: "Service principal" },
        { id: "chain", label: "Azure CLI / managed identity" },
        { id: "anonymous", label: "Public container" },
      ]
    : [
        { id: "keys", label: provider?.id === "gcs" ? "HMAC keys" : "Access keys" },
        ...(isS3 ? [{ id: "chain" as const, label: "AWS credential chain" }] : []),
        { id: "anonymous", label: "Public bucket (no sign-in)" },
      ];
  const [buckets, setBuckets] = useState<{ loading: boolean; list?: StorageBucket[]; error?: string } | null>(null);
  const noun = bucketNoun(provider?.id);
  const loadBuckets = async () => {
    setBuckets({ loading: true });
    try {
      setBuckets({ loading: false, list: await listBuckets() });
    } catch (e) {
      setBuckets({ loading: false, error: toError(e).message });
    }
  };
  const keyLabels =
    provider?.id === "gcs"
      ? ["HMAC access ID", "HMAC secret"]
      : provider?.id === "r2"
        ? ["Access key ID (R2 API token)", "Secret access key"]
        : ["Access key ID", "Secret access key"];
  return (
    <div className="space-y-3">
      <div>
        <Label htmlFor="st-provider">Storage</Label>
        <select
          id="st-provider"
          className="field"
          value={provider?.id ?? ""}
          onChange={(e) => {
            setValue("storage", e.target.value);
            // The credential chain is AWS only.
            // Sign-ins differ between Azure and the S3 family.
            const az = e.target.value === "azure";
            const keep = auth === "keys" || auth === "anonymous" || (auth === "chain" && (az || ["s3", "s3_compatible"].includes(e.target.value))) || (az && ["connection_string", "sas", "service_principal"].includes(auth));
            if (!keep) setValue("storage_auth", "");
            setBuckets(null);
          }}
        >
          <option value="">None (local files and public HTTPS URLs only)</option>
          {STORAGE_PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
        <p className="mt-1 text-[11.5px] text-muted">
          {provider ? (
            <>
              Query files with their URL, e.g. <span className="font-mono">SELECT * FROM &apos;{provider.example}&apos;</span>, or add URLs to Attached files. Keys are kept in the OS keychain and given to DuckDB as a temporary secret for each session (never written to a database file).
            </>
          ) : (
            "Read files in S3, Cloudflare R2, Google Cloud Storage or an S3-compatible server such as MinIO."
          )}
        </p>
      </div>

      {provider && (
        <>
          <div>
            <Label>Sign-in</Label>
            <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Storage sign-in">
              {auths.map((a) => (
                <button
                  type="button"
                  key={a.id}
                  role="radio"
                  aria-checked={auth === a.id}
                  onClick={() => setValue("storage_auth", a.id === "keys" ? "" : a.id)}
                  className={`rounded-md border px-2.5 py-1 text-[12.5px] ${auth === a.id ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
                >
                  {a.label}
                </button>
              ))}
            </div>
          </div>

          {azure && (
            <div className="space-y-3">
              {auth !== "connection_string" && (
                <div>
                  <Label htmlFor="st-account">Storage account *</Label>
                  <input id="st-account" className="field font-mono text-[12px]" value={values.storage_account ?? ""} placeholder="mystorageaccount" spellCheck={false} onChange={(e) => setValue("storage_account", e.target.value)} />
                </div>
              )}
              {auth === "keys" && <SecretField label="Account key" value={secret} onChange={setSecret} placeholder={secretPlaceholder} canClear={canClear} clear={clear} onClear={onClear} />}
              {auth === "sas" && <SecretField label="SAS token" value={secret} onChange={setSecret} placeholder={secretPlaceholder || "sv=…&sig=…"} canClear={canClear} clear={clear} onClear={onClear} />}
              {auth === "connection_string" && (
                <SecretField label="Connection string" value={secret} onChange={setSecret} placeholder={secretPlaceholder || "DefaultEndpointsProtocol=https;AccountName=…;AccountKey=…"} canClear={canClear} clear={clear} onClear={onClear} />
              )}
              {auth === "service_principal" && (
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <Label htmlFor="st-tenant">Tenant (directory) ID *</Label>
                    <input id="st-tenant" className="field font-mono text-[12px]" value={values.storage_tenant ?? ""} onChange={(e) => setValue("storage_tenant", e.target.value)} />
                  </div>
                  <div>
                    <Label htmlFor="st-client">Client (application) ID *</Label>
                    <input id="st-client" className="field font-mono text-[12px]" value={values.storage_client_id ?? ""} onChange={(e) => setValue("storage_client_id", e.target.value)} />
                  </div>
                  <div className="col-span-2">
                    <SecretField label="Client secret" value={secret} onChange={setSecret} placeholder={secretPlaceholder} canClear={canClear} clear={clear} onClear={onClear} />
                  </div>
                </div>
              )}
              {auth === "chain" && (
                <p className="text-[11.5px] text-muted">
                  Uses the Azure CLI login (run <span className="font-mono">az login</span> first), the AZURE_* environment variables, or the machine&apos;s managed identity. The identity needs a Storage Blob Data role on the account.
                </p>
              )}
              <div>
                <Label htmlFor="st-endpoint">Blob endpoint (optional)</Label>
                <input
                  id="st-endpoint"
                  className="field font-mono text-[12px]"
                  value={values.storage_endpoint ?? ""}
                  placeholder="https://<account>.blob.core.windows.net (Azurite: http://127.0.0.1:10000/devstoreaccount1)"
                  onChange={(e) => setValue("storage_endpoint", e.target.value)}
                />
              </div>
              <p className="text-[11.5px] text-muted">
                URLs: <span className="font-mono">az://container/path/file.parquet</span>, or ADLS Gen2 <span className="font-mono">abfss://container@account.dfs.core.windows.net/path/</span>.
              </p>
            </div>
          )}

          {auth === "keys" && !azure && (
            <div className="grid grid-cols-2 gap-3">
              <div>
                <Label htmlFor="st-key">{keyLabels[0]} *</Label>
                <input id="st-key" className="field font-mono text-[12px]" value={user} autoComplete="off" spellCheck={false} onChange={(e) => setUser(e.target.value)} />
              </div>
              <SecretField label={keyLabels[1]} value={secret} onChange={setSecret} placeholder={secretPlaceholder} canClear={canClear} clear={clear} onClear={onClear} />
            </div>
          )}
          {auth === "chain" && !azure && (
            <div>
              <Label htmlFor="st-profile">AWS profile (optional)</Label>
              <input id="st-profile" className="field" value={values.storage_profile ?? ""} placeholder="default" onChange={(e) => setValue("storage_profile", e.target.value)} />
              <p className="mt-1 text-[11.5px] text-muted">
                Uses the AWS SDK&apos;s credentials: environment variables, ~/.aws/credentials and config profiles, SSO (run <span className="font-mono">aws sso login</span> first) or the instance role.
              </p>
            </div>
          )}

          <div className="grid grid-cols-2 gap-3">
            {provider.id === "r2" && (
              <div>
                <Label htmlFor="st-acct">Cloudflare account ID *</Label>
                <input id="st-acct" className="field font-mono text-[12px]" value={values.storage_account_id ?? ""} onChange={(e) => setValue("storage_account_id", e.target.value)} />
              </div>
            )}
            {isS3 && (
              <div>
                <Label htmlFor="st-region">Region{provider.id === "s3_compatible" ? " (optional)" : ""}</Label>
                <input id="st-region" className="field" value={values.storage_region ?? ""} placeholder="us-east-1" onChange={(e) => setValue("storage_region", e.target.value)} />
              </div>
            )}
            {provider.id !== "gcs" && !azure && (
              <div className={provider.id === "r2" ? "" : "col-span-1"}>
                <Label htmlFor="st-endpoint">Endpoint{provider.id === "s3_compatible" ? " *" : " (optional)"}</Label>
                <input
                  id="st-endpoint"
                  className="field font-mono text-[12px]"
                  value={values.storage_endpoint ?? ""}
                  placeholder={provider.id === "s3_compatible" ? "http://localhost:9000" : provider.id === "r2" ? "<account>.r2.cloudflarestorage.com" : "s3.amazonaws.com"}
                  onChange={(e) => setValue("storage_endpoint", e.target.value)}
                />
              </div>
            )}
            {isS3 && (
              <div>
                <Label htmlFor="st-style">URL style</Label>
                <select id="st-style" className="field" value={values.storage_url_style ?? ""} onChange={(e) => setValue("storage_url_style", e.target.value)}>
                  <option value="">{provider.id === "s3_compatible" ? "Default (path: host/bucket/key)" : "Default (virtual host: bucket.host/key)"}</option>
                  <option value="path">Path (host/bucket/key)</option>
                  <option value="vhost">Virtual host (bucket.host/key)</option>
                </select>
              </div>
            )}
            {provider.id === "s3_compatible" && (
              <div className="mt-5">
                <Check
                  checked={values.storage_use_ssl !== "false" && !/^http:\/\//i.test(values.storage_endpoint ?? "")}
                  onChange={(v) => setValue("storage_use_ssl", v ? "" : "false")}
                  label="Use TLS (https)"
                  help="Off for http:// endpoints such as a local MinIO."
                />
              </div>
            )}
            <div className="col-span-2">
              <Label htmlFor="st-bucket">{azure ? "Container" : "Bucket"} (optional)</Label>
              <div className="flex gap-2">
                <input
                  id="st-bucket"
                  className="field font-mono text-[12px]"
                  value={values.storage_bucket ?? ""}
                  placeholder={`my-${noun} or my-${noun}/prefix`}
                  spellCheck={false}
                  onChange={(e) => setValue("storage_bucket", e.target.value)}
                />
                {auth !== "anonymous" && (
                  <button type="button" className="btn-ghost shrink-0 border border-line" onClick={() => void loadBuckets()} disabled={buckets?.loading}>
                    {buckets?.loading ? <Loader2 size={13} className="animate-spin" /> : <FolderOpen size={13} />} List {bucketNoun(provider.id, true)}
                  </button>
                )}
              </div>
              {buckets?.error && <p className="mt-1 text-[11.5px] break-words text-danger select-text">{buckets.error}</p>}
              {buckets?.list && (
                <div className="mt-1.5 max-h-[150px] overflow-auto rounded-md border border-line p-1" role="listbox" aria-label={bucketNoun(provider.id, true)}>
                  {buckets.list.length === 0 && <div className="px-2 py-2 text-[12px] text-muted">No {bucketNoun(provider.id, true)} in this account</div>}
                  {buckets.list.map((b) => (
                    <button
                      type="button"
                      role="option"
                      aria-selected={values.storage_bucket === b.name}
                      key={b.name}
                      className={`flex w-full items-center gap-2 rounded px-2 py-1 text-left text-[12.5px] hover:bg-hover ${values.storage_bucket === b.name ? "bg-hover" : ""}`}
                      onClick={() => setValue("storage_bucket", b.name)}
                    >
                      <span className="min-w-0 flex-1 truncate font-mono">{b.name}</span>
                      {b.created && <span className="shrink-0 text-[11px] text-muted">{b.created.slice(0, 10)}</span>}
                    </button>
                  ))}
                </div>
              )}
              <p className="mt-1 text-[11.5px] text-muted">
                Test and connecting check that it can be listed, and &quot;Browse object storage&quot; opens it. Other {bucketNoun(provider.id, true)} work too, by URL.
              </p>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function SecretField({
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
