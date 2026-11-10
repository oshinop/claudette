/**
 * The real Paseo daemon, unmodified, with a Claude provider whose `command` is claudette.
 * Runs in the `paseo` container (scripts/test.sh).
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { modelRequests, onRemote, project, remoteDir, scenario } from "../e2e/harness";

const HOME = mkdtempSync(join(homedir(), "paseo-"));

/** The daemon inherits this environment, which has no CLAUDETTE_HOST: the provider's `env` must supply it. */
const { CLAUDETTE_HOST: _host, ...daemonEnv } = process.env;

function paseo(...args: string[]) {
  const proc = Bun.spawnSync(["paseo", "--home", HOME, ...args], { env: daemonEnv, stdout: "pipe", stderr: "pipe", timeout: 120_000 });
  return { code: proc.exitCode ?? -1, out: proc.stdout.toString(), err: proc.stderr.toString() };
}

beforeAll(() => {
  mkdirSync(HOME, { recursive: true });
  // The provider entry from the README.
  const provider = { extends: "claude", label: "Claude on Mac remote", command: ["/usr/local/bin/claudette"], env: { CLAUDETTE_HOST: "remote" } };
  writeFileSync(join(HOME, "config.json"), JSON.stringify({ agents: { providers: { "claude-remote": provider } } }));
  const started = paseo("start");
  if (started.code !== 0) throw new Error(`paseo start failed:\n${started.out}${started.err}`);
});

afterAll(() => {
  Bun.spawnSync(["paseo", "--home", HOME, "daemon", "stop"], { stdout: "ignore", stderr: "ignore" });
});

describe("Paseo with claudette as the Claude command", () => {
  const dir = project({ "notes.txt": "paseo-local-file\n" });
  let agentId = "";

  // The second test continues the agent this one starts.
  test("an agent's tools run on the local machine, and Paseo shows the reply", async () => {
    const status = paseo("status", "--json");
    expect(JSON.parse(status.out).providers).toContainEqual({ provider: "claude-remote", available: true, error: null });

    const id = await scenario([
      { tool: "Bash", input: { command: "hostname > host.txt && cat notes.txt" } },
      { tool: "Read", input: { file_path: join(dir, "host.txt") } },
      { text: "PASEO saw: {{last_tool_result}}" },
    ]);
    const run = paseo("run", "--provider", "claude-remote", "--mode", "bypassPermissions", "--cwd", dir, "--json", `[[scenario:${id}]] do local things`);
    expect(run.code, run.out + run.err).toBe(0);
    const result = JSON.parse(run.out);
    expect(result.status).toBe("completed");
    agentId = result.agentId;

    expect(readFileSync(join(dir, "host.txt"), "utf8")).toBe("local\n");
    expect(onRemote(`ls -A ${remoteDir(dir)}`)).toBe("");
    expect(paseo("logs", agentId).out).toContain("PASEO saw:      1\tlocal");
    const [first] = await modelRequests(id);
    expect(first!.tools.filter((t) => t.startsWith("mcp__cc__")).length).toBeGreaterThan(0);
  });

  test("after a daemon restart the agent continues the same Claude conversation", async () => {
    expect(paseo("restart").code).toBe(0);
    const id = await scenario([{ text: "second turn done" }]);
    const send = paseo("send", agentId, `[[scenario:${id}]] continue`);
    expect(send.code, send.out + send.err).toBe(0);
    expect(paseo("wait", agentId).code).toBe(0);
    expect(paseo("logs", agentId).out).toContain("second turn done");
    const [request] = await modelRequests(id);
    expect(JSON.stringify(request!.messages)).toContain("do local things");
  });
});
