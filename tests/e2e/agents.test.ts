import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { modelRequests, onRemote, project, scenario, start } from "./harness";

const MOCK = process.env.MOCK_API ?? "http://mockapi:8080";

function runP(dir: string, prompt: string, env: Record<string, string> = {}) {
  const proc = Bun.spawnSync(["claudette", "-p", prompt, "--output-format", "json"], { cwd: dir, env: { ...process.env, ...env }, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  let result = "";
  try {
    result = (JSON.parse(proc.stdout.toString()) as { result?: string }).result ?? "";
  } catch {
  }
  return { code: proc.exitCode, result, out: `${proc.stdout}\n${proc.stderr}` };
}

async function requestsWith(text: string) {
  const all = (await (await fetch(`${MOCK}/_log`)).json()) as { messages: unknown[] }[];
  return all.filter((r) => JSON.stringify(r.messages).includes(text));
}

async function eventually<T>(get: () => Promise<T[]>, timeoutMs = 20_000): Promise<T[]> {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const found = await get();
    if (found.length || Date.now() > end) return found;
    await Bun.sleep(200);
  }
}

async function idleSession(dir: string, env: Record<string, string> = {}) {
  const id = await scenario([{ text: "ready" }]);
  const session = start(dir, { env: { ...process.env, ...env } });
  await session.turn(`[[scenario:${id}]] stand by`);
  return session;
}

describe("sessions on this local machine reach each other locally", () => {
  test("ListAgents shows this local machine's sessions by project; SendMessage starts a turn in the idle one, marked as from another session", async () => {
    const api = project();
    const a = await idleSession(api);
    try {
      const web = project();
      const marker = `PING-${Math.random().toString(36).slice(2)}`;
      const id = await scenario([
        { tool: "ListAgents", input: {} },
        { tool: "SendMessage", input: { to: basename(api), message: `${marker}: the schema changed` } },
        { text: "{{last_tool_result}}" },
      ]);
      const run = runP(web, `[[scenario:${id}]] coordinate`);
      expect(run.result, run.out).toStartWith(`Delivered to ${basename(api)} [`);

      const [first, second] = await modelRequests(id);
      expect(first!.tools).toContain("mcp__cc__ListAgents");
      expect(first!.tools).toContain("mcp__cc__SendMessage");
      expect(first!.tools).not.toContain("ListAgents");
      expect(first!.tools).not.toContain("SendMessage");
      const listing = JSON.stringify(second!.messages.at(-1));
      expect(listing).toContain(`This session is ${basename(web)} [`);
      expect(listing).toContain(`${basename(api)} [`);
      expect(listing).toContain(`~/${basename(api)} · idle`);

      const delivered = await eventually(async () => (await requestsWith(`${marker}: the schema changed`)).filter((r) => JSON.stringify(r.messages).includes("Another Claude session")));
      const text = JSON.stringify(delivered.at(-1)!.messages);
      expect(text).toContain("Another Claude session sent a message");
      expect(text).toContain(`(From session ${basename(web)} [`);
      expect(text).toContain(`working in ~/${basename(web)}. To reply, use SendMessage`);
    } finally {
      await a.close();
    }
  });

  test("a session in another runtime folder, as on another local machine, and sessions run on the remote machine itself are not listed", async () => {
    const mine = project();
    const other = mkdtempSync(join(tmpdir(), "elsewhere-"));
    mkdirSync(other, { recursive: true, mode: 0o700 });
    const near = await idleSession(mine);
    const farDir = project();
    const far = await idleSession(farDir, { XDG_RUNTIME_DIR: other });
    const remoteId = await scenario([{ tool: "Bash", input: { command: "sleep 8" } }, { text: "done" }]);
    const onTheRemote = Bun.spawn(["ssh", "remote", `mkdir -p /tmp/remote-native && cd /tmp/remote-native && claude -p '[[scenario:${remoteId}]] wait' --name remote-native --allowedTools Bash`], { stdout: "ignore", stderr: "ignore" });
    try {
      await Bun.sleep(3000);
      const id = await scenario([{ tool: "ListAgents", input: {} }, { text: "{{last_tool_result}}" }]);
      const run = runP(project(), `[[scenario:${id}]] list`);
      const listed = [...run.result.matchAll(/ · (~\/[^ ]+) · /g)].map((m) => m[1]);
      expect(listed, run.out).toEqual([`~/${basename(mine)}`]);
      expect(run.result).not.toContain(basename(farDir));
      expect(run.result).not.toContain("remote-native");
    } finally {
      await onTheRemote.exited;
      onRemote("rm -rf /tmp/remote-native ~/.claude/projects/-tmp-remote-native");
      await near.close();
      await far.close();
    }
  });

  test("notify_when_idle: an idle session sends one notice back right away", async () => {
    const watched = project();
    const w = await idleSession(watched);
    try {
      const id = await scenario([{ tool: "SendMessage", input: { to: basename(watched), notify_when_idle: true } }, { text: "{{last_tool_result}}" }]);
      const watcher = start(project());
      try {
        const r = await watcher.turn(`[[scenario:${id}]] tell me when it is done`);
        expect((r as { result?: string }).result).toContain(`A notice will arrive when ${basename(watched)} [`);
        const notices = await eventually(() => requestsWith(`(~/${basename(watched)}) is idle`));
        expect(JSON.stringify(notices.at(-1)!.messages)).toContain("Another Claude session sent a message");
      } finally {
        await watcher.close();
      }
    } finally {
      await w.close();
    }
  });
});

describe("what stays off", () => {
  test("background sessions are refused; agent teams and the remote machine's own agent view stay off, even when turned on", () => {
    const dir = project();
    for (const args of [["agents"], ["attach", "abc"], ["stop", "abc"], ["--bg", "do it"]]) {
      const run = Bun.spawnSync(["claudette", ...args], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
      expect(run.exitCode, args.join(" ")).not.toBe(0);
    }
    const probe = "/tmp/probe-agent-settings";
    onRemote(`cat > ${probe} <<'EOF'
#!/bin/sh
node -e '
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { values } = require("node:util").parseArgs({ args: process.argv.slice(1), options: { settings: { type: "string" } }, strict: false, allowPositionals: true });
assert.ok(values.settings);
const settings = JSON.parse(fs.readFileSync(values.settings, "utf8"));
for (const [key, value] of Object.entries({ CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS: "0", CLAUDE_CODE_DISABLE_AGENT_VIEW: "1" })) {
  assert.equal(process.env[key], value, key);
  assert.equal(settings.env?.[key], value, key);
}
assert.equal(settings.disableAgentView, true);
' -- "$@" || exit $?
exec claude "$@"
EOF
chmod 755 ${probe}`);
    try {
      const run = runP(dir, "hi", {
        CLAUDETTE_REMOTE_CLAUDE: probe,
        CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS: "1",
        CLAUDE_CODE_DISABLE_AGENT_VIEW: "0",
      });
      expect(run.code, run.out).toBe(0);
    } finally {
      onRemote(`rm -f ${probe}`);
    }
    expect(onRemote("ls -d /tmp/claudette-*.inbox 2>/dev/null | wc -l")).toBe("0");
  });
});
