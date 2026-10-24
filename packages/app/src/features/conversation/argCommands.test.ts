import { describe, expect, it } from "vitest";
import { admin, BorgError, createBorgClient, GrpcStatus } from "@borg/client";
import { fakeDaemon } from "../../test/harness";
import { parseArgCommand, parseIntervalSecs, runArgCommand } from "./argCommands";

const Op = admin.SessionHeartbeatMutation_Op;

describe("parseArgCommand", () => {
  it.each([
    ["/goal", { kind: "goal-show" }],
    ["/goal Ship the release notes", { kind: "goal-set", objective: "Ship the release notes" }],
    ["/goal edit Ship v2", { kind: "goal-set", objective: "Ship v2" }],
    ["/goal pause", { kind: "goal-status", status: "paused" }],
    ["/goal resume", { kind: "goal-status", status: "active" }],
    ["/goal clear", { kind: "goal-clear" }],
    ["/goal gate add cargo test", { kind: "goal-terminal-only" }],
    ["/goal --budget 5000 Ship it", { kind: "goal-terminal-only" }],
    ["/heartbeat", { kind: "heartbeat", op: Op.OP_STATUS, prompt: "", intervalSecs: 0 }],
    ["/hb pause", { kind: "heartbeat", op: Op.OP_PAUSE, prompt: "", intervalSecs: 0 }],
    [
      "/heartbeat every 15m Check  the deploy",
      { kind: "heartbeat", op: Op.OP_SET, prompt: "Check  the deploy", intervalSecs: 900 },
    ],
    ["/mode plan", { kind: "mode", mode: "plan" }],
    ["/mode Execute", { kind: "mode", mode: "execute" }],
  ])("reads %j", (text, want) => {
    expect(parseArgCommand(text)).toEqual(want);
  });

  it.each([
    "/goal edit",
    "/heartbeat every soon Check",
    "/heartbeat every 10m",
    "/hb nonsense",
    "/mode",
    "/mode fast",
  ])("answers %j with usage instead of guessing", (text) => {
    expect(parseArgCommand(text)?.kind).toBe("usage");
  });

  it.each(["/goals are nice", "what is my /goal", "/tmp/goal", "hello"])(
    "leaves %j to be sent as a message",
    (text) => {
      expect(parseArgCommand(text)).toBeUndefined();
    },
  );
});

describe("parseIntervalSecs", () => {
  it.each([
    ["90s", 90],
    ["10m", 600],
    ["2h", 7200],
    ["1d", 86_400],
    ["45", 45],
    ["0m", undefined],
    ["soon", undefined],
    ["5w", undefined],
  ])("%s → %s", (raw, want) => {
    expect(parseIntervalSecs(raw)).toBe(want);
  });
});

describe("runArgCommand", () => {
  it("switches the mode through the setting the mode switch uses", async () => {
    const daemon = fakeDaemon({ SetSetting: {} });
    const borg = createBorgClient(daemon.transport);
    expect(await runArgCommand(borg, "c", { kind: "mode", mode: "execute" })).toBe(
      "Execute mode is on. Borg acts.",
    );
    expect(daemon.calls[0]).toMatchObject({
      method: "SetSetting",
      request: { key: "conversation.collaboration_mode", value: "execute" },
    });

    const saved: string[] = [];
    const save = async (key: string, value: string) => (saved.push(`${key}=${value}`), true);
    expect(await runArgCommand(borg, "c", { kind: "mode", mode: "plan" }, save)).toContain(
      "Plan mode is on.",
    );
    expect(saved).toEqual(["conversation.collaboration_mode=plan"]);
    expect(await runArgCommand(borg, "c", { kind: "mode", mode: "plan" }, async () => false)).toBe(
      "That did not work: the mode did not change.",
    );
  });

  it("sets the goal on the session it was given, never the daemon's latest", async () => {
    const daemon = fakeDaemon({
      SetGoal: { present: true, objective: "Ship it", status: "active" },
    });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "conv-7", {
      kind: "goal-set",
      objective: "Ship it",
    });
    expect(daemon.calls[0]).toMatchObject({
      method: "SetGoal",
      request: { sessionId: "conv-7", op: admin.GoalMutation_Op.OP_REPLACE, objective: "Ship it" },
    });
    expect(reply).toContain("Goal set: Ship it. Borg");
  });

  it("does not double the stop after a goal that has one", async () => {
    const daemon = fakeDaemon({
      SetGoal: { present: true, objective: "Ship it.", status: "active" },
    });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "goal-set",
      objective: "Ship it.",
    });
    expect(reply).toContain("Goal set: Ship it. Borg");
  });

  it("says when there is no goal to pause", async () => {
    const daemon = fakeDaemon({ GetGoal: { present: false } });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "goal-status",
      status: "paused",
    });
    expect(reply).toBe("There is no goal to pause.");
    expect(daemon.calls.map((c) => c.method)).toEqual(["GetGoal"]);
  });

  it("pauses a goal that is set", async () => {
    const daemon = fakeDaemon({
      GetGoal: { present: true, objective: "Ship it", status: "active" },
      SetGoal: { present: true, objective: "Ship it", status: "paused" },
    });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "goal-status",
      status: "paused",
    });
    expect(daemon.calls[1]?.request).toMatchObject({ status: "paused" });
    expect(reply).toBe("Goal paused: Ship it");
  });

  it("reads a heartbeat back in whole units", async () => {
    const daemon = fakeDaemon({
      MutateSessionHeartbeat: {
        present: true,
        prompt: "Check CI",
        intervalSecs: 900,
        paused: false,
        secondsUntilDue: 130,
        report: "Heartbeat set.",
      },
    });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "heartbeat",
      op: Op.OP_SET,
      prompt: "Check CI",
      intervalSecs: 900,
    });
    expect(daemon.calls[0]?.request).toMatchObject({ sessionId: "c", intervalSecs: 900 });
    expect(reply).toBe("Heartbeat (every 15 minutes, next in 3 minutes): Check CI");
  });

  it("confirms a clear in words rather than as an empty status", async () => {
    const daemon = fakeDaemon({ MutateSessionHeartbeat: { present: false, report: "cleared" } });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "heartbeat",
      op: Op.OP_CLEAR,
      prompt: "",
      intervalSecs: 0,
    });
    expect(reply).toBe("Heartbeat cleared.");
  });

  it("shows a daemon refusal instead of claiming success", async () => {
    const daemon = fakeDaemon({
      SetGoal: new BorgError(GrpcStatus.Unimplemented, "objective too long"),
    });
    const reply = await runArgCommand(createBorgClient(daemon.transport), "c", {
      kind: "goal-set",
      objective: "x",
    });
    expect(reply).toContain("did not work");
    expect(reply).toContain("objective too long");
  });
});
