import { useState } from "react";
import { Database, FileSearch, FolderPlus, Plus, Upload, Download } from "lucide-react";
import type { ConnectionView } from "../../lib/types";
import { CatalogSearch } from "../CatalogSearch";
import { FolderTree, createFolder } from "../FolderTree";
import { exportConnections } from "../ConnectionsIoDialog";
import { useStore } from "../../store";
import { MenuItem, MenuSeparator, Popover } from "../ui";
import { ConnectionNode } from "./ConnectionNode";
import { queryLocalFile } from "./queryLocalFile";
import { PanelHeader } from "./primitives";

export function ConnectionsPanel() {
  const connections = useStore((s) => s.connections);
  const openDialog = useStore((s) => s.openConnectionDialog);
  const query = useStore((s) => s.catalogSearch.query.trim().toLowerCase());
  const named = query ? connections.filter((c) => c.name.toLowerCase().includes(query)) : connections;
  // Right-click on empty space: new connection / new folder.
  const [blankMenu, setBlankMenu] = useState<{ x: number; y: number } | null>(null);
  const onBlankContextMenu = (e: React.MouseEvent<HTMLDivElement>) => {
    // Rows (connections, folders, objects) open their own menu and call preventDefault;
    // inputs keep the native menu; menus rendered in portals are not inside this panel.
    if (e.defaultPrevented) return;
    const target = e.target as HTMLElement;
    if (!e.currentTarget.contains(target) || target.closest("input, textarea, [contenteditable='true'], [role='treeitem'], [role='option']")) return;
    e.preventDefault();
    setBlankMenu({ x: e.clientX, y: e.clientY });
  };
  const tree = (items: ConnectionView[], filtering: boolean) => (
    <div role="tree" aria-label="Connections">
      <FolderTree
        kind="connections"
        items={items}
        itemId={(c) => c.id}
        itemFolder={(c) => c.folder_id}
        filtering={filtering}
        renderItem={(c) => <ConnectionNode conn={c} />}
        newItem={{ label: "New connection here", create: (folderId) => openDialog(null, folderId) }}
      />
    </div>
  );
  return (
    <>
      <PanelHeader title="Connections">
        <button className="icon-btn" title="Query a local file (CSV, Parquet, JSON, Excel) with DuckDB" aria-label="Query a local file" onClick={() => void queryLocalFile()}>
          <FileSearch size={14} />
        </button>
        <button className="icon-btn" title="New folder" aria-label="New folder" onClick={() => void createFolder("connections")}>
          <FolderPlus size={14} />
        </button>
        <button className="icon-btn" title="New connection" aria-label="New connection" onClick={() => openDialog(null)}>
          <Plus size={15} />
        </button>
      </PanelHeader>
      <div className="flex min-h-0 flex-1 flex-col" onContextMenu={onBlankContextMenu}>
        {connections.length === 0 ? (
          <div className="px-3 py-8 text-center text-[12.5px] text-muted">
            <Database size={28} className="mx-auto mb-3 opacity-50" />
            No connections yet.
            <button className="btn-primary mx-auto mt-3" onClick={() => openDialog(null)}>
              <Plus size={14} /> Add connection
            </button>
          </div>
        ) : (
          <CatalogSearch tree={tree(connections, false)} connectionsTree={named.length ? tree(named, true) : null} />
        )}
      </div>
      {blankMenu && (
        <Popover x={blankMenu.x} y={blankMenu.y} onClose={() => setBlankMenu(null)} className="w-52">
          <MenuItem icon={<Plus size={13} />} label="New connection…" onClick={() => (setBlankMenu(null), openDialog(null))} />
          <MenuItem icon={<FolderPlus size={13} />} label="New folder" onClick={() => (setBlankMenu(null), void createFolder("connections"))} />
          <MenuSeparator />
          <MenuItem icon={<FileSearch size={13} />} label="Query a local file…" onClick={() => (setBlankMenu(null), void queryLocalFile())} />
          <MenuItem icon={<Upload size={13} />} label="Import a file into a table…" onClick={() => (setBlankMenu(null), void queryLocalFile(true))} />
          <MenuSeparator />
          <MenuItem icon={<Download size={13} />} label="Import connections…" onClick={() => (setBlankMenu(null), useStore.getState().setImportConnectionsOpen(true))} />
          <MenuItem icon={<Upload size={13} />} label="Export connections…" onClick={() => (setBlankMenu(null), void exportConnections())} />
        </Popover>
      )}
    </>
  );
}
