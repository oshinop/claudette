import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { modelRequests, permissions, project, scenario, start } from "./harness";

describe("tools and permissions", () => {
  test("the Cron tools are offered: they are session-scoped, and a kept task lives in the project", async () => {
    const dir = project();
    const id = await scenario([{ text: "ok" }]);
    const run = Bun.spawnSync(["claudette", "-p", `[[scenario:${id}]] hi`], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    expect(run.exitCode, run.stderr.toString()).toBe(0);
    const [first] = await modelRequests(id);
    const request = JSON.stringify(first);
    for (const tool of ["CronCreate", "CronList", "CronDelete"]) expect(request).toContain(tool);
  });

  test("an 'always allow' the host gives for this session covers the tool's paths an ask rule does not", async () => {
    const settings = { permissions: { ask: ["Edit(src/**)"] } };
    const dir = project({ ".claude/settings.json": JSON.stringify(settings), "src/.keep": "" });
    const id = await scenario([
      { tool: "Write", input: { file_path: join(dir, "first.txt"), content: "1" } },
      { tool: "Write", input: { file_path: join(dir, "second.txt"), content: "2" } },
      { tool: "Write", input: { file_path: join(dir, "src", "asked.ts"), content: "3" } },
      { text: "done" },
    ]);
    const perms = permissions((ask) => ({
      behavior: "allow",
      updatedInput: ask.input,
      updatedPermissions: [{ type: "addRules", rules: [{ toolName: "Write" }], behavior: "allow", destination: "session" }],
    }));
    const session = start(dir, { settingSources: ["project"], canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] write`);
    await session.close();
    // The first write asks; the allow then covers the second; the ask rule still holds for src/.
    expect(perms.asked.map((a) => a.input.file_path)).toEqual([join(dir, "first.txt"), join(dir, "src", "asked.ts")]);
  });
});
