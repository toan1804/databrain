// Character-level differences between two text values, with the characters
// people can't see (spaces, tabs, no-break and zero-width spaces, a BOM,
// combining accents, a bad decoding's U+FFFD) made visible.

export interface TextChar {
  /** The character as it is in the value (one code point). */
  ch: string;
  /** What to draw for it when hidden characters are shown. */
  shown: string;
  /** U+00A0 NO-BREAK SPACE, for the tooltip. */
  name: string;
  /** space: a kind of space; hidden: not visible at all; other: a non-ASCII letter or sign. */
  kind: "plain" | "space" | "hidden" | "other";
  /** Not in the other value at this place. */
  changed: boolean;
}

const hex = (cp: number) => `U+${cp.toString(16).toUpperCase().padStart(4, "0")}`;

const NAMED: Record<number, [shown: string, name: string, kind: TextChar["kind"]]> = {
  0x09: ["→", "TAB", "space"],
  0x0a: ["↵", "LINE FEED (new line)", "space"],
  0x0d: ["␍", "CARRIAGE RETURN", "space"],
  0x20: ["·", "SPACE", "space"],
  0xa0: ["⍽", "NO-BREAK SPACE", "space"],
  0xad: ["SHY", "SOFT HYPHEN", "hidden"],
  0x2007: ["⍽", "FIGURE SPACE", "space"],
  0x202f: ["⍽", "NARROW NO-BREAK SPACE", "space"],
  0x3000: ["⍽", "IDEOGRAPHIC SPACE", "space"],
  0x200b: ["ZWSP", "ZERO WIDTH SPACE", "hidden"],
  0x200c: ["ZWNJ", "ZERO WIDTH NON-JOINER", "hidden"],
  0x200d: ["ZWJ", "ZERO WIDTH JOINER", "hidden"],
  0x200e: ["LRM", "LEFT-TO-RIGHT MARK", "hidden"],
  0x200f: ["RLM", "RIGHT-TO-LEFT MARK", "hidden"],
  0x2060: ["WJ", "WORD JOINER", "hidden"],
  0xfeff: ["BOM", "ZERO WIDTH NO-BREAK SPACE (byte order mark)", "hidden"],
  0xfffd: ["�", "REPLACEMENT CHARACTER (text decoded with the wrong encoding)", "other"],
};

/** How one character is shown and named. */
export function describeChar(ch: string): Omit<TextChar, "changed"> {
  const cp = ch.codePointAt(0) ?? 0;
  const named = NAMED[cp];
  if (named) return { ch, shown: named[0], name: `${hex(cp)} ${named[1]}`, kind: named[2] };
  if (cp >= 0x2000 && cp <= 0x200a) return { ch, shown: "·", name: `${hex(cp)} SPACE (typographic)`, kind: "space" };
  if (cp < 0x20 || (cp >= 0x7f && cp <= 0x9f)) return { ch, shown: `\\x${cp.toString(16).padStart(2, "0")}`, name: `${hex(cp)} CONTROL CHARACTER`, kind: "hidden" };
  if ((cp >= 0x300 && cp <= 0x36f) || (cp >= 0x1ab0 && cp <= 0x1aff) || (cp >= 0x20d0 && cp <= 0x20ff)) {
    return { ch, shown: `◌${ch}`, name: `${hex(cp)} COMBINING MARK (an accent written as its own character)`, kind: "other" };
  }
  if (cp > 0x7e) return { ch, shown: ch, name: hex(cp), kind: "other" };
  return { ch, shown: ch, name: hex(cp), kind: "plain" };
}

/** Longest common subsequence up to this many cells; past it, common start and end only. */
const LCS_LIMIT = 250_000;

/** Which characters of `a` and `b` are not in the other (by code point). */
export function charDiff(a: string, b: string): { before: TextChar[]; after: TextChar[] } {
  const x = Array.from(a);
  const y = Array.from(b);
  const inX = new Array<boolean>(x.length).fill(false);
  const inY = new Array<boolean>(y.length).fill(false);
  let s = 0;
  while (s < x.length && s < y.length && x[s] === y[s]) inX[s] = inY[s++] = true;
  let ex = x.length;
  let ey = y.length;
  while (ex > s && ey > s && x[ex - 1] === y[ey - 1]) inX[--ex] = inY[--ey] = true;
  const n = ex - s;
  const m = ey - s;
  if (n > 0 && m > 0 && n * m <= LCS_LIMIT) {
    // Lengths of the common subsequence of the suffixes, then walk it.
    const w = m + 1;
    const L = new Uint32Array((n + 1) * w);
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        L[i * w + j] = x[s + i] === y[s + j] ? L[(i + 1) * w + j + 1] + 1 : Math.max(L[(i + 1) * w + j], L[i * w + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (x[s + i] === y[s + j]) {
        inX[s + i++] = true;
        inY[s + j++] = true;
      } else if (L[(i + 1) * w + j] >= L[i * w + j + 1]) i++;
      else j++;
    }
  }
  return {
    before: x.map((ch, i) => ({ ...describeChar(ch), changed: !inX[i] })),
    after: y.map((ch, i) => ({ ...describeChar(ch), changed: !inY[i] })),
  };
}

/** The text as a reader sees it: one kind of space, no invisible characters, accents composed. */
function asSeen(s: string): string {
  return s
    .normalize("NFC")
    .replace(/[\u00ad\u200b-\u200f\u2060\ufeff]/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

/** The two values differ, but not in a way anyone can see. */
export function differsUnseen(a: string | null | undefined, b: string | null | undefined): boolean {
  return a != null && b != null && a !== b && asSeen(a) === asSeen(b);
}
