// Connection form, Sign-in: method, user, password or token, OAuth, CLI.
import { Globe, Loader2, LogOut } from "lucide-react";
import { AUTH_LABELS, CLI_HELP, CLI_LABEL, Label, SecretField, TOKEN_LABEL, isRedshift } from "./parts";
import { ConnectionFormState } from "./useConnectionForm";

export function AuthSection({ f }: { f: ConnectionFormState }) {
  const { kind, info, authKind, setAuthKind, user, setUser, oauth, setOauth, cliProfile, setCliProfile, secret, setSecret, passphrase, setPassphrase, clearSecret, setClearSecret, authStatus, busy, interactive, needsUser, needsClientId, canSubmit, signIn, signOut, secretPlaceholder, initial, values } = f;
  // Postgres' CLI login is Redshift IAM: only for Redshift connections.
  const methods = (info?.auth_methods ?? []).filter((m) => !(kind === "postgres" && m === "cloud_cli" && !isRedshift(values) && authKind !== "cloud_cli"));
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Authentication method">
        {methods.map((m) => (
          <button
            type="button"
            key={m}
            role="radio"
            aria-checked={authKind === m}
            onClick={() => setAuthKind(m)}
            className={`rounded-md border px-2.5 py-1 text-[12.5px] ${authKind === m ? "border-accent bg-accent/10" : "border-line hover:bg-hover"}`}
          >
            {m === "api_token" ? (TOKEN_LABEL[kind] ?? AUTH_LABELS[m]) : m === "cloud_cli" ? (CLI_LABEL[kind] ?? AUTH_LABELS[m]) : AUTH_LABELS[m]}
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
          {(kind === "databricks" || kind === "postgres") && (
            <div>
              <Label htmlFor="c-prof">{kind === "postgres" ? "AWS profile (optional)" : "CLI profile (optional)"}</Label>
              <input id="c-prof" className="field" value={cliProfile} placeholder={kind === "postgres" ? "default (or AWS_PROFILE)" : "DEFAULT"} onChange={(e) => setCliProfile(e.target.value)} />
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
  );
}
