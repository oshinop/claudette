import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { installRemoteServer, MOCK_API, modelRequests, onRemote, project, projectKey, remoteDir, scenario, start, until } from "./harness";

/** claudette used the way a person (or T3 Code's text generation) uses claude: one -p run. */
function claude(dir: string, args: string[], env: Record<string, string> = {}) {
  const log = join(mkdtempSync(join(tmpdir(), "log-")), "claudette.log");
  const proc = Bun.spawnSync(["claudette", ...args], { cwd: dir, env: { ...process.env, CLAUDETTE_LOG: log, ...env }, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const stdout = proc.stdout.toString();
  let result: { result?: string; is_error?: boolean } = {};
  try {
    result = JSON.parse(stdout);
  } catch {
    // Left empty: the assertions show stdout and stderr.
  }
  const entries = readFileSync(log, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l) as { dir: string; line: string });
  const spawn = entries.find((e) => e.dir === "spawn")?.line ?? "";
  const hooks = entries.filter((e) => e.dir === "hook").map((e) => JSON.parse(e.line));
  return { code: proc.exitCode, result, out: `${stdout}\n${proc.stderr.toString()}`, spawn, hooks };
}

/** Files a run leaves in the remote machine's /tmp, and sockets still bound there, once the holder
 * has had the moment it takes to hear the goodbye. */
const leftoverSockets = async () => {
  const count = () => onRemote("{ ls /tmp/claudette-* 2>/dev/null; grep claudette /proc/net/unix; } | wc -l");
  await until(() => count() === "0", 5_000);
  return count();
};

describe("claudette as claude, with -p", () => {
  test("tools run on the local machine; acceptEdits covers local edits; the run's sockets are cleaned up", async () => {
    const dir = project();
    const id = await scenario([
      { tool: "Write", input: { file_path: join(dir, "p.txt"), content: "from -p" } },
      { tool: "Bash", input: { command: "hostname" } },
      { text: "{{last_tool_result}}" },
    ]);
    const run = claude(dir, ["-p", `[[scenario:${id}]] go`, "--output-format", "json", "--permission-mode", "acceptEdits", "--allowedTools", "Bash"]);

    expect(run.result.result, run.out).toBe("local");
    expect(readFileSync(join(dir, "p.txt"), "utf8")).toBe("from -p");
    const [first] = await modelRequests(id);
    expect(first!.tools.filter((t) => t.startsWith("mcp__cc__")).sort()).toEqual(
      ["Bash", "Edit", "Glob", "Grep", "ListAgents", "NotebookEdit", "Read", "SendMessage", "Write"].map((t) => `mcp__cc__${t}`),
    );
    for (const builtin of ["Read", "Write", "Bash"]) expect(first!.tools).not.toContain(builtin);
    expect(first!.system).toContain(`Working directory: ${dir}`);
    expect(await leftoverSockets()).toBe("0");
    // The hook made the acceptEdits decision claude does not make for MCP tools.
    expect(run.hooks).toContainEqual({ tool: "Write", mode: "acceptEdits", decision: { behavior: "allow" } });
  });

  test("without acceptEdits, a -p run cannot edit: nobody is there to approve", async () => {
    const dir = project();
    const id = await scenario([{ tool: "Write", input: { file_path: join(dir, "no.txt"), content: "x" } }, { text: "error={{last_tool_error}}" }]);
    const run = claude(dir, ["-p", `[[scenario:${id}]] go`, "--output-format", "json"]);
    expect(run.result.result, run.out).toBe("error=true");
    expect(existsSync(join(dir, "no.txt"))).toBe(false);
  });

  test("T3 Code's investigating text generation: its tool list and Bash rules hold for the local tools", async () => {
    const dir = project({ "a.txt": "x" });
    const id = await scenario([
      { tool: "Bash", input: { command: "git log --oneline" } },
      { tool: "Bash", input: { command: "git log -o /tmp/escape" } },
      { text: "denied={{last_tool_error}}" },
    ]);
    const run = claude(dir, [
      "-p", `[[scenario:${id}]] summarize`, "--output-format", "json",
      "--tools", "Read,Grep,Glob,Bash",
      "--allowedTools", "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*)",
      "--disallowedTools", "Bash(git * --output*),Bash(git * -o *)",
      "--disable-slash-commands", "--strict-mcp-config", "--permission-mode", "dontAsk", "--settings", "{}",
    ]);

    expect(run.result.result, run.out).toBe("denied=true");
    const requests = await modelRequests(id);
    expect(requests[0]!.tools.filter((t) => t.startsWith("mcp__cc__")).sort()).toEqual(["mcp__cc__Bash", "mcp__cc__Glob", "mcp__cc__Grep", "mcp__cc__Read"]);
    // The allowed command ran on the local machine and saw its repository.
    expect(JSON.stringify(requests[1]!.messages.at(-1))).toContain(" init");
    expect(run.hooks.map((h) => h.decision?.behavior)).toEqual(["allow", "deny"]);
  });

  test("with no local tools allowed, only the workspace, the sessions folder, the hook, the user configuration's saves and the inbox relay are served", async () => {
    const dir = project();
    const run = claude(dir, ["-p", "title please", "--output-format", "json", "--tools", ""]);
    expect(run.result.result, run.out).toBe("ok");
    // The workspace (its SFTP server and the project's hook callbacks), the sessions folder,
    // claudette's own hook, which also reports whether the session is working or idle, and the
    // socket the Claude Code folder's saves come back on (this machine has no settings file to
    // send). The session's settings, the appended system prompt and the MCP configuration, which
    // holds only the inbox relay, come with the run's first frames, off the remote command line.
    expect(JSON.parse(run.spawn).sockets).toBe(5);
  });

  test("the remote claude sees the project at its folder, and nothing else of the remote machine's home", async () => {
    const dir = project({ "local.txt": "from the local machine\n" });
    onRemote("mkdir -p ~/secret && echo hidden > ~/secret/s.txt");
    // A stand-in for claude on the remote machine that reports what it can reach.
    onRemote(
      `cat > /tmp/probe-claude <<'EOF'
#!/bin/sh
echo "pwd=$(pwd)"
echo "file=$(cat local.txt)"
echo "secret=$(cat ~/secret/s.txt 2>/dev/null || echo denied)"
echo x > written.txt 2>/dev/null && echo "write=yes" || echo "write=no"
mkdir -p .claude && echo '{}' > .claude/probe.json && echo "claude-dir=yes"
touch ~/escaped 2>/dev/null
EOF
chmod 755 /tmp/probe-claude`,
    );
    const run = Bun.spawnSync(["claudette", "-p", "hi"], { cwd: dir, env: { ...process.env, CLAUDETTE_REMOTE_CLAUDE: "/tmp/probe-claude" }, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    const out = run.stdout.toString();

    expect(out, run.stderr.toString()).toContain(`pwd=${onRemote(`cd ${remoteDir(dir)} && pwd`)}`);
    expect(out).toContain("file=from the local machine");
    expect(out).toContain("secret=denied");
    expect(out).toContain("write=no");
    expect(out).toContain("claude-dir=yes");
    expect(existsSync(join(dir, "written.txt"))).toBe(false);
    expect(readFileSync(join(dir, ".claude", "probe.json"), "utf8")).toBe("{}\n");
    expect(onRemote("ls ~/escaped 2>/dev/null | wc -l")).toBe("0");
    expect(onRemote(`ls -A ${remoteDir(dir)}`)).toBe("");
    expect(await leftoverSockets()).toBe("0");
  });

  test("a stdio MCP server from --mcp-config runs on the local machine, and its prompt runs as a slash command", async () => {
    const dir = project();
    const id = await scenario([{ tool: "where", input: {} }, { text: "{{last_tool_result}}" }]);
    const config = { mcpServers: { local: { type: "stdio", command: "bun", args: ["/app/fixtures/stdio-mcp.ts"] } } };
    const mcp = ["--mcp-config", JSON.stringify(config), "--strict-mcp-config"];
    const run = claude(dir, ["-p", `[[scenario:${id}]] where`, "--output-format", "json", ...mcp, "--allowedTools", "mcp__local__where"], { ENABLE_TOOL_SEARCH: "false" });
    expect(run.result.result, run.out).toBe(`stdio-server-on:local cwd:${dir}`);

    const prompt = claude(dir, ["-p", "/mcp__local__local-prompt", "--output-format", "json", ...mcp]);
    expect(prompt.code, prompt.out).toBe(0);
    const all = (await (await fetch(`${MOCK_API}/_log`)).json()) as { messages: unknown[] }[];
    expect(all.some((r) => JSON.stringify(r.messages).includes("prompt-from-local"))).toBe(true);
  });

  test("a session an SDK host (T3 Code) started continues from the terminal with --resume", async () => {
    const dir = project();
    const id = await scenario([{ text: "turn-done" }]);
    const host = start(dir);
    const first = await host.turn(`[[scenario:${id}]] the codeword is quince`);
    await host.close();

    const run = claude(dir, ["-p", "--resume", first.session_id, `[[scenario:${id}]] what was it?`, "--output-format", "json"]);
    expect(run.result.result, run.out).toBe("turn-done");
    expect(JSON.stringify((await modelRequests(id)).at(-1)!.messages)).toContain("quince");
  });

  test("sessions live on the local machine: --continue picks up the last one, and the remote machine's earlier ones move to the local machine", async () => {
    const dir = project();
    // A session the remote machine kept for this project before sessions moved to the local machine.
    const folder = `$(mkdir -p ${remoteDir(dir)} && cd ${remoteDir(dir)} && pwd -P | sed 's/[^a-zA-Z0-9]/-/g')`;
    onRemote(`P=~/.claude/projects/${folder}; mkdir -p "$P" && echo '{"type":"summary"}' > "$P/kept-on-remote.jsonl"`);
    const id = await scenario([{ text: "turn-done" }]);
    const first = claude(dir, ["-p", `[[scenario:${id}]] the fruit is loquat`, "--output-format", "json"]);
    expect(first.result.result, first.out).toBe("turn-done");

    const local = join(homedir(), ".claude", "projects", projectKey(dir));
    expect(readFileSync(join(local, "kept-on-remote.jsonl"), "utf8")).toBe('{"type":"summary"}\n');
    const sessions = readdirSync(local).filter((f) => f.endsWith(".jsonl") && f !== "kept-on-remote.jsonl");
    expect(sessions).toHaveLength(1);
    expect(readFileSync(join(local, sessions[0]!), "utf8")).toContain(`"cwd":"${dir}"`);
    expect(onRemote("find ~/.claude/projects -name '*.jsonl' | wc -l")).toBe("0");

    const next = claude(dir, ["-p", "--continue", `[[scenario:${id}]] which fruit?`, "--output-format", "json"]);
    expect(next.result.result, next.out).toBe("turn-done");
    expect(JSON.stringify((await modelRequests(id)).at(-1)!.messages)).toContain("loquat");
    expect(readdirSync(local).filter((f) => f.endsWith(".jsonl"))).toHaveLength(2);
  });
});

const FIXTURE = "/app/fixtures/stdio-mcp.ts";

/** `claude mcp …`, as a user who aliased claude to claudette types it. */
const mcp = (dir: string, ...args: string[]) => {
  const run = Bun.spawnSync(["claudette", "mcp", ...args], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  return { code: run.exitCode, out: `${run.stdout}${run.stderr}` };
};

describe("MCP servers: all configured on the local machine, each running where it says", () => {
  test("an approved .mcp.json server runs on the local machine; an unapproved one runs nowhere", async () => {
    const servers = { where: { command: "bun", args: [FIXTURE] }, unapproved: { command: "bun", args: [FIXTURE] } };
    const dir = project({ ".mcp.json": JSON.stringify({ mcpServers: servers }), ".claude/settings.local.json": JSON.stringify({ enabledMcpjsonServers: ["where"] }) });
    const id = await scenario([{ tool: "where", input: {} }, { text: "{{last_tool_result}}" }]);
    const run = claude(dir, ["-p", `[[scenario:${id}]] where`, "--output-format", "json", "--allowedTools", "mcp__where__where"], { ENABLE_TOOL_SEARCH: "false" });

    expect(run.result.result, run.out).toBe(`stdio-server-on:local cwd:${dir}`);
    expect(run.out).toContain('not starting the .mcp.json server "unapproved"');
    const [first] = await modelRequests(id);
    expect(first!.tools.filter((t) => t.startsWith("mcp__unapproved__"))).toEqual([]);
    const listing = mcp(dir, "list").out;
    expect(listing).toContain(`where: bun ${FIXTURE}  (runs on this machine; project (.mcp.json))`);
    expect(listing).toContain("unapproved: bun");
    expect(listing).toContain("NOT APPROVED");
  });

  test("a server set to run remotely runs on the remote machine, with its secrets kept off the command line; the remote machine's own servers never start", async () => {
    const server = installRemoteServer();
    // A server in the remote machine's own configuration: claudette must not let it in.
    onRemote(`claude mcp add --scope user remote-own -- node ${server} >/dev/null 2>&1`);
    try {
      const dir = project();
      const token = `tok-${Math.random().toString(36).slice(2)}`;
      const add = mcp(dir, "add", "--remote", "-e", `TOKEN=${token}`, "remote-where", "node", server);
      expect(add.code, add.out).toBe(0);
      expect(add.out).toContain("local scope, runs on remote");

      const id = await scenario([{ tool: "where", input: {} }, { text: "{{last_tool_result}}" }]);
      const run = claude(dir, ["-p", `[[scenario:${id}]] where`, "--output-format", "json", "--allowedTools", "mcp__remote-where__where"], { ENABLE_TOOL_SEARCH: "false" });
      expect(run.result.result, run.out).toBe(`stdio-server-on:remote token:${token}`);
      expect(run.spawn).not.toContain(token);
      const [first] = await modelRequests(id);
      expect(first!.tools.filter((t) => t.startsWith("mcp__"))).toContain("mcp__remote-where__where");
      expect(first!.tools.filter((t) => t.startsWith("mcp__remote-own__"))).toEqual([]);
      expect(await leftoverSockets()).toBe("0");

      expect(mcp(dir, "remove", "remote-where").code).toBe(0);
      expect(mcp(dir, "list").out).not.toContain("remote-where");
    } finally {
      onRemote("claude mcp remove --scope user remote-own >/dev/null 2>&1");
    }
  });

  test("project scope writes the local machine's .mcp.json and approves it here", () => {
    const dir = project();
    const add = mcp(dir, "add", "--scope", "project", "proj-server", "--", "bun", FIXTURE);
    expect(add.code, add.out).toBe(0);
    expect(JSON.parse(readFileSync(join(dir, ".mcp.json"), "utf8")).mcpServers["proj-server"]).toEqual({ command: "bun", args: [FIXTURE] });
    expect(JSON.parse(readFileSync(join(dir, ".claude", "settings.local.json"), "utf8")).enabledMcpjsonServers).toEqual(["proj-server"]);
    expect(mcp(dir, "add", "--scope", "project", "--remote", "x", "y").code).not.toBe(0);
  });

  test("user and local scope land in the local machine's ~/.claude.json, where a local claude keeps them, and the server runs", async () => {
    const claudeJson = join(homedir(), ".claude.json");
    const before = existsSync(claudeJson) ? readFileSync(claudeJson, "utf8") : undefined;
    // An existing ~/.claude.json from Claude Code: its other keys stay.
    writeFileSync(claudeJson, JSON.stringify({ theme: "dark", projects: {} }));
    try {
      const dir = project();
      expect(mcp(dir, "add", "--scope", "user", "user-where", "--", "bun", FIXTURE).code).toBe(0);
      const add = mcp(dir, "add", "local-where", "--", "bun", FIXTURE);
      expect(add.code, add.out).toBe(0);
      const saved = JSON.parse(readFileSync(claudeJson, "utf8"));
      expect(saved.theme).toBe("dark");
      expect(saved.mcpServers["user-where"]).toEqual({ command: "bun", args: [FIXTURE] });
      expect(saved.projects[realpathSync(dir)].mcpServers["local-where"]).toEqual({ command: "bun", args: [FIXTURE] });

      const id = await scenario([{ tool: "where", input: {} }, { text: "{{last_tool_result}}" }]);
      const run = claude(dir, ["-p", `[[scenario:${id}]] where`, "--output-format", "json", "--allowedTools", "mcp__local-where__where"], { ENABLE_TOOL_SEARCH: "false" });
      expect(run.result.result, run.out).toBe(`stdio-server-on:local cwd:${dir}`);

      expect(mcp(dir, "remove", "local-where").code).toBe(0);
      expect(mcp(dir, "remove", "user-where").code).toBe(0);
      expect(mcp(dir, "list").out).toContain("No MCP servers");
      // A ~/.claude.json that does not parse is left alone rather than replaced.
      writeFileSync(claudeJson, "{ not json");
      expect(mcp(dir, "add", "--scope", "user", "x", "--", "y").code).not.toBe(0);
      expect(readFileSync(claudeJson, "utf8")).toBe("{ not json");
    } finally {
      if (before === undefined) rmSync(claudeJson, { force: true });
      else writeFileSync(claudeJson, before);
    }
  });
});

describe("plugins: Claude Code's own installer, working on the local machine's store", () => {
  test("claude plugin installs from a local marketplace into the local machine's ~/.claude, and the next session loads it", async () => {
    const claudeDir = join(homedir(), ".claude");
    const settingsFile = join(claudeDir, "settings.json");
    const before = existsSync(settingsFile) ? readFileSync(settingsFile, "utf8") : undefined;
    mkdirSync(claudeDir, { recursive: true });
    writeFileSync(settingsFile, JSON.stringify({ env: { KEPT: "1" } }, null, 2));
    const market = project({
      ".claude-plugin/marketplace.json": JSON.stringify({ name: "market", owner: { name: "me" }, plugins: [{ name: "lint", source: "./plugins/lint", description: "d" }] }),
      "plugins/lint/.claude-plugin/plugin.json": JSON.stringify({ name: "lint", version: "1.0.0" }),
      "plugins/lint/skills/installed-skill/SKILL.md": "---\nname: installed-skill\ndescription: Installed through claudette\n---\nhi\n",
    });
    const plugin = (...args: string[]) => {
      const run = Bun.spawnSync(["claudette", "plugin", ...args], { cwd: market, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
      return { code: run.exitCode, out: `${run.stdout}${run.stderr}` };
    };
    try {
      const added = plugin("marketplace", "add", "./");
      expect(added.code, added.out).toBe(0);
      const installed = plugin("install", "lint@market");
      expect(installed.code, installed.out).toBe(0);

      // The store reads as a local claude would have left it: this machine's paths, its own keys kept.
      const registry = JSON.parse(readFileSync(join(claudeDir, "plugins/installed_plugins.json"), "utf8"));
      const installPath = registry.plugins["lint@market"][0].installPath;
      expect(installPath).toBe(join(claudeDir, "plugins/cache/market/lint/1.0.0"));
      expect(existsSync(join(installPath, "skills/installed-skill/SKILL.md"))).toBe(true);
      const known = JSON.parse(readFileSync(join(claudeDir, "plugins/known_marketplaces.json"), "utf8"));
      expect(known.market.source.path).toBe(market);
      const settings = JSON.parse(readFileSync(settingsFile, "utf8"));
      expect(settings.enabledPlugins).toEqual({ "lint@market": true });
      expect(settings.extraKnownMarketplaces.market.source.path).toBe(market);
      expect(settings.env).toEqual({ KEPT: "1" });
      expect(plugin("list").out).toContain("lint@market");
      // Nothing of it lands on the remote machine.
      expect(onRemote("ls ~/.claude/plugins/cache 2>/dev/null; ls -d /tmp/claudette-*.store 2>/dev/null")).toBe("");

      const dir = project();
      const id = await scenario([{ text: "done" }]);
      const run = claude(dir, ["-p", `[[scenario:${id}]] hi`, "--output-format", "json"]);
      expect(run.result.result, run.out).toBe("done");
      expect(JSON.stringify((await modelRequests(id))[0])).toContain("installed-skill");
    } finally {
      rmSync(join(claudeDir, "plugins"), { recursive: true, force: true });
      if (before === undefined) rmSync(settingsFile, { force: true });
      else writeFileSync(settingsFile, before);
    }
  });
});
