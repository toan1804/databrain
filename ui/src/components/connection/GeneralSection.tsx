// Connection form, General: type, name, address or file, color, environment.
import { traitsOf } from "../../lib/sourceTraits";
import { FilePlus2, FolderOpen, FolderPlus, Plus } from "lucide-react";
import type { EnvTag, SslMode } from "../../lib/types";
import { OracleAgentPanel, OracleClientPanel } from "../OracleClient";
import { COLORS, Check, ENVS, Label, REDSHIFT_KEYS, isRedshift } from "./parts";
import { ConnectionFormState } from "./useConnectionForm";

export function GeneralSection({ f }: { f: ConnectionFormState }) {
  const { kind, info, name, setName, values, setValue, ssl, setSsl, readOnly, setReadOnly, color, setColor, env, setEnv, fields, defaultName, portValid, browseDb, addFiles } = f;
  const redshift = kind === "postgres" && isRedshift(values);
  return (
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
          if (kind === "postgres" && f.key === "flavor")
            return (
              <div key={f.key} className="col-span-2">
                <Label htmlFor={id}>{f.label}</Label>
                <select
                  id={id}
                  className="field"
                  value={redshift ? "redshift" : ""}
                  onChange={(e) => {
                    setValue("flavor", e.target.value);
                    // Redshift listens on 5439.
                    if (e.target.value === "redshift" && (!values.port || values.port === "5432")) setValue("port", "5439");
                    if (e.target.value !== "redshift" && values.port === "5439") setValue("port", "");
                  }}
                >
                  <option value="">PostgreSQL (and compatible: Aurora, AlloyDB, Neon, Supabase…)</option>
                  <option value="redshift">Amazon Redshift (provisioned or Serverless)</option>
                </select>
              </div>
            );
          // Redshift's IAM settings only for Redshift.
          if (kind === "postgres" && REDSHIFT_KEYS.includes(f.key) && !redshift) return null;
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
          <Check
            checked={readOnly}
            onChange={setReadOnly}
            label="Read-only"
            help={
              ["postgres", "mysql", "sqlite", "duckdb", "clickhouse"].includes(kind)
                ? "Only queries run; the database session itself is read-only."
                : "Only queries run. This database has no read-only session, so a function or procedure that writes can't be stopped: use a database user without write permissions for a guarantee."
            }
          />
        </div>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <div>
          <Label htmlFor="c-timeout">Connect timeout (seconds)</Label>
          <input
            id="c-timeout"
            className="field"
            inputMode="numeric"
            placeholder="Settings default"
            value={values.connect_timeout_secs ?? ""}
            onChange={(e) => setValue("connect_timeout_secs", e.target.value.replace(/\D/g, ""))}
          />
          <p className="mt-1 text-[11.5px] text-muted">For this connection only, 3–600 (e.g. more over a VPN). Empty: Settings → Queries.</p>
        </div>
        <div>
          <Label htmlFor="c-idle">Close idle sessions after (minutes)</Label>
          <input
            id="c-idle"
            className="field"
            inputMode="numeric"
            placeholder="Settings default"
            value={values.idle_disconnect_minutes ?? ""}
            onChange={(e) => setValue("idle_disconnect_minutes", e.target.value.replace(/\D/g, ""))}
          />
          <p className="mt-1 text-[11.5px] text-muted">0 keeps them open (e.g. a warehouse that is slow to reconnect). Empty: Settings → Queries.</p>
        </div>
      </div>
    </>
  );
}
