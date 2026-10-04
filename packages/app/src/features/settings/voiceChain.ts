/**
 * Editing the first row of a provider chain (`tts.models`, `audio.models`).
 *
 * Both settings are an ordered JSON list: the first row answers, the rest are
 * fallbacks. Settings edits only the first row and leaves every other row —
 * and every field it does not show — exactly as it found them, so a power
 * user's hand-built chain survives a trip through the page.
 */

/** One chain row. Only the fields the page edits are named. */
export interface ChainRow {
  provider: string;
  model?: string;
  voice?: string;
  command?: string;
  [field: string]: unknown;
}

/** The rows in `json`; a missing or malformed value reads as no chain. */
export function parseChain(json: string | undefined): ChainRow[] {
  if (!json) return [];
  try {
    const parsed: unknown = JSON.parse(json);
    return Array.isArray(parsed)
      ? parsed.filter(
          (r): r is ChainRow =>
            typeof r === "object" && r !== null && typeof (r as ChainRow).provider === "string",
        )
      : [];
  } catch {
    return [];
  }
}

/** The row that answers first, if any. */
export function primaryRow(json: string | undefined): ChainRow | undefined {
  return parseChain(json)[0];
}

/** Drop empty strings so a cleared field falls back to the provider default. */
function compact(row: ChainRow): ChainRow {
  return Object.fromEntries(
    Object.entries(row).filter(([, v]) => v !== "" && v !== undefined),
  ) as ChainRow;
}

/** `json` with its first row's fields updated (`""` clears a field). */
export function withPrimaryFields(json: string | undefined, fields: Partial<ChainRow>): string {
  const rows = parseChain(json);
  const [first, ...rest] = rows;
  if (!first) return JSON.stringify(rows);
  return JSON.stringify([compact({ ...first, ...fields }), ...rest]);
}

/**
 * `json` with `provider` answering first. Switching starts a fresh row —
 * a voice id or model belongs to one provider and would fail on another —
 * and a fallback row for the same provider moves up rather than repeating.
 */
export function withPrimaryProvider(json: string | undefined, provider: string): string {
  const rows = parseChain(json);
  if (rows[0]?.provider === provider) return JSON.stringify(rows);
  const existing = rows.slice(1).find((r) => r.provider === provider);
  const rest = rows.slice(1).filter((r) => r !== existing);
  return JSON.stringify([existing ?? { provider }, ...rest]);
}
