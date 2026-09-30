import { useState } from "react";
import { Loader2 } from "lucide-react";
import { api, toError } from "../lib/api";
import { useStore } from "../store";
import { Modal } from "./ui";

export function SaveQueryDialog() {
  const { open, tabId } = useStore((s) => s.saveQueryDialog);
  const tab = useStore((s) => s.tabs.find((t) => t.id === tabId));
  if (!open || !tab) return null;
  return <SaveQueryForm key={tab.id} tabId={tab.id} initialName={/^Query \d+$/.test(tab.title) ? "" : tab.title} />;
}

function SaveQueryForm({ tabId, initialName }: { tabId: string; initialName: string }) {
  const close = () => useStore.getState().setSaveQueryDialog({ open: false });
  const [name, setName] = useState(initialName);
  const [description, setDescription] = useState("");
  const [tags, setTags] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    const st = useStore.getState();
    const tab = st.tabs.find((t) => t.id === tabId);
    if (!tab || !name.trim()) return;
    setBusy(true);
    try {
      const saved = await api.saveQuery({
        id: "",
        name: name.trim(),
        sql: tab.sql,
        connection_id: tab.connection_id ?? null,
        folder_id: null,
        description: description.trim() || null,
        tags: tags.split(",").map((t) => t.trim()).filter(Boolean),
        created_at: 0,
        updated_at: 0,
      });
      st.updateTab(tabId, { saved_query_id: saved.id, title: saved.name, dirty: false });
      await st.refreshSavedQueries();
      st.toast(`Saved "${saved.name}"`, "success");
      close();
    } catch (e) {
      st.toast(toError(e).message, "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Save query"
      onClose={close}
      width={440}
      footer={
        <>
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={submit} disabled={!name.trim() || busy}>
            {busy && <Loader2 size={14} className="animate-spin" />} Save
          </button>
        </>
      }
    >
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div>
          <label htmlFor="sq-name" className="mb-1 block text-[11.5px] font-medium text-muted">
            Name
          </label>
          <input id="sq-name" className="field" value={name} placeholder="Monthly revenue" onChange={(e) => setName(e.target.value)} />
        </div>
        <div>
          <label htmlFor="sq-desc" className="mb-1 block text-[11.5px] font-medium text-muted">
            Description
          </label>
          <textarea id="sq-desc" className="field h-16 resize-none" value={description} onChange={(e) => setDescription(e.target.value)} />
        </div>
        <div>
          <label htmlFor="sq-tags" className="mb-1 block text-[11.5px] font-medium text-muted">
            Tags (comma separated)
          </label>
          <input id="sq-tags" className="field" value={tags} placeholder="finance, weekly" onChange={(e) => setTags(e.target.value)} />
        </div>
        <button type="submit" className="hidden" />
      </form>
    </Modal>
  );
}
