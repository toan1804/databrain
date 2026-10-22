import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowUp, Cloud, Database, File, FileSearch, Folder, Loader2, Plus, RefreshCw, TableProperties } from "lucide-react";
import { create } from "zustand";
import { api, toError } from "../lib/api";
import type { ConnectionView, StorageBucket, StorageObject } from "../lib/types";
import { asFolder, bucketNoun, groupListing, isReadable, isRemote, parentUrl, providerOf, storageOf } from "../lib/storage";
import { formatBytes } from "../lib/util";
import { useStore } from "../store";
import { localFilesConnection } from "./Sidebar";
import { Modal } from "./ui";

/** Most objects listed at once (the listing is recursive). */
const LIST_LIMIT = 5000;
/** "Public" choice: the local-files connection (no sign-in). */
const PUBLIC = "__public__";

interface State {
  open: boolean;
  connectionId: string | null;
  url: string;
}

const useRemoteFile = create<State>(() => ({ open: false, connectionId: null, url: "" }));

/** Open "Query a file in object storage", optionally on a connection and at a URL. */
export function openRemoteFile(opts: { connectionId?: string | null; url?: string } = {}) {
  useRemoteFile.setState({ open: true, connectionId: opts.connectionId ?? null, url: opts.url ?? "" });
}

/** DuckDB connections with object storage set up. */
function storageConnections(connections: ConnectionView[]) {
  return connections.filter((c) => storageOf(c));
}

export function RemoteFileDialog() {
  const open = useRemoteFile((s) => s.open);
  if (!open) return null;
  return <RemoteFileForm />;
}

