import { useEffect, useState } from "react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { AlertCircle, Bot, Check, Copy, Globe, KeyRound, Loader2, LogOut, Plug, Plus, RefreshCw, ScrollText, Server, Terminal, Trash2 } from "lucide-react";
import { api, toError } from "../lib/api";
import type { AuditEntry, KiroStatus, ProviderAuth, ProviderKind, ProviderView } from "../lib/types";
import { relativeTime } from "../lib/util";
import { useStore } from "../store";
import { useAi } from "../aiStore";
import { Modal } from "./ui";

export const PROVIDER_KINDS: { kind: ProviderKind; label: string; base: string; auth: ProviderAuth[]; model: string; hint?: string }[] = [
  {
    kind: "kiro",
    label: "Kiro",
    base: "",
    auth: ["kiro_browser", "api_key"],
    model: "auto",
    hint: "Uses your Kiro subscription through kiro-cli. Sign in with the browser (Builder ID, GitHub, Google, IAM Identity Center) or use a Kiro API key (ksk_…, Pro plans).",
  },
  { kind: "openai", label: "OpenAI", base: "https://api.openai.com/v1", auth: ["api_key"], model: "gpt-4.1" },
  { kind: "anthropic", label: "Anthropic", base: "https://api.anthropic.com/v1", auth: ["api_key"], model: "claude-sonnet-4-5" },
  { kind: "gemini", label: "Google Gemini", base: "https://generativelanguage.googleapis.com/v1beta", auth: ["api_key", "google_adc"], model: "gemini-2.5-pro" },
  {
    kind: "azure_openai",
    label: "Azure OpenAI",
    base: "",
    auth: ["api_key", "azure_cli"],
    model: "",
    hint: "Endpoint like https://myres.openai.azure.com. The model is your deployment name.",
  },
  {
    kind: "openrouter",
    label: "OpenRouter",
    base: "https://openrouter.ai/api/v1",
    auth: ["browser_openrouter", "api_key"],
    model: "anthropic/claude-sonnet-4.5",
    hint: "Sign in with your browser — no key copy/paste needed. Gives access to most hosted models.",
  },
  { kind: "ollama", label: "Ollama (local)", base: "http://localhost:11434/v1", auth: ["none"], model: "qwen2.5-coder:14b" },
  { kind: "lm_studio", label: "LM Studio (local)", base: "http://localhost:1234/v1", auth: ["none"], model: "" },
  {
    kind: "openai_compatible",
    label: "OpenAI-compatible",
    base: "",
    auth: ["api_key", "none"],
    model: "",
    hint: "vLLM, LiteLLM, Groq, Together, DeepSeek, Mistral, llama.cpp server…",
  },
];

const AUTH_LABEL: Record<ProviderAuth, string> = {
  api_key: "API key",
  browser_openrouter: "Browser sign-in",
  azure_cli: "Azure CLI (az login)",
  google_adc: "Google ADC (gcloud)",
  kiro_browser: "Browser sign-in",
  none: "No auth",
};

export function SettingsDialog() {
  const open = useStore((s) => s.settingsOpen);
  const setOpen = useStore((s) => s.setSettingsOpen);
  const [section, setSection] = useState<"providers" | "mcp" | "audit">("providers");
  if (!open) return null;
  const item = (id: typeof section, icon: React.ReactNode, label: string) => (
    <button
      onClick={() => setSection(id)}
      aria-pressed={section === id}
      className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[13px] ${section === id ? "bg-hover" : "text-muted hover:text-fg"}`}
    >
      {icon}
      {label}
    </button>
  );
  return (
    <Modal title="Settings" onClose={() => setOpen(false)} width={820}>
      <div className="flex min-h-[460px] gap-4">
        <div className="w-40 shrink-0 space-y-0.5">
          {item("providers", <Bot size={14} />, "AI providers")}
          {item("mcp", <Plug size={14} />, "MCP / Kiro")}
          {item("audit", <ScrollText size={14} />, "AI audit log")}
        </div>
        <div className="min-w-0 flex-1">
          {section === "providers" && <Providers />}
          {section === "mcp" && <Mcp />}
          {section === "audit" && <Audit />}
        </div>
      </div>
    </Modal>
  );
}

function Providers() {
  const providers = useAi((s) => s.providers);
  const refresh = useAi((s) => s.refreshProviders);
  const [sel, setSel] = useState<string | "new" | null>(providers[0]?.id ?? "new");
  const current = providers.find((p) => p.id === sel) ?? null;
  return (
    <div className="flex h-full gap-3">
      <div className="w-48 shrink-0 space-y-0.5 border-r border-line pr-3">
        {providers.map((p) => (
          <button
            key={p.id}
            onClick={() => setSel(p.id)}
            className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[12.5px] ${sel === p.id ? "bg-hover" : "hover:bg-hover"}`}
          >
            <span className="min-w-0 flex-1 truncate">{p.name}</span>
            {(p.has_key || ["none", "azure_cli", "google_adc", "kiro_browser"].includes(p.config.auth)) && (
              <Check size={12} className="text-success" />
            )}
          </button>
        ))}
        <button className="btn-ghost w-full justify-start py-1" onClick={() => setSel("new")}>
          <Plus size={13} /> Add provider
        </button>
      </div>
      <div className="min-w-0 flex-1">
        <ProviderForm
          key={current?.id ?? "new"}
          initial={current}
          onSaved={async (id) => {
            await refresh();
            setSel(id);
          }}
          onDeleted={async () => {
            await refresh();
            setSel("new");
          }}
        />
      </div>
    </div>
  );
}

