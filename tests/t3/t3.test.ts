/**
 * The real T3 Code server, unmodified, with its built-in Claude provider's Binary path set to
 * claudette. Runs in the `t3` container (scripts/test.sh --t3).
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { Subprocess } from "bun";
import { modelRequests, onRemote, project, remoteDir, scenario } from "../e2e/harness";

const T3_HOME = mkdtempSync(join(homedir(), "t3home-"));
const PORT = "3773";
const env = { ...process.env, T3CODE_HOME: T3_HOME };
let server: Subprocess<"ignore", "pipe", "inherit"> | undefined;

/** Starts `t3 serve` and resolves once it prints that it is ready. */
async function startServer() {
  server = Bun.spawn(["t3", "serve", "--no-browser", "--port", PORT], { env, stdin: "ignore", stdout: "pipe", stderr: "inherit" });
  // One reader for the server's whole life: breaking out of `for await` would cancel the pipe,
  // and the server would die of EPIPE on its next log line.
  const reader = server.stdout.getReader();
  const decoder = new TextDecoder();
  let seen = "";
  while (!seen.includes("T3 Code server is ready.")) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`t3 serve exited:\n${seen}`);
    seen += decoder.decode(value);
  }
  void (async () => {
    while (!(await reader.read()).done);
  })();
}

async function stopServer() {
  server?.kill("SIGTERM");
  await server?.exited;
  server = undefined;
}

function t3(...args: string[]): { code: number; json: { threadId: string; status: string; reply: string; error: unknown } | undefined; out: string } {
  const proc = Bun.spawnSync(["t3", ...args], { env, stdout: "pipe", stderr: "pipe", timeout: 120_000 });
  const out = proc.stdout.toString() + proc.stderr.toString();
  let json;
  try {
    json = JSON.parse(proc.stdout.toString());
  } catch {
    json = undefined;
  }
  return { code: proc.exitCode ?? -1, json, out };
}

beforeAll(async () => {
  mkdirSync(join(T3_HOME, "userdata"), { recursive: true });
  writeFileSync(
    join(T3_HOME, "userdata", "settings.json"),
    JSON.stringify({ providerInstances: { claudeAgent: { driver: "claudeAgent", enabled: true, config: { binaryPath: "/usr/local/bin/claudette" } } } }),
  );
  await startServer();
});

afterAll(stopServer);

describe("T3 Code with claudette as the Claude binary", () => {
  const dir = project({ "notes.txt": "t3-local-file\n" });
  let threadId = "";

  // The second test continues the thread this one starts.
  test("a thread's tools run on the local machine, and T3 gets the reply", async () => {
    expect(t3("project", "add", dir).code).toBe(0);
    const id = await scenario([
      { tool: "Bash", input: { command: "hostname > host.txt && cat notes.txt" } },
      { tool: "Write", input: { file_path: join(dir, "written.txt"), content: "from-t3" } },
      { tool: "Read", input: { file_path: join(dir, "host.txt") } },
      { text: "T3 saw: {{last_tool_result}}" },
    ]);
    const run = t3("thread", "start", dir, `[[scenario:${id}]] do local things`, "--provider", "claudeAgent", "--model", "claude-opus-5-5", "--wait", "--json");
    expect(run.json, run.out).toBeDefined();
    expect(run.json!.status).toBe("completed");
    expect(run.json!.reply).toBe("T3 saw:      1\tlocal");
    threadId = run.json!.threadId;

    expect(readFileSync(join(dir, "host.txt"), "utf8")).toBe("local\n");
    expect(readFileSync(join(dir, "written.txt"), "utf8")).toBe("from-t3");
    expect(onRemote(`ls -A ${remoteDir(dir)}`)).toBe("");
    const [first] = await modelRequests(id);
    expect(first!.tools.filter((t) => t.startsWith("mcp__cc__")).sort()).toEqual(
      ["Bash", "Edit", "Glob", "Grep", "ListAgents", "NotebookEdit", "Read", "SendMessage", "Write"].map((t) => `mcp__cc__${t}`),
    );
    expect(first!.system).toContain("T3 Code");
  });

  test("after a T3 restart the thread resumes the same Claude conversation", async () => {
    await stopServer();
    await startServer();
    const id = await scenario([{ text: "second turn done" }]);
    const run = t3("thread", "send", threadId, `[[scenario:${id}]] continue`, "--wait", "--json");
    expect(run.json, run.out).toBeDefined();
    expect(run.json!.status).toBe("completed");
    expect(run.json!.reply).toBe("second turn done");
    const [request] = await modelRequests(id);
    // The first turn's prompt and tool results are in the resumed conversation.
    expect(JSON.stringify(request!.messages)).toContain("do local things");
    expect(JSON.stringify(request!.messages)).toContain("t3-local-file");
  });
});
