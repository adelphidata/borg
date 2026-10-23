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
 * Numbers that are names, not amounts. A port, a process id or an account id
 * is read digit by digit, and `7,842` would be a port nobody recognises.
 */
const IDENTIFIER_KEY = /(^|[._])(port|pid|uid|gid|id|year|version)s?$/;

const INTEGER = /^-?\d+$/;

/** Digits in groups of three, the way {@link groupDigits} writes them. */
const GROUPED = /^-?\d{1,3}(?:,\d{3})+$/;

/**
 * Sorts a stored value. Only a JSON array or object counts as structured.
 *
 * `kind` is the daemon's own type for a curated key, and empty for the rest.
 * When it says text, a run of digits is a name (a Vertex project number), not
 * a count, and keeps its digits ungrouped.
 */
export function valueShape(key: string, value: string, kind = ""): ValueShape {
  const trimmed = value.trim();
  if (trimmed.startsWith("[") || trimmed.startsWith("{")) {
    try {
      const parsed: unknown = JSON.parse(trimmed);
      if (containerOf(parsed) !== undefined) {
        return { kind: "json", parsed: parsed as unknown[] | Record<string, unknown> };
      }
    } catch {
      // Not JSON after all — a value that only starts with a bracket is text.
    }
  }
  const counted = kind === "" || kind === "uint";
  if (counted && INTEGER.test(trimmed) && !IDENTIFIER_KEY.test(key)) return { kind: "integer" };
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
 * Undoes {@link groupDigits} on what the user typed. Only commas between
 * groups of three are dropped: `0,5` is a decimal comma, and reading it as
 * `05` would save ten times what was meant instead of letting the daemon
 * refuse it. A comma that is part of real text stays too.
 */
export function ungroupDigits(value: string): string {
  const trimmed = value.trim();
  if (GROUPED.test(trimmed)) return trimmed.replaceAll(",", "");
  return INTEGER.test(trimmed) ? trimmed : value;
}

/** Two-space JSON for the editor. */
export function prettyJson(parsed: unknown): string {
  return JSON.stringify(parsed, null, 2);
}

/** Whether a parsed value is a JSON list, a JSON object, or neither. */
export function containerOf(parsed: unknown): "list" | "object" | undefined {
  if (Array.isArray(parsed)) return "list";
  return parsed !== null && typeof parsed === "object" ? "object" : undefined;
}

/**
 * Turns an edited JSON draft back into the compact form the daemon stores, or
 * says why it cannot. Saving a half-typed bracket would hand the daemon a
 * string it rejects with a parser error the user never needed to see, and so
 * would a list saved as an object or as a bare number.
 */
export function compactJson(
  draft: string,
  container?: "list" | "object",
): { value: string } | { error: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(draft);
  } catch (e) {
    const reason = e instanceof Error ? e.message : String(e);
    return { error: `This is not valid JSON yet. ${reason}` };
  }
  if (container !== undefined && containerOf(parsed) !== container) {
    return {
      error:
        container === "list"
          ? "This setting is a list. Keep the square brackets around it."
          : "This setting is an object. Keep the curly braces around it.",
    };
  }
  return { value: JSON.stringify(parsed) };
}

/** The short form a settings row shows on the right. */
export function summarizeValue(key: string, value: string, kind = ""): string {
  if (value === "") return "—";
  const shape = valueShape(key, value, kind);
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
