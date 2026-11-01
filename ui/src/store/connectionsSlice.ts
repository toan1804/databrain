import type { StateCreator } from "zustand";
import { api, toError } from "../lib/api";
import type { ConnectionView, ConnectorInfo } from "../lib/types";
import type { SignInState, State } from "./types";

export interface ConnectionsSlice {
  signIn: SignInState | null;
  connectors: ConnectorInfo[];
  connections: ConnectionView[];
  /** The "Import connections" dialog. */
  importConnectionsOpen: boolean;
  setImportConnectionsOpen: (open: boolean) => void;
  connectionDialog: { open: boolean; profile?: ConnectionView | null; folderId?: string | null };
  refreshConnections: () => Promise<void>;
  openConnectionDialog: (profile?: ConnectionView | null, folderId?: string | null) => void;
  closeConnectionDialog: () => void;
  disconnect: (connId: string) => Promise<void>;
  /** Ask, then delete a connection (secrets and AI knowledge too). Resolves true when deleted. */
  deleteConnection: (connId: string) => Promise<boolean>;
  signInConnection: (connectionId: string) => Promise<boolean>;
  setSignIn: (s: SignInState | null) => void;
}

export const createConnectionsSlice: StateCreator<State, [], [], ConnectionsSlice> = (set, get) => ({
  signIn: null,
  connectors: [],
  connections: [],
  importConnectionsOpen: false,
  setImportConnectionsOpen: (open) => set({ importConnectionsOpen: open }),
  connectionDialog: { open: false },
  refreshConnections: async () => {
    const connections = await api.listConnections();
    set({ connections });
  },
  openConnectionDialog: (profile, folderId) => set({ connectionDialog: { open: true, profile, folderId: folderId ?? null } }),
  closeConnectionDialog: () => set({ connectionDialog: { open: false } }),

  deleteConnection: (connId) =>
    new Promise<boolean>((resolve) => {
      const conn = get().connections.find((c) => c.id === connId);
      if (!conn) return resolve(false);
      const tabs = get().tabs.filter((t) => t.connection_id === connId).length;
      get().askConfirm({
        title: `Delete connection "${conn.name}"?`,
        reasons: [
          "Removes the connection, its saved password/SSH/OAuth credentials and its AI knowledge (indexed schema and notes).",
          "Saved queries, history and notebooks are kept, without a connection.",
          ...(tabs ? [`${tabs} open tab${tabs === 1 ? "" : "s"} will have no connection.`] : []),
          ...(conn.env === "prod" ? ["This is a production connection."] : []),
        ],
        confirmLabel: "Delete",
        onCancel: () => resolve(false),
        onConfirm: async () => {
          try {
            await api.deleteConnection(connId);
          } catch (e) {
            // "partial": deleted, but a secret could not be removed.
            const err = toError(e);
            get().toast(err.message, err.kind === "partial" ? "info" : "error");
            if (err.kind !== "partial") return resolve(false);
          }
          const drop = <T,>(m: Record<string, T>) => Object.fromEntries(Object.entries(m).filter(([k]) => k !== connId && !k.startsWith(connId + "|")));
          if (get().schemaFilter[connId]) get().setSchemaFilter(connId, null);
          set((s) => ({
            schemas: drop(s.schemas),
            objects: drop(s.objects),
            columns: drop(s.columns),
            tabs: s.tabs.map((t) => (t.connection_id === connId ? { ...t, connection_id: null } : t)),
            catalogSearch: s.catalogSearch.scope === connId ? { ...s.catalogSearch, scope: null } : s.catalogSearch,
          }));
          await get().refreshConnections();
          get().toast(`Deleted "${conn.name}"`, "success");
          resolve(true);
        },
      });
    }),
  disconnect: async (connId) => {
    await api.disconnect(connId);
    set((s) => {
      const drop = <T,>(m: Record<string, T>) =>
        Object.fromEntries(Object.entries(m).filter(([k]) => !k.startsWith(connId)));
      return { schemas: drop(s.schemas), objects: drop(s.objects), columns: drop(s.columns), catalogs: drop(s.catalogs), catalogSchemas: drop(s.catalogSchemas), explorer: drop(s.explorer) };
    });
    await get().refreshConnections();
  },

  signInConnection: async (connectionId) => {
    const conn = get().connections.find((c) => c.id === connectionId);
    set({ signIn: { connectionId, label: `Sign in to ${conn?.name ?? "connection"}`, event: null } });
    try {
      const st = await api.signIn(connectionId);
      set({ signIn: null });
      get().toast(`Signed in${st.identity ? ` as ${st.identity}` : ""}`, "success");
      await get().refreshConnections();
      return true;
    } catch (e) {
      set({ signIn: null });
      const err = toError(e);
      if (err.kind !== "cancelled") get().toast(err.message, "error");
      return false;
    }
  },
  setSignIn: (signIn) => set({ signIn }),
});
