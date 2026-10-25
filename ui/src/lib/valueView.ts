// Showing one cell's full value: what it looks like (JSON, XML, an image in
// a binary column, plain text) and the formatted forms the value viewer shows.

export type ValueKind = "text" | "json" | "xml" | "hex" | "image";

/** Binary cells arrive as lowercase hex (Arrow's display of binary). */
const HEX = /^(?:[0-9a-f]{2})+$/i;

const MAGIC: [string, string][] = [
  ["89504e470d0a1a0a", "image/png"],
  ["ffd8ff", "image/jpeg"],
  ["474946383", "image/gif"],
  ["424d", "image/bmp"],
];

/** MIME type of an image held as hex, or null. */
export function imageType(hex: string): string | null {
  const h = hex.slice(0, 32).toLowerCase();
  for (const [m, t] of MAGIC) if (h.startsWith(m)) return t;
  // RIFF....WEBP
  if (h.startsWith("52494646") && h.slice(16, 24) === "57454250") return "image/webp";
  return null;
}

/** The forms a value can be shown in, best first. */
export function valueKinds(v: string, family: string | undefined): ValueKind[] {
  if (family === "binary" && HEX.test(v)) return imageType(v) ? ["image", "hex", "text"] : ["hex", "text"];
  const t = v.trimStart();
  if ((t.startsWith("{") || t.startsWith("[")) && parseJson(v) !== undefined) return ["json", "text"];
  if (t.startsWith("<") && /<\/?[A-Za-z_][\w:.-]*[\s>/]/.test(t) && t.trimEnd().endsWith(">")) return ["xml", "text"];
  return ["text"];
}

export function parseJson(v: string): unknown {
  try {
    return JSON.parse(v);
  } catch {
    return undefined;
  }
}

/** Indented XML (one element or text run per line). Leaves odd input as is. */
export function prettyXml(xml: string): string {
  const tokens = xml.replace(/>\s+</g, "><").match(/<!\[CDATA\[[\s\S]*?\]\]>|<!--[\s\S]*?-->|<[^>]+>|[^<]+/g);
  if (!tokens) return xml;
  let depth = 0;
  const out: string[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i];
    if (/^<\//.test(tok)) {
      depth = Math.max(0, depth - 1);
      out.push("  ".repeat(depth) + tok);
    } else if (/^<[^!?][^>]*[^/]>$|^<[A-Za-z]>$/.test(tok)) {
      // <a>text</a> stays on one line.
      const text = tokens[i + 1];
      const close = tokens[i + 2];
      if (text && !text.startsWith("<") && close?.startsWith("</")) {
        out.push("  ".repeat(depth) + tok + text.trim() + close);
        i += 2;
      } else {
        out.push("  ".repeat(depth) + tok);
        depth++;
      }
    } else {
      out.push("  ".repeat(depth) + tok.trim());
    }
  }
  return out.filter((l) => l.trim()).join("\n");
}

/** Hex dump: offset, 16 bytes in hex, and their printable ASCII. */
export function hexDump(hex: string, maxBytes = 64 * 1024): string {
  const bytes = Math.min(hex.length / 2, maxBytes);
  const lines: string[] = [];
  for (let off = 0; off < bytes; off += 16) {
    const chunk = hex.slice(off * 2, Math.min(bytes, off + 16) * 2);
    const pairs = chunk.match(/../g) ?? [];
    const ascii = pairs.map((p) => {
      const c = parseInt(p, 16);
      return c >= 32 && c < 127 ? String.fromCharCode(c) : ".";
    });
    lines.push(`${off.toString(16).padStart(8, "0")}  ${pairs.join(" ").padEnd(47)}  ${ascii.join("")}`);
  }
  if (hex.length / 2 > bytes) lines.push(`… ${(hex.length / 2 - bytes).toLocaleString()} more bytes`);
  return lines.join("\n");
}

export function hexToDataUrl(hex: string, mime: string): string {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(hex.substr(i * 2, 2), 16);
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return `data:${mime};base64,${btoa(bin)}`;
}

/** "12 chars · 3 lines" / "2.4 KB binary". */
export function valueSize(v: string, family: string | undefined): string {
  if (family === "binary" && HEX.test(v)) {
    const n = v.length / 2;
    return n >= 1024 ? `${(n / 1024).toFixed(1)} KB binary` : `${n} bytes`;
  }
  const lines = v.split("\n").length;
  return `${v.length.toLocaleString()} chars${lines > 1 ? ` · ${lines.toLocaleString()} lines` : ""}`;
}
