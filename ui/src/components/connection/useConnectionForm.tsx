// State of the connection form (type, address, sign-in, SSH, AI settings)
// and its actions: test, save, sign in and out, delete, pick files.
import { useEffect, useMemo, useRef, useState } from "react";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { api, toError } from "../../lib/api";
import type { AiPolicy, AuthMethod, AuthMethodKind, AuthStatus, ConnectionProfile, ConnectorKind, EnvTag, OAuthParams, SshConfig, SslMode } from "../../lib/types";
import { useStore } from "../../store";
import { COLORS, DEFAULT_POLICY, TOP_LEVEL, rank, type Section } from "./parts";

/** State and actions of the connection form (one dialog, new or edit). */
/** Connection options about DataBrain's sessions (not sent to the server). */
export const SESSION_OPTIONS = ["connect_timeout_secs", "idle_disconnect_minutes"] as const;

export function useConnectionForm(initial: ConnectionProfile | null, folderId: string | null) {
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

  const [section, setSection] = useState<Section>("general");
  const [kind, setKind] = useState<ConnectorKind>(initial?.config.kind ?? sorted[0]?.kind ?? "sqlite");
  const info = connectors.find((c) => c.kind === kind);
  const [name, setName] = useState(initial?.name ?? "");
  const [values, setValues] = useState<Record<string, string>>(() => {
    const c = initial?.config;
    return {
      host: c?.host ?? "",
      port: c?.port ? String(c.port) : "",
      database: c?.database ?? "",
      file_path: c?.file_path ?? "",
      ...(c?.options ?? {}),
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

  const buildAuth = (): AuthMethod => {
    const u = user.trim();
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
    // This connection's own session handling (empty = the Settings value).
    for (const k of SESSION_OPTIONS) {
      const v = (values[k] ?? "").trim();
      if (/^\d+$/.test(v)) options[k] = String(Number(v));
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
      return values.file_path?.trim() ? values.file_path.split(/[\\/]/).pop()! : first ? `DuckDB · ${first.split(/[\\/]/).pop()}` : "DuckDB";
    }
    if (usesFile) return values.file_path?.split(/[\\/]/).pop() || info?.display_name || kind;
    const main = values.account || values.project || values.host || "localhost";
    return `${values.database || values.catalog || values.dataset || info?.display_name || kind}@${main}`;
  };

  const needsUser = ["password", "key_pair", "external_browser"].includes(authKind);
  const needsClientId =
    ["device_code", "client_credentials"].includes(authKind) || (authKind === "oauth_browser" && kind !== "databricks");
  const portValid = !values.port || (/^\d+$/.test(values.port) && +values.port > 0 && +values.port < 65536);
  const missing = fields.filter((f) => f.required && !(values[f.key] ?? "").trim()).map((f) => f.label);
  if (needsUser && !user.trim()) missing.push("User");
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
      const saved = await api.saveConnection(buildProfile(), secret || null, clearSecret && !secret, secretSlots());
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

  return { initial, folderId, connectors, closeDialog, testId, testSecs, setTestSecs, close, toast, isEdit, sorted, section, setSection, kind, setKind, info, name, setName, values, setValues, setValue, initialAuth, authKind, setAuthKind, user, setUser, oauth, setOauth, cliProfile, setCliProfile, secret, setSecret, passphrase, setPassphrase, clearSecret, setClearSecret, authStatus, setAuthStatus, sshOn, setSshOn, ssh, setSsh, sshSecret, setSshSecret, ssl, setSsl, readOnly, setReadOnly, color, setColor, env, setEnv, policy, setPolicy, busy, setBusy, testResult, setTestResult, interactive, changeKind, usesFile, fields, buildAuth, buildProfile, defaultName, needsUser, needsClientId, portValid, missing, canSubmit, secretSlots, test, cancelTest, persist, save, signIn, signOut, remove, browseDb, addFiles, sectionBtn, secretPlaceholder };
}

export type ConnectionFormState = ReturnType<typeof useConnectionForm>;