function RemoteFileForm() {
  const initial = useRemoteFile.getState();
  const connections = useStore((s) => s.connections);
  const toast = useStore((s) => s.toast);
  const storages = useMemo(() => storageConnections(connections), [connections]);
  const [connId, setConnId] = useState<string>(() => initial.connectionId ?? storages[0]?.id ?? PUBLIC);
  const conn = connections.find((c) => c.id === connId);
  const storage = storageOf(conn);
  const [url, setUrl] = useState(() => initial.url || storage?.root || "");
  // The folder listed below, and what it holds (recursively).
  const [listed, setListed] = useState<{ prefix: string; objects: StorageObject[] } | null>(null);
  const [listing, setListing] = useState(false);
  // The account's buckets (containers), shown above the bucket root.
  const [buckets, setBuckets] = useState<StorageBucket[] | null>(null);
  const [busy, setBusy] = useState<"query" | "attach" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const close = () => useRemoteFile.setState({ open: false });

  // A connection added from here (New storage connection…) is picked.
  const known = useRef(new Set(storages.map((c) => c.id)));
  useEffect(() => {
    const added = storages.find((c) => !known.current.has(c.id));
    storages.forEach((c) => known.current.add(c.id));
    if (added) {
      setConnId(added.id);
      const root = storageOf(added)?.root;
      if (root) setUrl(root);
    }
  }, [storages]);

  /** Connection the request runs on (the local-files one for public URLs). */
  const target = async (): Promise<ConnectionView | undefined> => (connId === PUBLIC ? await localFilesConnection() : conn);

  const list = async (prefix: string) => {
    const mine = ++seq.current;
    setError(null);
    setListing(true);
    try {
      const c = await target();
      if (!c) throw new Error("Choose a storage connection");
      const objects = await api.storageList(c.id, prefix, LIST_LIMIT);
      if (seq.current === mine) {
        setListed({ prefix, objects });
        setBuckets(null);
      }
    } catch (e) {
      if (seq.current === mine) {
        setListed(null);
        setError(toError(e).message);
      }
    } finally {
      if (seq.current === mine) setListing(false);
    }
  };

  /** The folder of the URL: itself when it names a folder, else its parent. */
  const folderOf = (u: string) => {
    const t = u.trim();
    if (t.endsWith("/")) return t;
    const last = t.split("/").pop() ?? "";
    return last.includes(".") || /[*?[]/.test(last) ? (parentUrl(t) ?? asFolder(t)) : asFolder(t);
  };

  /** List the account's buckets (no URL needed). */
  const listBuckets = async () => {
    if (!conn || connId === PUBLIC) return;
    const mine = ++seq.current;
    setError(null);
    setListing(true);
    try {
      const list = await api.storageBuckets(conn.config, null, conn.id);
      if (seq.current === mine) {
        setBuckets(list);
        setListed(null);
      }
    } catch (e) {
      if (seq.current === mine) setError(toError(e).message);
    } finally {
      if (seq.current === mine) setListing(false);
    }
  };

  const browse = () => {
    // Nothing typed yet: the account's buckets.
    if (!url.trim() && conn && connId !== PUBLIC) return void listBuckets();
    if (!isRemote(url)) return setError(`Enter a URL such as ${providerOf(storage?.provider)?.example ?? "s3://bucket/path/"}`);
    void list(folderOf(url));
  };

  const query = async (u = url) => {
    const c = await target().catch(() => undefined);
    if (!c) return setError("Choose a storage connection");
    setBusy("query");
    setError(null);
    try {
      const sql = await api.storageScanSql(c.id, u.trim());
      const st = useStore.getState();
      const name = u.trim().replace(/\/+$/, "").split("/").pop() || "remote file";
      const id = st.newTab({ title: name, sql, connection_id: c.id });
      void st.runTab(id, "all", { doc: sql, selFrom: 0, selTo: 0, cursor: 0 });
      close();
    } catch (e) {
      setError(toError(e).message);
    } finally {
      setBusy(null);
    }
  };

  /** Add the URL to the connection's attached files: a view in `files`. */
  const attach = async () => {
    if (!conn || connId === PUBLIC) return;
    const u = url.trim();
    const files = (conn.config.options?.files ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
    if (files.includes(u)) return toast("Already attached to this connection", "info");
    setBusy("attach");
    setError(null);
    try {
      // Check that it can be read before saving it (a bad path fails the connection).
      await api.storageScanSql(conn.id, u);
      const { connected: _connected, ...profile } = conn;
      void _connected;
      await api.saveConnection({ ...profile, config: { ...conn.config, options: { ...conn.config.options, files: [...files, u].join("\n") } } }, null);
      const st = useStore.getState();
      await st.refreshConnections();
      st.toast(`Added to ${conn.name}: it shows as a view in the files schema`, "success");
      close();
    } catch (e) {
      setError(toError(e).message);
    } finally {
      setBusy(null);
    }
  };

  // Opened on a connection without a bucket: show its buckets right away.
  const autoListed = useRef(false);
  useEffect(() => {
    if (autoListed.current || url.trim() || !conn || connId === PUBLIC || conn.config.options?.storage_auth === "anonymous") return;
    autoListed.current = true;
    void listBuckets();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conn?.id]);

  const view = listed ? groupListing(listed.objects, listed.prefix) : null;
  // Up from a bucket root goes to the bucket list.
  const up = listed ? parentUrl(listed.prefix) : null;
  const canListBuckets = !!conn && connId !== PUBLIC && conn.config.options?.storage_auth !== "anonymous";
  const noun = bucketNoun(storage?.provider);
  const example = providerOf(storage?.provider)?.example ?? "https://example.com/data.parquet or s3://public-bucket/file.parquet";
  const canRun = isRemote(url) && !busy;

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <Cloud size={16} className="text-accent" /> Query a file in object storage
        </span>
      }
      onClose={close}
      width={680}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">Folders are read as Delta, Iceberg, or all their files (Hive partitions included).</span>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          {connId !== PUBLIC && (
            <button className="btn-ghost border border-line" disabled={!canRun || !conn} onClick={() => void attach()} title="Add to this connection's attached files: a view in the files schema">
              {busy === "attach" ? <Loader2 size={13} className="animate-spin" /> : <TableProperties size={13} />} Add as view
            </button>
          )}
          <button className="btn-primary" disabled={!canRun} onClick={() => void query()}>
            {busy === "query" ? <Loader2 size={13} className="animate-spin" /> : <FileSearch size={13} />} Open query
          </button>
        </>
      }
    >
      <div className="space-y-3">
        <div>
          <label htmlFor="rf-conn" className="mb-1 block text-[11.5px] font-medium text-muted">
            Storage
          </label>
          <div className="flex gap-2">
            <select
              id="rf-conn"
              className="field"
              value={connId}
              onChange={(e) => {
                setConnId(e.target.value);
                setListed(null);
                setBuckets(null);
                setError(null);
                const root = storageOf(connections.find((c) => c.id === e.target.value))?.root;
                setUrl(root ?? "");
              }}
            >
              {storages.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name} · {storageOf(c)?.label.split(" (")[0]}
                </option>
              ))}
              <option value={PUBLIC}>No sign-in (public buckets and HTTPS URLs)</option>
            </select>
            <button
              type="button"
              className="btn-ghost shrink-0 border border-line"
              onClick={() => useStore.getState().openConnectionDialog(null, null, { kind: "duckdb", section: "storage", options: { storage: "s3" } })}
            >
              <Plus size={13} /> New storage connection…
            </button>
          </div>
          {storages.length === 0 && (
            <p className="mt-1 text-[11.5px] text-muted">Private buckets need a connection with their keys: S3, Cloudflare R2, Google Cloud Storage or S3-compatible (MinIO…).</p>
          )}
        </div>

        <div>
          <label htmlFor="rf-url" className="mb-1 block text-[11.5px] font-medium text-muted">
            File or folder URL
          </label>
          <div className="flex gap-2">
            <input
              id="rf-url"
              className="field font-mono text-[12px]"
              value={url}
              placeholder={example}
              spellCheck={false}
              autoFocus
              onChange={(e) => setUrl(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  // Folders are opened in the browser, files are queried.
                  if (url.trim().endsWith("/") || !url.trim()) browse();
                  else void query();
                }
              }}
            />
            <button type="button" className="btn-ghost shrink-0 border border-line" onClick={browse} disabled={listing} title="List the folder's files">
              {listing ? <Loader2 size={13} className="animate-spin" /> : <Folder size={13} />} Browse
            </button>
            {canListBuckets && (
              <button type="button" className="btn-ghost shrink-0 border border-line" onClick={() => void listBuckets()} disabled={listing} title={`List the account's ${bucketNoun(storage?.provider, true)}`}>
                <Database size={13} /> {noun === "container" ? "Containers" : "Buckets"}
              </button>
            )}
          </div>
          <p className="mt-1 text-[11.5px] text-muted">
            A file (<span className="font-mono">.parquet .csv .json .xlsx</span>), a glob (<span className="font-mono">…/2024/*.parquet</span>) or a folder ending in <span className="font-mono">/</span>.
          </p>
        </div>

        {error && (
          <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12.5px] break-words text-danger select-text" role="alert">
            {error}
          </div>
        )}

        {buckets && (
          <div className="rounded-lg border border-line">
            <div className="flex items-center gap-1.5 border-b border-line px-2 py-1.5 text-[12px]">
              <span className="min-w-0 flex-1 truncate text-muted">
                {buckets.length.toLocaleString()} {bucketNoun(storage?.provider, buckets.length !== 1)} in {conn?.name}
              </span>
              <button className="icon-btn h-6 w-6" disabled={listing} title="List again" aria-label="List again" onClick={() => void listBuckets()}>
                <RefreshCw size={12} className={listing ? "animate-spin" : ""} />
              </button>
            </div>
            <div className="max-h-[300px] overflow-auto p-1" role="listbox" aria-label={bucketNoun(storage?.provider, true)}>
              {buckets.length === 0 && <div className="px-2 py-6 text-center text-[12px] text-muted">No {bucketNoun(storage?.provider, true)}</div>}
              {buckets.map((b) => {
                const u = `${storage?.scheme ?? "s3"}://${b.name}/`;
                return (
                  <div
                    key={b.name}
                    role="option"
                    aria-selected={url === u}
                    tabIndex={0}
                    title="Click to select; double-click to open"
                    className={`flex h-[26px] cursor-pointer items-center gap-2 rounded-md px-2 text-[12.5px] hover:bg-hover ${url === u ? "bg-hover" : ""}`}
                    onClick={() => setUrl(u)}
                    onDoubleClick={() => (setUrl(u), void list(u))}
                    onKeyDown={(e) => e.key === "Enter" && (setUrl(u), void list(u))}
                  >
                    <Database size={13} className="shrink-0 text-accent" />
                    <span className="min-w-0 flex-1 truncate font-mono">{b.name}</span>
                    {b.created && <span className="shrink-0 text-[11px] text-muted">{b.created.slice(0, 16).replace("T", " ")}</span>}
                  </div>
                );
              })}
            </div>
          </div>
        )}

        {listed && view && (
          <div className="rounded-lg border border-line">
            <div className="flex items-center gap-1.5 border-b border-line px-2 py-1.5 text-[12px]">
              <button
                className="icon-btn h-6 w-6"
                disabled={(!up && !canListBuckets) || listing}
                title={up ? "Up one folder" : `All ${bucketNoun(storage?.provider, true)}`}
                aria-label="Up one folder"
                onClick={() => (up ? (setUrl(up), void list(up)) : (setUrl(""), void listBuckets()))}
              >
                <ArrowUp size={13} />
              </button>
              <span className="min-w-0 flex-1 truncate font-mono text-[11.5px]" title={listed.prefix}>
                {listed.prefix}
              </span>
              <button className="icon-btn h-6 w-6" disabled={listing} title="List again" aria-label="List again" onClick={() => void list(listed.prefix)}>
                <RefreshCw size={12} className={listing ? "animate-spin" : ""} />
              </button>
            </div>
            <div className="max-h-[300px] overflow-auto p-1" role="listbox" aria-label="Objects">
              {view.folders.length === 0 && view.files.length === 0 && <div className="px-2 py-6 text-center text-[12px] text-muted">Nothing here</div>}
              {view.folders.map((f) => (
                <div
                  key={f.url}
                  role="option"
                  aria-selected={url === f.url}
                  tabIndex={0}
                  title="Click to select (Open query reads the whole folder); double-click to open it"
                  className={`flex h-[26px] cursor-pointer items-center gap-2 rounded-md px-2 text-[12.5px] hover:bg-hover ${url === f.url ? "bg-hover" : ""}`}
                  onClick={() => setUrl(f.url)}
                  onDoubleClick={() => (setUrl(f.url), void list(f.url))}
                  onKeyDown={(e) => e.key === "Enter" && (setUrl(f.url), void list(f.url))}
                >
                  <Folder size={13} className="shrink-0 text-accent" />
                  <span className="min-w-0 flex-1 truncate">{f.name}/</span>
                  <span className="shrink-0 text-[11px] text-muted">
                    {f.files.toLocaleString()} file{f.files === 1 ? "" : "s"} · {formatBytes(f.size)}
                  </span>
                </div>
              ))}
              {view.files.map((f) => (
                <div
                  key={f.path}
                  role="option"
                  aria-selected={url === f.path}
                  tabIndex={0}
                  title={isReadable(f.name) ? "Click to select; double-click to query" : "Not a file type DuckDB reads by name"}
                  className={`flex h-[26px] cursor-pointer items-center gap-2 rounded-md px-2 text-[12.5px] hover:bg-hover ${url === f.path ? "bg-hover" : ""} ${isReadable(f.name) ? "" : "text-muted"}`}
                  onClick={() => setUrl(f.path)}
                  onDoubleClick={() => isReadable(f.name) && void query(f.path)}
                  onKeyDown={(e) => e.key === "Enter" && isReadable(f.name) && void query(f.path)}
                >
                  <File size={13} className="shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate">{f.name}</span>
                  <span className="shrink-0 text-[11px] text-muted">
                    {f.size !== null ? formatBytes(f.size) : ""}
                    {f.modified ? ` · ${f.modified.slice(0, 16)}` : ""}
                  </span>
                </div>
              ))}
            </div>
            {listed.objects.length >= LIST_LIMIT && (
              <div className="border-t border-line px-2 py-1 text-[11px] text-muted">
                Showing the first {LIST_LIMIT.toLocaleString()} objects under this folder. Type a deeper path to see the rest.
              </div>
            )}
          </div>
        )}
      </div>
    </Modal>
  );
}
