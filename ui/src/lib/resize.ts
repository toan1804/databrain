/** Clamp a dragged height (px) to the allowed range. */
export function clampHeight(h: number, min: number, max: number): number {
  return Math.round(Math.min(max, Math.max(min, h)));
}

export const EDITOR_MIN = 38;
export const EDITOR_MAX = 1600;
export const OUTPUT_MIN = 120;
export const OUTPUT_MAX = 2000;
export const OUTPUT_DEFAULT = 300;
