import { useState, type ReactNode } from "react";
import { ChevronDown, ChevronRight, Folder as FolderIcon, FolderOpen, FolderPlus, Pencil, Trash2 } from "lucide-react";
import { api, toError } from "../lib/api";
import type { Folder, FolderKind } from "../lib/types";
import { useStore } from "../store";
import { MenuItem, Popover } from "./ui";

const DRAG_MIME = "application/x-databrain-item";

/** Props to make an item draggable into folders. */
export function dragProps(kind: FolderKind, id: string) {
  return {
    draggable: true,
    onDragStart: (e: React.DragEvent) => {
      e.dataTransfer.setData(DRAG_MIME, JSON.stringify({ kind, id }));
      e.dataTransfer.effectAllowed = "move";
    },
  };
}

function useExpanded(kind: FolderKind) {
  const storageKey = `db.folders.${kind}`;
  const [open, setOpen] = useState<Set<string>>(() => new Set(JSON.parse(localStorage.getItem(storageKey) ?? "[]")));
  const toggle = (id: string, force?: boolean) =>
    setOpen((s) => {
      const n = new Set(s);
      if (force ?? !n.has(id)) n.add(id);
      else n.delete(id);
      localStorage.setItem(storageKey, JSON.stringify([...n]));
      return n;
    });
  return { open, toggle };
}

export async function createFolder(kind: FolderKind, parent: string | null = null) {
  const st = useStore.getState();
  const existing = new Set(st.folders[kind].map((f) => f.name));
  let name = "New folder";
  for (let i = 2; existing.has(name); i++) name = `New folder ${i}`;
  try {
    const f = await api.saveFolder({ id: "", parent_id: parent, name, kind });
    await st.refreshFolders(kind);
    return f;
  } catch (e) {
    st.toast(toError(e).message, "error");
    return null;
  }
}

/**
 * Folder tree for connections / saved queries / notebooks. Items are rendered
 * by the caller; items without a folder go at the root below the folders.
 */
