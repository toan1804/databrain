import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { CheckCircle2, Copy, Info, Loader2, Square, Trash2, XCircle } from "lucide-react";
import { toError } from "../lib/api";
import type { ConnectionProfile } from "../lib/types";
import { useStore } from "../store";
import { AskAiAboutError, connectionErrorQuestion } from "./AskAiError";
import { Modal } from "./ui";
import { AiSection } from "./connection/AiSection";
import { AuthSection } from "./connection/AuthSection";
import { GeneralSection } from "./connection/GeneralSection";
import { SshSection } from "./connection/SshSection";
import { Label } from "./connection/parts";
import { useConnectionForm } from "./connection/useConnectionForm";

export function ConnectionDialog() {
  const { open, profile, folderId } = useStore((s) => s.connectionDialog);
  if (!open) return null;
  return <ConnectionForm key={profile?.id ?? "new"} initial={profile ?? null} folderId={folderId ?? null} />;
}

function ConnectionForm({ initial, folderId }: { initial: ConnectionProfile | null; folderId: string | null }) {
  const f = useConnectionForm(initial, folderId);
  const { testSecs, close, isEdit, sorted, section, kind, info, busy, testResult, changeKind, usesFile, buildProfile, missing, canSubmit, test, cancelTest, save, remove, sectionBtn } = f;
  return (
    <Modal
      title={isEdit ? "Edit connection" : "New connection"}
      onClose={close}
      width={640}
      footer={
        <>
          {isEdit && (
            <button className="btn-ghost mr-auto text-danger" onClick={remove}>
              <Trash2 size={14} /> Delete
            </button>
          )}
          {!canSubmit && missing.length > 0 && <span className="mr-auto truncate text-[11.5px] text-muted">Required: {missing.join(", ")}</span>}
          {busy === "test" ? (
            <button className="btn-ghost" onClick={cancelTest} title="Stop the connection test">
              <Loader2 size={14} className="animate-spin" /> Testing{testSecs ? ` ${testSecs}s` : "…"} <Square size={11} className="ml-1" /> Stop
            </button>
          ) : (
            <button className="btn-ghost" onClick={test} disabled={!canSubmit || !!busy}>
              Test
            </button>
          )}
          <button className="btn-ghost" onClick={close}>
            Cancel
          </button>
          <button className="btn-primary" onClick={save} disabled={!canSubmit || !!busy}>
            {busy === "save" && <Loader2 size={14} className="animate-spin" />}
            {isEdit ? "Save" : "Add connection"}
          </button>
        </>
      }
    >
      <form
        className="space-y-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (canSubmit) void save();
        }}
      >
        {!isEdit && (
          <div>
            <Label>Database type</Label>
            <div className="grid grid-cols-3 gap-2">
              {sorted.map((c) => (
                <button
                  type="button"
                  key={c.kind}
                  onClick={() => changeKind(c.kind)}
                  aria-pressed={kind === c.kind}
                  className={`rounded-lg border px-3 py-2 text-left text-[13px] transition-colors ${
                    kind === c.kind ? "border-accent bg-accent/10" : "border-line hover:bg-hover"
                  }`}
                >
                  {c.display_name}
                </button>
              ))}
            </div>
          </div>
        )}

        <div className="flex gap-1 border-b border-line pb-2" role="tablist">
          {sectionBtn("general", "General")}
          {sectionBtn("auth", "Authentication", !usesFile)}
          {sectionBtn("ssh", "SSH tunnel", !!info?.capabilities.ssh)}
          {sectionBtn("ai", "AI")}
        </div>

        {info?.note && section !== "ai" && (
          <div className="flex items-start gap-2 rounded-lg bg-panel-2 px-3 py-2 text-[12px] text-muted">
            <Info size={13} className="mt-0.5 shrink-0" /> {info.note}
          </div>
        )}

        {section === "general" && <GeneralSection f={f} />}

        {section === "auth" && !usesFile && <AuthSection f={f} />}

        {section === "ssh" && info?.capabilities.ssh && <SshSection f={f} />}

        {section === "ai" && <AiSection f={f} />}

        {testResult && (
          <div
            className={`flex flex-wrap items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px] ${
              testResult.ok ? "border-success/40 bg-success/10" : "border-danger/40 bg-danger/10"
            }`}
            role="status"
          >
            {testResult.ok ? <CheckCircle2 size={15} className="mt-px shrink-0 text-success" /> : <XCircle size={15} className="mt-px shrink-0 text-danger" />}
            <span className="min-w-0 flex-1 break-words select-text">{testResult.text}</span>
            {!testResult.ok && testResult.text !== "Test cancelled" && (
              <AskAiAboutError error={testResult.text} connectionId={initial?.id ?? null} question={() => connectionErrorQuestion(testResult.text, buildProfile().config)} />
            )}
            {!testResult.ok && (
              <button
                type="button"
                className="icon-btn -my-1 h-6 w-6 shrink-0"
                aria-label="Copy the error"
                title="Copy the error"
                onClick={() =>
                  void writeText(testResult.text)
                    .then(() => useStore.getState().toast("Error copied", "success"))
                    .catch((e) => useStore.getState().toast(toError(e).message, "error"))
                }
              >
                <Copy size={13} />
              </button>
            )}
          </div>
        )}
        <button type="submit" className="hidden" />
      </form>
    </Modal>
  );
}