function ProviderForm({
  initial,
  onSaved,
  onDeleted,
}: {
  initial: ProviderView | null;
  onSaved: (id: string) => Promise<void>;
  onDeleted: () => Promise<void>;
}) {
  const toast = useStore((s) => s.toast);
  const [kind, setKind] = useState<ProviderKind>(initial?.kind ?? "openai");
  const spec = PROVIDER_KINDS.find((k) => k.kind === kind)!;
  const [name, setName] = useState(initial?.name ?? "");
  const [baseUrl, setBaseUrl] = useState(initial?.config.base_url ?? "");
  const [auth, setAuth] = useState<ProviderAuth>(initial?.config.auth ?? spec.auth[0]);
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState(initial?.config.default_model ?? "");
  const [fastModel, setFastModel] = useState(initial?.config.fast_model ?? "");
  const [apiVersion, setApiVersion] = useState(initial?.config.api_version ?? (kind === "azure_openai" ? "2024-10-21" : ""));
  const [models, setModels] = useState<string[]>([]);
  const [busy, setBusy] = useState<null | "save" | "signin" | "models" | "status">(null);
  const [kiro, setKiro] = useState<KiroStatus | null>(null);
  const isKiro = kind === "kiro";

  const refreshKiro = async (id = initial?.id) => {
    if (!id) return;
    setBusy((b) => b ?? "status");
    try {
      setKiro(await api.aiProviderStatus(id));
    } catch (e) {
      setKiro({ installed: false, signed_in: false, message: toError(e).message });
    } finally {
      setBusy((b) => (b === "status" ? null : b));
    }
  };
  useEffect(() => {
    if (initial?.kind === "kiro") void refreshKiro(initial.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initial?.id, initial?.config.auth]);

  const changeKind = (k: ProviderKind) => {
    setKind(k);
    const s = PROVIDER_KINDS.find((x) => x.kind === k)!;
    setAuth(s.auth[0]);
    setModel(s.model);
    setBaseUrl("");
    setApiVersion(k === "azure_openai" ? "2024-10-21" : "");
  };

  const save = async (): Promise<string | null> => {
    setBusy("save");
    try {
      const rec = await api.aiSaveProvider(
        {
          id: initial?.id ?? "",
          kind,
          name: name.trim() || spec.label,
          config: {
            base_url: baseUrl.trim() || null,
            auth,
            default_model: model.trim() || null,
            fast_model: fastModel.trim() || null,
            api_version: apiVersion.trim() || null,
            extra_headers: initial?.config.extra_headers ?? {},
            max_output_tokens: initial?.config.max_output_tokens ?? null,
          },
          created_at: initial?.created_at ?? 0,
          updated_at: 0,
        },
        auth === "api_key" && apiKey ? apiKey : null,
      );
      setApiKey("");
      await onSaved(rec.id);
      if (isKiro) await refreshKiro(rec.id);
      const ai = useAi.getState();
      if (!ai.providerId || ai.providerId === rec.id) ai.setProvider(rec.id, model.trim() || null);
      toast(`Saved ${rec.name}`, "success");
      return rec.id;
    } catch (e) {
      toast(toError(e).message, "error");
      return null;
    } finally {
      setBusy(null);
    }
  };

  const signIn = async () => {
    const id = initial?.id ?? (await save());
    if (!id) return;
    setBusy("signin");
    const label = isKiro ? "Sign in to Kiro" : "Sign in to OpenRouter";
    useStore.getState().setSignIn({ connectionId: null, label, event: null });
    try {
      await api.aiProviderSignIn(id);
      toast(isKiro ? "Signed in to Kiro" : "Signed in to OpenRouter", "success");
      await onSaved(id);
      if (isKiro) await refreshKiro(id);
    } catch (e) {
      const err = toError(e);
      if (err.kind !== "cancelled") toast(err.message, "error");
    } finally {
      useStore.getState().setSignIn(null);
      setBusy(null);
    }
  };

  const loadModels = async () => {
    if (!initial) return;
    setBusy("models");
    try {
      setModels((await api.aiListModels(initial.id)).map((m) => m.id));
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(null);
    }
  };

  const remove = () =>
    useStore.getState().askConfirm({
      title: `Remove ${initial?.name}?`,
      reasons: ["The provider and its stored key are deleted."],
      confirmLabel: "Remove",
      onConfirm: async () => {
        if (!initial) return;
        await api.aiDeleteProvider(initial.id);
        await onDeleted();
      },
    });

  const L = ({ children }: { children: React.ReactNode }) => <div className="mb-1 text-[11.5px] font-medium text-muted">{children}</div>;

  return (
    <div className="space-y-3">
      <div>
        <L>Provider</L>
        <div className="grid grid-cols-4 gap-1.5">
          {PROVIDER_KINDS.map((k) => (
            <button
              key={k.kind}
              disabled={!!initial}
              onClick={() => changeKind(k.kind)}
              aria-pressed={kind === k.kind}
              className={`rounded-lg border px-2 py-1.5 text-left text-[12px] disabled:cursor-default ${kind === k.kind ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
            >
              {k.label}
            </button>
          ))}
        </div>
        {spec.hint && <p className="mt-1.5 text-[11.5px] text-muted">{spec.hint}</p>}
      </div>
      <div className="grid grid-cols-2 gap-3">
        <div>
          <L>Name</L>
          <input className="field" value={name} placeholder={spec.label} onChange={(e) => setName(e.target.value)} />
        </div>
        <div>
          <L>Authentication</L>
          <select className="field" value={auth} onChange={(e) => setAuth(e.target.value as ProviderAuth)}>
            {spec.auth.map((a) => (
              <option key={a} value={a}>
                {AUTH_LABEL[a]}
              </option>
            ))}
          </select>
        </div>
      </div>
      <div>
        <L>{isKiro ? "kiro-cli path (optional)" : `Base URL ${kind === "azure_openai" || kind === "openai_compatible" ? "(required)" : ""}`}</L>
        <input
          className="field font-mono text-[12px]"
          value={baseUrl}
          placeholder={isKiro ? "auto-detect (PATH, ~/.local/bin)" : spec.base || "https://…"}
          onChange={(e) => setBaseUrl(e.target.value)}
        />
      </div>
      {isKiro && (
        <KiroPanel
          status={kiro}
          auth={auth}
          saved={!!initial}
          busy={busy}
          onSignIn={signIn}
          onRefresh={() => void refreshKiro()}
          onSignOut={async () => {
            if (!initial) return;
            try {
              await api.aiProviderSignOut(initial.id);
              toast(auth === "kiro_browser" ? "Signed out of kiro-cli" : "API key removed", "success");
              await onSaved(initial.id);
              await refreshKiro();
            } catch (e) {
              toast(toError(e).message, "error");
            }
          }}
        />
      )}
      {auth === "api_key" && (
        <div>
          <L>{isKiro ? "Kiro API key" : "API key"}</L>
          <input
            type="password"
            className="field"
            autoComplete="off"
            value={apiKey}
            placeholder={initial?.has_key ? "•••••••• (saved in keychain)" : isKiro ? "ksk_… (create one at app.kiro.dev → API Keys)" : "Paste key"}
            onChange={(e) => setApiKey(e.target.value)}
          />
          <p className="mt-1 text-[11px] text-muted">Stored in the OS keychain, never in the workspace database.</p>
        </div>
      )}
      {auth === "browser_openrouter" && (
        <div className="flex items-center gap-2 rounded-lg border border-line bg-panel-2 p-2.5">
          <Globe size={15} className="text-accent" />
          <div className="flex-1 text-[12.5px]">
            {initial?.has_key ? "Signed in (key stored in keychain)" : "Not signed in"}
          </div>
          <button className="btn-primary py-1" onClick={signIn} disabled={!!busy}>
            {busy === "signin" && <Loader2 size={12} className="animate-spin" />}
            {initial?.has_key ? "Sign in again" : "Sign in with browser"}
          </button>
        </div>
      )}
      {(auth === "azure_cli" || auth === "google_adc") && (
        <p className="rounded-lg border border-line bg-panel-2 p-2.5 text-[12px] text-muted">
          Uses your {auth === "azure_cli" ? "Azure CLI login (`az login`)" : "gcloud application-default login"} on this computer.
        </p>
      )}
      <div className="grid grid-cols-2 gap-3">
        <div>
          <L>{kind === "azure_openai" ? "Deployment" : isKiro ? "Model (auto = Kiro picks)" : "Default model"}</L>
          <div className="flex gap-1">
            <input className="field" list="prov-models" value={model} placeholder={spec.model} onChange={(e) => setModel(e.target.value)} />
            {initial && (
              <button className="btn-ghost shrink-0 border border-line px-2" title="Load models" onClick={loadModels} disabled={!!busy}>
                {busy === "models" ? <Loader2 size={12} className="animate-spin" /> : <Server size={12} />}
              </button>
            )}
          </div>
          <datalist id="prov-models">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </div>
        <div>
          <L>Fast model (inline edits)</L>
          <input className="field" list="prov-models" value={fastModel} placeholder="(optional)" onChange={(e) => setFastModel(e.target.value)} />
        </div>
      </div>
      {kind === "azure_openai" && (
        <div className="w-48">
          <L>API version</L>
          <input className="field" value={apiVersion} onChange={(e) => setApiVersion(e.target.value)} />
        </div>
      )}
      <div className="flex items-center justify-end gap-2 pt-1">
        {initial && (
          <button className="btn-ghost mr-auto text-danger" onClick={remove}>
            <Trash2 size={13} /> Remove
          </button>
        )}
        <button className="btn-primary" onClick={() => void save()} disabled={!!busy}>
          {busy === "save" ? <Loader2 size={13} className="animate-spin" /> : <KeyRound size={13} />} {initial ? "Save" : "Add provider"}
        </button>
      </div>
    </div>
  );
}

function Mcp() {
  const toast = useStore((s) => s.toast);
  const connections = useStore((s) => s.connections);
  const [cfg, setCfg] = useState<string>("");
  useEffect(() => {
    api
      .mcpConfig()
      .then((c) => setCfg(JSON.stringify(c, null, 2)))
      .catch(() => {});
  }, []);
  const enabled = connections.filter((c) => c.ai_policy?.mcp_enabled);
  return (
    <div className="space-y-3 text-[12.5px]">
      <p>
        DataBrain includes an MCP server (<span className="font-mono">databrain-mcp</span>) so external agents such as{" "}
        <strong>Kiro CLI</strong>, Claude Code or Cursor can search your schema and run read-only queries through DataBrain's
        policies. It runs over stdio on your machine only — it opens no network port.
      </p>
      <div>
        <div className="mb-1 text-[11.5px] font-medium text-muted">MCP client config (e.g. ~/.kiro/settings/mcp.json)</div>
        <div className="relative">
          <pre className="overflow-auto rounded-lg bg-panel-2 p-2.5 font-mono text-[11.5px] select-text">{cfg || "…"}</pre>
          <button
            className="icon-btn absolute right-1.5 top-1.5"
            aria-label="Copy config"
            onClick={() => void writeText(cfg).then(() => toast("Copied", "success"))}
          >
            <Copy size={13} />
          </button>
        </div>
      </div>
      <div>
        <div className="mb-1 text-[11.5px] font-medium text-muted">Connections exposed to MCP</div>
        {enabled.length === 0 ? (
          <p className="text-muted">None. Enable "Allow external agents (MCP)" in a connection's AI settings.</p>
        ) : (
          <ul className="list-disc pl-5">
            {enabled.map((c) => (
              <li key={c.id}>
                {c.name} <span className="text-muted">({c.ai_policy?.run_query === "auto_read" ? "reads auto-approved" : "schema only; queries need approval in DataBrain"})</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function Audit() {
  const [items, setItems] = useState<AuditEntry[]>([]);
  const connections = useStore((s) => s.connections);
  useEffect(() => {
    api.aiAudit().then(setItems).catch(() => {});
  }, []);
  return (
    <div className="max-h-[460px] overflow-auto text-[12px]">
      {items.length === 0 && <div className="py-8 text-center text-muted">No AI tool calls yet</div>}
      <table className="w-full">
        <tbody>
          {items.map((a) => (
            <tr key={a.id} className="border-b border-line/60 align-top">
              <td className="whitespace-nowrap py-1 pr-2 text-muted">{relativeTime(a.created_at)}</td>
              <td className="py-1 pr-2 font-mono">{a.tool}</td>
              <td className="py-1 pr-2">
                <span
                  className={`rounded px-1 text-[10.5px] ${
                    a.decision === "denied" || a.decision === "blocked" ? "bg-danger/15 text-danger" : a.decision === "approved" ? "bg-success/15 text-success" : "bg-panel-2 text-muted"
                  }`}
                >
                  {a.decision}
                </span>
              </td>
              <td className="py-1 pr-2 text-muted">{connections.find((c) => c.id === a.connection_id)?.name ?? ""}</td>
              <td className="max-w-[260px] truncate py-1 font-mono text-[11px] text-muted" title={JSON.stringify(a.args)}>
                {a.summary ?? JSON.stringify(a.args)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function KiroPanel({
  status,
  auth,
  saved,
  busy,
  onSignIn,
  onRefresh,
  onSignOut,
}: {
  status: KiroStatus | null;
  auth: ProviderAuth;
  saved: boolean;
  busy: string | null;
  onSignIn: () => void;
  onRefresh: () => void;
  onSignOut: () => void;
}) {
  const browser = auth === "kiro_browser";
  return (
    <div className="space-y-2 rounded-lg border border-line bg-panel-2 p-2.5 text-[12.5px]">
      <div className="flex items-center gap-2">
        {!saved ? (
          <span className="text-muted">Save the provider to check kiro-cli and sign in.</span>
        ) : !status ? (
          <span className="flex items-center gap-1.5 text-muted">
            <Loader2 size={12} className="animate-spin" /> Checking kiro-cli…
          </span>
        ) : status.signed_in ? (
          <span className="flex min-w-0 items-center gap-1.5">
            <Check size={13} className="shrink-0 text-success" />
            <span className="truncate">
              Signed in{status.identity ? ` as ${status.identity}` : ""}
              {status.account_type ? <span className="text-muted"> · {status.account_type}</span> : null}
            </span>
          </span>
        ) : (
          <span className="flex min-w-0 items-center gap-1.5">
            <AlertCircle size={13} className="shrink-0 text-warning" />
            <span className="truncate">{status.message ?? "Not signed in"}</span>
          </span>
        )}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {saved && (
            <button className="icon-btn h-7 w-7" title="Check again" aria-label="Check Kiro status" onClick={onRefresh} disabled={!!busy}>
              {busy === "status" ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
            </button>
          )}
          {browser && status?.signed_in && (
            <button className="btn-ghost py-1" onClick={onSignOut} disabled={!!busy}>
              <LogOut size={12} /> Sign out
            </button>
          )}
          {browser && (!status || !status.signed_in) && status?.installed !== false && (
            <button className="btn-primary py-1" onClick={onSignIn} disabled={!!busy}>
              {busy === "signin" ? <Loader2 size={12} className="animate-spin" /> : <Globe size={12} />} Sign in with browser
            </button>
          )}
          {!browser && saved && status?.signed_in && (
            <button className="btn-ghost py-1" onClick={onSignOut} disabled={!!busy}>
              <Trash2 size={12} /> Remove key
            </button>
          )}
        </div>
      </div>
      {status?.installed === false && (
        <p className="text-[11.5px] text-muted">
          Install Kiro CLI: <span className="font-mono select-text">curl -fsSL https://cli.kiro.dev/install | bash</span>, then check again.
        </p>
      )}
      {status?.cli_path && <p className="truncate font-mono text-[11px] text-muted" title={status.cli_path}>{status.cli_path}</p>}
      <p className="flex items-start gap-1.5 text-[11px] text-muted">
        <Terminal size={12} className="mt-px shrink-0" />
        {browser
          ? "Sign-in opens a terminal running `kiro-cli login`; finish in the browser and DataBrain continues automatically."
          : "The key stays in the OS keychain and is only passed to kiro-cli for each request. A kiro-cli browser session, if present, takes precedence."}
      </p>
      <p className="text-[11px] text-muted">
        Kiro runs with a DataBrain-managed agent (<span className="font-mono">~/.kiro/agents/databrain-sql.json</span>) that can only use
        DataBrain's tools; queries and editor changes still ask you here.
      </p>
    </div>
  );
}

