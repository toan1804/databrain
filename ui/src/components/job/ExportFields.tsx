// Settings of a save-to-file step (folder, file name, format).
import { useEffect, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { api } from "../../lib/api";
import type { FileFormat, Job, JobNode } from "../../lib/types";
import { exportFileName } from "../../lib/jobGraph";
import { field, label } from "./shared";

/** Last part of a folder path ("Downloads" when none is set). */
export function folderName(f?: string | null): string {
  const t = f?.trim().replace(/[/\\]+$/, "");
  return t ? (t.split(/[/\\]/).pop() ?? t) : "Downloads";
}

/** Folder, file name and format of an export step. */
export function ExportFields({ job, node, onChange }: { job: Job; node: JobNode; onChange: (patch: Partial<JobNode>) => void }) {
  const [downloads, setDownloads] = useState<string | null>(null);
  useEffect(() => {
    api
      .defaultExportFolder()
      .then(setDownloads)
      .catch(() => {});
  }, []);
  const format = node.export_format ?? "csv";
  const folder = node.export_folder?.trim() || downloads || "Downloads";
  const preview = exportFileName(node.export_file, node.name, job.name, format);
  const choose = async () => {
    const picked = await openDialog({ directory: true, multiple: false, defaultPath: node.export_folder?.trim() || downloads || undefined, title: "Folder to save the file in" }).catch(() => null);
    if (typeof picked === "string") onChange({ export_folder: picked });
  };
  return (
    <div className="space-y-2 rounded-md border border-line p-2">
      <div>
        <label className={label}>Folder</label>
        <div className="flex gap-1">
          <div className={`${field} min-w-0 flex-1 truncate font-mono text-[11.5px]`} title={folder}>
            {folder}
          </div>
          <button className="btn-ghost shrink-0 py-1" onClick={() => void choose()}>
            <FolderOpen size={12} /> Choose…
          </button>
          {node.export_folder && (
            <button className="btn-ghost shrink-0 py-1" title="Use the Downloads folder" onClick={() => onChange({ export_folder: null })}>
              Downloads
            </button>
          )}
        </div>
      </div>
      <div className="grid grid-cols-[1fr_auto] gap-2">
        <div className="min-w-0">
          <label className={label} htmlFor={`file-${node.id}`}>
            File name
          </label>
          <input
            id={`file-${node.id}`}
            className={`${field} font-mono`}
            placeholder="{step}_{date}_{time}  (generated)"
            value={node.export_file ?? ""}
            onChange={(e) => onChange({ export_file: e.target.value || null })}
          />
        </div>
        <div>
          <label className={label} htmlFor={`fmt-${node.id}`}>
            Format
          </label>
          <select id={`fmt-${node.id}`} className={field} value={format} onChange={(e) => onChange({ export_format: e.target.value as FileFormat })}>
            <option value="csv">CSV</option>
            <option value="parquet">Parquet</option>
            <option value="json">JSON (lines)</option>
          </select>
        </div>
      </div>
      <div className="text-[11px] text-muted">
        Next run writes <span className="font-mono text-fg">{preview}</span>
        {node.export_file?.trim() && !/\{(date|time)\}/.test(node.export_file) ? " (replaced on each run)" : ""}. In the name, {"{step}"}, {"{job}"}, {"{date}"} and {"{time}"} are filled in; leave it empty for a new name per run.
      </div>
    </div>
  );
}
