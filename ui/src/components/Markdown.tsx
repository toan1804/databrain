// Minimal Markdown renderer that builds React elements (never raw HTML), so
// model output and notebook text cannot inject markup into the webview.
// Supports headings, paragraphs, lists, block quotes, fenced code, tables,
// inline code, bold, italics and links (opened externally, http(s) only).

import { Fragment, type ReactNode } from "react";

export type CodeRenderer = (code: string, lang: string, key: number) => ReactNode;

type Block =
  | { t: "h"; level: number; text: string }
  | { t: "p"; text: string }
  | { t: "ul" | "ol"; items: string[] }
  | { t: "quote"; text: string }
  | { t: "code"; lang: string; code: string }
  | { t: "table"; head: string[]; rows: string[][] }
  | { t: "hr" };

export function parseBlocks(src: string): Block[] {
  const lines = src.replace(/\r\n/g, "\n").split("\n");
  const out: Block[] = [];
  let i = 0;
  const isSpecial = (l: string) => /^(#{1,6}\s|```|>\s?|\s*[-*+]\s|\s*\d+[.)]\s|\|)/.test(l) || /^(-{3,}|\*{3,})\s*$/.test(l);
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    const fence = line.match(/^```\s*([\w+-]*)/);
    if (fence) {
      const code: string[] = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) code.push(lines[i++]);
      i++; // closing fence (or EOF while streaming)
      out.push({ t: "code", lang: fence[1].toLowerCase(), code: code.join("\n") });
      continue;
    }
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      out.push({ t: "h", level: h[1].length, text: h[2] });
      i++;
      continue;
    }
    if (/^(-{3,}|\*{3,})\s*$/.test(line)) {
      out.push({ t: "hr" });
      i++;
      continue;
    }
    if (line.startsWith(">")) {
      const q: string[] = [];
      while (i < lines.length && lines[i].startsWith(">")) q.push(lines[i++].replace(/^>\s?/, ""));
      out.push({ t: "quote", text: q.join(" ") });
      continue;
    }
    if (/^\s*[-*+]\s/.test(line) || /^\s*\d+[.)]\s/.test(line)) {
      const ordered = /^\s*\d+[.)]\s/.test(line);
      const items: string[] = [];
      while (i < lines.length && (ordered ? /^\s*\d+[.)]\s/ : /^\s*[-*+]\s/).test(lines[i])) {
        items.push(lines[i].replace(ordered ? /^\s*\d+[.)]\s/ : /^\s*[-*+]\s/, ""));
        i++;
        // Continuation lines.
        while (i < lines.length && /^\s{2,}\S/.test(lines[i]) && !/^\s*([-*+]|\d+[.)])\s/.test(lines[i])) {
          items[items.length - 1] += " " + lines[i++].trim();
        }
      }
      out.push({ t: ordered ? "ol" : "ul", items });
      continue;
    }
    if (line.startsWith("|") && i + 1 < lines.length && /^\|?\s*:?-{2,}/.test(lines[i + 1])) {
      const cells = (l: string) => l.replace(/^\||\|\s*$/g, "").split("|").map((c) => c.trim());
      const head = cells(line);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && lines[i].startsWith("|")) rows.push(cells(lines[i++]));
      out.push({ t: "table", head, rows });
      continue;
    }
    const para: string[] = [line];
    i++;
    while (i < lines.length && lines[i].trim() && !isSpecial(lines[i])) para.push(lines[i++]);
    out.push({ t: "p", text: para.join(" ") });
  }
  return out;
}

const INLINE = /(`[^`]+`)|(\*\*[^*]+\*\*)|(__[^_]+__)|(\*[^*\s][^*]*\*)|(\[[^\]]+\]\([^)\s]+\))/g;

export function renderInline(text: string, onLink?: (url: string) => void): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  let k = 0;
  for (const m of text.matchAll(INLINE)) {
    const idx = m.index ?? 0;
    if (idx > last) out.push(text.slice(last, idx));
    const tok = m[0];
    if (m[1]) out.push(<code key={k++} className="rounded bg-panel-2 px-1 py-px font-mono text-[0.92em]">{tok.slice(1, -1)}</code>);
    else if (m[2] || m[3]) out.push(<strong key={k++}>{tok.slice(2, -2)}</strong>);
    else if (m[4]) out.push(<em key={k++}>{tok.slice(1, -1)}</em>);
    else if (m[5]) {
      const [, label, url] = tok.match(/^\[([^\]]+)\]\(([^)\s]+)\)$/) ?? [];
      const safe = /^https?:\/\//i.test(url ?? "");
      out.push(
        safe && onLink ? (
          <button key={k++} className="text-accent underline" onClick={() => onLink(url)}>
            {label}
          </button>
        ) : (
          <span key={k++} className="text-accent">
            {label}
          </span>
        ),
      );
    }
    last = idx + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

export function Markdown({
  text,
  code,
  onLink,
  className = "",
}: {
  text: string;
  code?: CodeRenderer;
  onLink?: (url: string) => void;
  className?: string;
}) {
  const blocks = parseBlocks(text);
  const inl = (t: string) => renderInline(t, onLink);
  return (
    <div className={`space-y-2 break-words text-[13px] leading-relaxed ${className}`}>
      {blocks.map((b, i) => {
        switch (b.t) {
          case "h": {
            const size = b.level === 1 ? "text-[17px]" : b.level === 2 ? "text-[15px]" : "text-[13.5px]";
            return (
              <div key={i} className={`${size} font-semibold`} role="heading" aria-level={b.level}>
                {inl(b.text)}
              </div>
            );
          }
          case "p":
            return <p key={i}>{inl(b.text)}</p>;
          case "ul":
          case "ol": {
            const L = b.t === "ul" ? "ul" : "ol";
            return (
              <L key={i} className={`${b.t === "ul" ? "list-disc" : "list-decimal"} space-y-0.5 pl-5`}>
                {b.items.map((it, j) => (
                  <li key={j}>{inl(it)}</li>
                ))}
              </L>
            );
          }
          case "quote":
            return (
              <blockquote key={i} className="border-l-2 border-line pl-3 text-muted">
                {inl(b.text)}
              </blockquote>
            );
          case "hr":
            return <hr key={i} className="border-line" />;
          case "table":
            return (
              <div key={i} className="overflow-x-auto">
                <table className="text-[12px]">
                  <thead>
                    <tr>
                      {b.head.map((h, j) => (
                        <th key={j} className="border-b border-line px-2 py-1 text-left font-semibold">
                          {inl(h)}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {b.rows.map((r, j) => (
                      <tr key={j}>
                        {r.map((c, x) => (
                          <td key={x} className="border-b border-line/60 px-2 py-1 font-mono">
                            {inl(c)}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            );
          case "code":
            return code ? (
              <Fragment key={i}>{code(b.code, b.lang, i)}</Fragment>
            ) : (
              <pre key={i} className="overflow-x-auto rounded-md bg-panel-2 p-2 font-mono text-[12px] select-text">
                {b.code}
              </pre>
            );
        }
      })}
    </div>
  );
}
