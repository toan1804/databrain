import { useEffect, useRef, useState } from "react";
import { Markdown } from "../Markdown";
import { kbd } from "../../lib/platform";

export function MarkdownCell({
  source,
  onChange,
  autoEdit,
}: {
  source: string;
  onChange: (s: string) => void;
  autoEdit: boolean;
}) {
  const [editing, setEditing] = useState(autoEdit);
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    if (editing && ref.current) {
      ref.current.focus();
      ref.current.style.height = "auto";
      ref.current.style.height = `${ref.current.scrollHeight + 2}px`;
    }
  }, [editing]);
  if (editing)
    return (
      <textarea
        ref={ref}
        className="field min-h-[60px] resize-none font-mono text-[12.5px]"
        value={source}
        aria-label="Markdown cell"
        placeholder={kbd("Markdown text…  (⌘↵ or Esc to finish)")}
        onChange={(e) => {
          onChange(e.target.value);
          e.target.style.height = "auto";
          e.target.style.height = `${e.target.scrollHeight + 2}px`;
        }}
        onBlur={() => setEditing(false)}
        onKeyDown={(e) => {
          if (
            e.key === "Escape" ||
            ((e.metaKey || e.ctrlKey) && e.key === "Enter")
          ) {
            e.preventDefault();
            setEditing(false);
          }
        }}
      />
    );
  return (
    <div
      role="button"
      tabIndex={0}
      className="min-h-[28px] cursor-text rounded-md px-1 py-1 hover:bg-hover/40"
      title="Double-click to edit"
      onDoubleClick={() => setEditing(true)}
      onKeyDown={(e) => e.key === "Enter" && setEditing(true)}
    >
      {source.trim() ? (
        <Markdown text={source} />
      ) : (
        <span className="text-[12.5px] text-muted">
          Empty text cell — double-click to edit
        </span>
      )}
    </div>
  );
}
