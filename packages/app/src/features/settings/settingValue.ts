/**
 * How a raw registry value reads on screen, and how an edit goes back.
 *
 * The daemon stores every setting as a string, so `120000`, a JSON list of
 * paths, and a model id all arrive alike. Shown as they are, a timeout reads as
 * a phone number and a list of eleven paths runs off the edge of a one-line
 * field. Sorting the value into a shape lets each one be shown the way people
 * read it, while what is saved stays exactly what the daemon expects.
 */

export type ValueShape =
  | { kind: "json"; parsed: unknown[] | Record<string, unknown> }
  | { kind: "integer" }
  | { kind: "text" };

/**
 * Numbers that are names, not amounts. A port or a process id is read digit
 * by digit, and `7,842` would be a port nobody recognises.
 */
const IDENTIFIER_KEY = /(^|[._])(port|pid|uid|gid|year|version)s?$/;

const INTEGER = /^-?\d+$/;

/** Sorts a stored value. Only a JSON array or object counts as structured. */
export function valueShape(key: string, value: string): ValueShape {
  const trimmed = value.trim();
  if (trimmed.startsWith("[") || trimmed.startsWith("{")) {
    try {
      const parsed: unknown = JSON.parse(trimmed);
      if (parsed !== null && typeof parsed === "object") {
        return { kind: "json", parsed: parsed as unknown[] | Record<string, unknown> };
      }
    } catch {
      // Not JSON after all — a value that only starts with a bracket is text.
    }
  }
  if (INTEGER.test(trimmed) && !IDENTIFIER_KEY.test(key)) return { kind: "integer" };
  return { kind: "text" };
}

/** `1000000` → `1,000,000`. Anything that is not a plain integer passes through. */
export function groupDigits(value: string): string {
  const trimmed = value.trim();
  if (!INTEGER.test(trimmed)) return value;
  const negative = trimmed.startsWith("-");
  const digits = negative ? trimmed.slice(1) : trimmed;
  const grouped = digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return negative ? `-${grouped}` : grouped;
}

/**
 * Undoes {@link groupDigits} on what the user typed. Only a value made of
 * digits and commas is touched, so a comma that is part of real text stays.
 */
export function ungroupDigits(value: string): string {
  const trimmed = value.trim();
  return /^-?[\d,]+$/.test(trimmed) && /\d/.test(trimmed) ? trimmed.replaceAll(",", "") : value;
}

/** Two-space JSON for the editor. */
export function prettyJson(parsed: unknown): string {
  return JSON.stringify(parsed, null, 2);
}

/**
 * Turns an edited JSON draft back into the compact form the daemon stores, or
 * says why it cannot. Saving a half-typed bracket would hand the daemon a
 * string it rejects with a parser error the user never needed to see.
 */
export function compactJson(draft: string): { value: string } | { error: string } {
  try {
    return { value: JSON.stringify(JSON.parse(draft)) };
  } catch (e) {
    const reason = e instanceof Error ? e.message : String(e);
    return { error: `This is not valid JSON yet. ${reason}` };
  }
}

/** The short form a settings row shows on the right. */
export function summarizeValue(key: string, value: string): string {
  if (value === "") return "—";
  const shape = valueShape(key, value);
  if (shape.kind === "integer") return groupDigits(value);
  if (shape.kind === "json") {
    if (Array.isArray(shape.parsed)) {
      const n = shape.parsed.length;
      return n === 0 ? "None" : `${n} ${n === 1 ? "item" : "items"}`;
    }
    const n = Object.keys(shape.parsed).length;
    return n === 0 ? "None" : `${n} ${n === 1 ? "entry" : "entries"}`;
  }
  return value;
}