export function FolderTree<T>({
  kind,
  items,
  itemId,
  itemFolder,
  renderItem,
  filtering,
}: {
  kind: FolderKind;
  items: T[];
  itemId: (t: T) => string;
  itemFolder: (t: T) => string | null | undefined;
  renderItem: (t: T) => ReactNode;
  /** While searching, show matches flat (folders with no match hidden). */
  filtering?: boolean;
}) {
  const folders = useStore((s) => s.folders[kind]);
  const refreshFolders = useStore((s) => s.refreshFolders);
  const toast = useStore((s) => s.toast);
  const { open, toggle } = useExpanded(kind);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ folder: Folder; x: number; y: number } | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);

  const refreshItems = async () => {
    const st = useStore.getState();
    if (kind === "connections") await st.refreshConnections();
    else if (kind === "queries") await st.refreshSavedQueries();
    else await st.refreshNotebooks();
  };

  const onDrop = async (e: React.DragEvent, folderId: string | null) => {
    e.preventDefault();
    setDropTarget(null);
    const raw = e.dataTransfer.getData(DRAG_MIME);
    if (!raw) return;
    const { kind: k, id } = JSON.parse(raw) as { kind: FolderKind; id: string };
    if (k !== kind) return;
    try {
      await api.moveToFolder(kind, id, folderId);
      await refreshItems();
      if (folderId) toggle(folderId, true);
    } catch (err) {
      toast(toError(err).message, "error");
    }
  };

  const dropProps = (folderId: string | null) => ({
    onDragOver: (e: React.DragEvent) => {
      if (!e.dataTransfer.types.includes(DRAG_MIME)) return;
      e.preventDefault();
      e.stopPropagation();
      setDropTarget(folderId ?? "__root__");
    },
    onDragLeave: () => setDropTarget((t) => (t === (folderId ?? "__root__") ? null : t)),
    onDrop: (e: React.DragEvent) => {
      e.stopPropagation();
      void onDrop(e, folderId);
    },
  });

  const rename = async (f: Folder, name: string) => {
    setRenaming(null);
    if (!name.trim() || name === f.name) return;
    try {
      await api.saveFolder({ ...f, name: name.trim() });
      await refreshFolders(kind);
    } catch (e) {
      toast(toError(e).message, "error");
    }
  };

  const remove = (f: Folder) =>
    useStore.getState().askConfirm({
      title: `Delete folder "${f.name}"?`,
      reasons: ["The folder and its sub-folders are removed. Items inside move to the top level."],
      confirmLabel: "Delete folder",
      onConfirm: async () => {
        try {
          await api.deleteFolder(f.id);
          await Promise.all([refreshFolders(kind), refreshItems()]);
        } catch (e) {
          toast(toError(e).message, "error");
        }
      },
    });

  const known = new Set(folders.map((f) => f.id));
  const inFolder = (fid: string | null) => items.filter((i) => (itemFolder(i) && known.has(itemFolder(i)!) ? itemFolder(i) : null) === fid);
  const count = (fid: string): number =>
    inFolder(fid).length + folders.filter((f) => f.parent_id === fid).reduce((n, f) => n + count(f.id), 0);

  const renderFolder = (f: Folder, depth: number): ReactNode => {
    const n = count(f.id);
    if (filtering && n === 0) return null;
    const expanded = filtering || open.has(f.id);
    return (
      <div key={f.id}>
        <div
          role="treeitem"
          aria-expanded={expanded}
          tabIndex={0}
          {...dropProps(f.id)}
          onClick={() => toggle(f.id)}
          onKeyDown={(e) => e.key === "Enter" && toggle(f.id)}
          onContextMenu={(e) => {
            e.preventDefault();
            setMenu({ folder: f, x: e.clientX, y: e.clientY });
          }}
          onDoubleClick={() => setRenaming(f.id)}
          className={`group flex h-[26px] cursor-pointer items-center gap-1.5 rounded-md pr-1 text-[13px] hover:bg-hover ${
            dropTarget === f.id ? "bg-accent/15 ring-1 ring-accent/50" : ""
          }`}
          style={{ paddingLeft: 4 + depth * 14 }}
        >
          <span className="flex w-3.5 justify-center text-muted">{expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}</span>
          <span className="text-muted">{expanded ? <FolderOpen size={13} /> : <FolderIcon size={13} />}</span>
          {renaming === f.id ? (
            <input
              autoFocus
              className="min-w-0 flex-1 rounded bg-panel-2 px-1 outline-none"
              defaultValue={f.name}
              aria-label="Folder name"
              onClick={(e) => e.stopPropagation()}
              onBlur={(e) => void rename(f, e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                if (e.key === "Escape") setRenaming(null);
              }}
            />
          ) : (
            <span className="min-w-0 flex-1 truncate">{f.name}</span>
          )}
          <span className="text-[11px] text-muted">{n || ""}</span>
        </div>
        {expanded && (
          <div>
            {folders.filter((c) => c.parent_id === f.id).map((c) => renderFolder(c, depth + 1))}
            {inFolder(f.id).map((i) => (
              <div key={itemId(i)} style={{ paddingLeft: (depth + 1) * 14 }}>
                {renderItem(i)}
              </div>
            ))}
          </div>
        )}
      </div>
    );
  };

  return (
    <div {...dropProps(null)} className={`min-h-[40px] ${dropTarget === "__root__" ? "rounded-md bg-accent/5" : ""}`}>
      {folders.filter((f) => !f.parent_id || !known.has(f.parent_id)).map((f) => renderFolder(f, 0))}
      {inFolder(null).map((i) => (
        <div key={itemId(i)}>{renderItem(i)}</div>
      ))}
      {menu && (
        <Popover x={menu.x} y={menu.y} onClose={() => setMenu(null)} className="w-48">
          <MenuItem icon={<Pencil size={13} />} label="Rename" onClick={() => (setMenu(null), setRenaming(menu.folder.id))} />
          <MenuItem
            icon={<FolderPlus size={13} />}
            label="New subfolder"
            onClick={() => {
              setMenu(null);
              void createFolder(kind, menu.folder.id).then((f) => f && (toggle(menu.folder.id, true), setRenaming(f.id)));
            }}
          />
          <MenuItem icon={<Trash2 size={13} />} label="Delete folder" danger onClick={() => (setMenu(null), remove(menu.folder))} />
        </Popover>
      )}
    </div>
  );
}
