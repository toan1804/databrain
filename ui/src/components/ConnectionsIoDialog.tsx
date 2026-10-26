import { useEffect, useState } from "react";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { FolderOpen, KeyRound, Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { traitsOf } from "../lib/sourceTraits";
import type { ImportPreview, ImportSource } from "../lib/types";
import { useStore } from "../store";
import { EnvBadge, Modal } from "./ui";

const SOURCES: { id: ImportSource; label: string; hint: string }[] = [
  { id: "file", label: "DataBrain file", hint: "A .databrain-connections.json file exported from DataBrain (no passwords)." },
  {
    id: "dbeaver",
    label: "DBeaver",
    hint: "DBeaver's data-sources.json. Users and passwords come from credentials-config.json next to it (not when DBeaver has a master password); missing ones are entered after import.",
  },
  {
    id: "datagrip",
    label: "DataGrip",
    hint: "DataGrip's global data sources (options/dataSources.xml of the newest DataGrip), or choose a project's .idea/dataSources.xml. Users come along; passwords stay in DataGrip's keychain: enter them after import.",
  },
  { id: "pgpass", label: "pgpass", hint: "PostgreSQL's ~/.pgpass. Its passwords are saved to the keychain." },
];

/** Write every connection to a file (names, folders, settings; no secrets). */
export async function exportConnections() {
  const st = useStore.getState();
  const path = await saveDialog({ defaultPath: "connections.databrain-connections.json", filters: [{ name: "DataBrain connections", extensions: ["json"] }] });
  if (!path) return;
  try {
    const n = await api.exportConnections(null, path);
    st.toast(`Exported ${n} connection${n === 1 ? "" : "s"} (without passwords or keys)`, "success");
  } catch (e) {
    st.toast(toError(e).message, "error");
  }
}

export function ImportConnectionsDialog({ onClose }: { onClose: () => void }) {
  const toast = useStore((s) => s.toast);
  const [source, setSource] = useState<ImportSource>("dbeaver");
  const [path, setPath] = useState<string | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState(false);

  // DBeaver, DataGrip and pgpass have a usual place: look there first.
  useEffect(() => {
    setPreview(null);
    setError(null);
    if (source === "file" && !path) return;
    let alive = true;
    setBusy(true);
    api.importConnectionsPreview(source, path).then(
      (p) => {
        if (!alive) return;
        setPreview(p);
        setPicked(new Set(p.candidates.filter((c) => !c.existing).map((c) => c.index)));
      },
      (e) => alive && setError(toError(e).message),
    ).finally(() => alive && setBusy(false));
    return () => {
      alive = false;
    };
  }, [source, path]);

  const browse = async () => {
    const filters = source === "pgpass" ? [] : source === "datagrip" ? [{ name: "DataGrip data sources", extensions: ["xml"] }] : [{ name: "JSON", extensions: ["json"] }];
    const p = await openDialog({ multiple: false, directory: false, filters });
    if (typeof p === "string") setPath(p);
  };

  const run = async () => {
    setBusy(true);
    try {
      const r = await api.importConnections(source, preview?.path ?? path, [...picked]);
      await useStore.getState().refreshConnections();
      await useStore.getState().refreshFolders("connections");
      if (r.errors.length) toast(`Imported ${r.imported}; not imported: ${r.errors.join("; ")}`, "error");
      else toast(`Imported ${r.imported} connection${r.imported === 1 ? "" : "s"}`, "success");
      onClose();
    } catch (e) {
      toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };

  const all = preview?.candidates ?? [];
  return (
    <Modal
      title="Import connections"
      width={680}
      onClose={onClose}
      footer={
        <>
          <span className="mr-auto text-[11.5px] text-muted">{all.length ? `${picked.size} of ${all.length} selected` : ""}</span>
          <button className="btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-primary" disabled={busy || picked.size === 0} onClick={() => void run()}>
            {busy && <Loader2 size={13} className="animate-spin" />} Import
          </button>
        </>
      }
    >
      <div className="flex flex-col gap-3 text-[12.5px]">
        <div className="flex rounded-md border border-line p-0.5" role="radiogroup" aria-label="Import from">
          {SOURCES.map((s) => (
            <button
              key={s.id}
              role="radio"
              aria-checked={source === s.id}
              onClick={() => (setSource(s.id), setPath(null))}
              className={`h-7 flex-1 rounded text-[12px] ${source === s.id ? "bg-hover text-fg" : "text-muted hover:text-fg"}`}
            >
              {s.label}
            </button>
          ))}
        </div>
        <p className="text-[11.5px] text-muted">{SOURCES.find((s) => s.id === source)?.hint}</p>
        <div className="flex items-center gap-2">
          <input className="field flex-1 font-mono text-[11.5px]" readOnly aria-label="File" value={preview?.path ?? path ?? ""} placeholder={source === "file" ? "Choose a file…" : "Usual location"} />
          <button className="btn-ghost border border-line" onClick={() => void browse()}>
            <FolderOpen size={13} /> Choose…
          </button>
        </div>
        {error && (
          <p className="text-danger" role="alert">
            {error}
          </p>
        )}
        {busy && !preview && (
          <p className="flex items-center gap-2 text-muted">
            <Loader2 size={13} className="animate-spin" /> Reading…
          </p>
        )}
        {preview && all.length === 0 && <p className="text-muted">No connections DataBrain can use in this file.</p>}
        {all.length > 0 && (
          <div className="max-h-[50vh] overflow-auto rounded-md border border-line">
            <label className="flex items-center gap-2 border-b border-line bg-panel-2 px-2 py-1 text-[11.5px] text-muted">
              <input type="checkbox" checked={picked.size === all.length} onChange={(e) => setPicked(new Set(e.target.checked ? all.map((c) => c.index) : []))} />
              All
            </label>
            {all.map((c) => (
              <label key={c.index} className="flex cursor-pointer items-start gap-2 border-b border-line/60 px-2 py-1.5 last:border-b-0 hover:bg-hover">
                <input
                  type="checkbox"
                  className="mt-0.5"
                  checked={picked.has(c.index)}
                  onChange={(e) => {
                    const n = new Set(picked);
                    if (e.target.checked) n.add(c.index);
                    else n.delete(c.index);
                    setPicked(n);
                  }}
                />
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-1.5">
                    <span className="font-medium">{c.name}</span>
                    <span className="text-[11px] text-muted">{traitsOf(c.kind).name}</span>
                    {c.env !== "none" && <EnvBadge env={c.env} />}
                    {c.has_password && (
                      <span title="Comes with its password" className="text-muted">
                        <KeyRound size={11} aria-label="with password" />
                      </span>
                    )}
                  </span>
                  <span className="block truncate font-mono text-[11px] text-muted">
                    {c.folder.length > 0 && `${c.folder.join(" / ")} · `}
                    {c.user ? `${c.user}@` : ""}
                    {c.target}
                  </span>
                  {c.existing && <span className="block text-[11px] text-warning">Already here as “{c.existing}”</span>}
                  {c.note && <span className="block text-[11px] text-muted">{c.note}</span>}
                </span>
              </label>
            ))}
          </div>
        )}
      </div>
    </Modal>
  );
}
