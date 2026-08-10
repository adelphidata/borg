import { describe, expect, it } from "vitest";
import type { telemetry as telemetryPb } from "@borg/client";
import { accumulate } from "./useUsage";
import { formatCost } from "../../lib/format";

function costEvent(over: Partial<telemetryPb.CostEvent> = {}): telemetryPb.CostEvent {
  return {
    requestId: "r",
    sessionId: "s",
    provider: "anthropic",
    model: "claude-sonnet-5",
    costUsd: "0.01",
    costUsdPresent: true,
    costSource: "provider",
    promptTokens: 100,
    completionTokens: 20,
    timestampUnixMs: 0,
    kind: "chat",
    ...over,
  };
}

describe("accumulate", () => {
  it("totals cost and tokens across calls to the same model", () => {
    const spend = accumulate([costEvent(), costEvent({ costUsd: "0.02" })]);
    expect(spend.events).toBe(2);
    expect(spend.costUsd).toBeCloseTo(0.03);
    expect(spend.byModel).toHaveLength(1);
    expect(spend.byModel[0]).toMatchObject({ tokens: 240, calls: 2 });
  });

  it("keeps providers apart even when the model name matches", () => {
    const spend = accumulate([
      costEvent({ provider: "openai", model: "shared" }),
      costEvent({ provider: "groq", model: "shared" }),
    ]);
    expect(spend.byModel).toHaveLength(2);
  });

  it("counts tokens but no cost when the provider reported none", () => {
    const spend = accumulate([costEvent({ costUsdPresent: false, costUsd: "" })]);
    expect(spend.costUsd).toBe(0);
    expect(spend.byModel[0]!.tokens).toBe(120);
  });

  it("ignores an unparseable cost rather than poisoning the total with NaN", () => {
    const spend = accumulate([costEvent({ costUsd: "not-a-number" }), costEvent()]);
    expect(spend.costUsd).toBeCloseTo(0.01);
  });

  it("puts the most expensive model first", () => {
    const spend = accumulate([
      costEvent({ model: "cheap", costUsd: "0.001" }),
      costEvent({ model: "dear", costUsd: "1.50" }),
    ]);
    expect(spend.byModel.map((m) => m.model)).toEqual(["dear", "cheap"]);
  });

  it("reports nothing for no events", () => {
    expect(accumulate([])).toEqual({ events: 0, costUsd: 0, byModel: [] });
  });
});

describe("formatCost", () => {
  it("shows extra precision below a cent, so real spend never reads as zero", () => {
    expect(formatCost(0.0023)).toBe("$0.0023");
  });

  it("uses ordinary currency formatting at and above a cent", () => {
    expect(formatCost(1.5)).toBe("$1.50");
  });

  it("shows a plain zero when there really was no spend", () => {
    expect(formatCost(0)).toBe("$0.00");
  });
});
