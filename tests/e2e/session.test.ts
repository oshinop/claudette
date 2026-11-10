import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { createSdkMcpServer, forkSession, tool } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";
import { startHttpMcp } from "../fixtures/http-mcp";
import { installRemoteServer, modelRequests, onRemote, permissions, project, projectKey, remoteDir, resultText, scenario, start, toolUses, until } from "./harness";

describe("local tools", () => {
  test("Read returns a local file; the model sees local tools only, under the built-in names for the host", async () => {
    const dir = project({ "notes.txt": "local-only content\n" });
    const id = await scenario([{ tool: "Read", input: { file_path: join(dir, "notes.txt") } }, { text: "saw: {{last_tool_result}}" }]);
    const session = start(dir);
    const result = await session.turn(`[[scenario:${id}]] read notes`);
    await session.close();

    expect(resultText(result)).toContain("local-only content");
    expect(toolUses(session.messages)).toEqual([{ name: "Read", input: { file_path: join(dir, "notes.txt") } }]);
    const [first] = await modelRequests(id);
    expect(first!.tools).toContain("mcp__cc__Read");
    for (const builtin of ["Read", "Write", "Edit", "Bash", "Glob", "Grep", "NotebookEdit", "ListAgents", "SendMessage"]) expect(first!.tools).not.toContain(builtin);
    // Built-ins outside the allowlist stay off: they act on the remote machine. Tool search defers some built-ins
    // by name in the messages, so look at the whole request.
    const request = JSON.stringify(first);
    for (const off of ["CronCreate", "Monitor", "EnterWorktree"]) expect(request).not.toMatch(new RegExp(`\\b${off}\\b`));
    expect(request).toContain("WebFetch");
    expect(first!.system).toContain(`Working directory: ${dir}`);
    expect(first!.system).toContain("Is directory a git repo: Yes");
  });

  test("Bash runs on the local machine, keeps its directory between calls, and asks the host as Bash", async () => {
    const dir = project();
    Bun.spawnSync(["mkdir", "sub"], { cwd: dir });
    const id = await scenario([
      { tool: "Bash", input: { command: "cd sub && hostname" } },
      { tool: "Bash", input: { command: "pwd" } },
      { text: "pwd={{last_tool_result}}" },
    ]);
    const perms = permissions();
    const session = start(dir, { canUseTool: perms.canUseTool });
    const result = await session.turn(`[[scenario:${id}]] where am I`);
    await session.close();

    const requests = await modelRequests(id);
    const firstResult = JSON.stringify(requests[1]!.messages.at(-1));
    expect(firstResult).toContain("local");
    expect(resultText(result)).toBe(`pwd=${join(dir, "sub")}`);
    expect(perms.asked.map((a) => a.toolName)).toEqual(["Bash", "Bash"]);
  });

  test("Write, Edit and Read change the local machine's file; the remote folder stays empty", async () => {
    const dir = project();
    const file = join(dir, "new.txt");
    const id = await scenario([
      { tool: "Write", input: { file_path: file, content: "alpha\nbeta\n" } },
      { tool: "Edit", input: { file_path: file, old_string: "beta", new_string: "gamma" } },
      { tool: "Read", input: { file_path: file } },
      { text: "final: {{last_tool_result}}" },
    ]);
    const perms = permissions();
    const session = start(dir, { canUseTool: perms.canUseTool });
    const result = await session.turn(`[[scenario:${id}]] write and edit`);
    await session.close();

    expect(readFileSync(file, "utf8")).toBe("alpha\ngamma\n");
    expect(resultText(result)).toContain("gamma");
    expect(perms.asked.map((a) => a.toolName)).toEqual(["Write", "Edit"]);
    expect(onRemote(`test -e ${remoteDir(dir)}/new.txt && echo present || echo absent`)).toBe("absent");
  });

  test("Glob and Grep search the local machine", async () => {
    const dir = project({ "a.ts": "const needle = 1;\n", "b.md": "no match\n" });
    const id = await scenario([
      { tool: "Glob", input: { pattern: "**/*.ts" } },
      { tool: "Grep", input: { pattern: "needle", output_mode: "content" } },
      { text: "grep: {{last_tool_result}}" },
    ]);
    const session = start(dir);
    const result = await session.turn(`[[scenario:${id}]] search`);
    await session.close();

    const requests = await modelRequests(id);
    expect(JSON.stringify(requests[1]!.messages.at(-1))).toContain(join(dir, "a.ts"));
    expect(resultText(result)).toBe(`grep: ${join(dir, "a.ts")}:1:const needle = 1;`);
  });

  test("an explicit built-in tool list, as T3 Code passes, still leaves the local tools", async () => {
    const dir = project({ "x.ts": "" });
    const id = await scenario([{ tool: "Glob", input: { pattern: "*.ts" } }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, { tools: ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "Task"] });
    const result = await session.turn(`[[scenario:${id}]] list`);
    await session.close();
    expect(resultText(result)).toBe(join(dir, "x.ts"));
  });
});

describe("permissions", () => {
  test("acceptEdits approves edits inside the working directory and asks for one outside it", async () => {
    const dir = project();
    const outside = join(mkdtempSync(join(tmpdir(), "outside-")), "o.txt");
    const id = await scenario([
      { tool: "Write", input: { file_path: join(dir, "inside.txt"), content: "in" } },
      { tool: "Write", input: { file_path: outside, content: "out" } },
      { text: "done" },
    ]);
    const perms = permissions();
    const session = start(dir, { permissionMode: "acceptEdits", canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] edits`);
    await session.close();

    expect(readFileSync(join(dir, "inside.txt"), "utf8")).toBe("in");
    expect(readFileSync(outside, "utf8")).toBe("out");
    expect(perms.asked).toEqual([{ toolName: "Write", input: { file_path: outside, content: "out" }, suggestions: undefined }]);
  });

  test("an edit suggestion offers acceptEdits for this session", async () => {
    const dir = project();
    const id = await scenario([{ tool: "Write", input: { file_path: join(dir, "f.txt"), content: "x" } }, { text: "done" }]);
    const perms = permissions();
    const session = start(dir, { canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] write`);
    await session.close();
    expect(perms.asked[0]!.suggestions).toEqual([{ type: "setMode", mode: "acceptEdits", destination: "session" }]);
  });

  test("an approved command prefix covers later commands, never a compound one", async () => {
    const dir = project();
    const id = await scenario([
      { tool: "Bash", input: { command: "ls -la" } },
      { tool: "Bash", input: { command: "ls -1" } },
      { tool: "Bash", input: { command: "ls && touch compound" } },
      { text: "done" },
    ]);
    const perms = permissions((ask) => ({
      behavior: "allow",
      updatedInput: ask.input,
      ...(Array.isArray(ask.suggestions) ? { updatedPermissions: ask.suggestions } : {}),
    }));
    const session = start(dir, { canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] list`);
    await session.close();

    expect(perms.asked.map((a) => a.input.command)).toEqual(["ls -la", "ls && touch compound"]);
    expect(perms.asked[0]!.suggestions).toEqual([
      { type: "addRules", rules: [{ toolName: "Bash", ruleContent: "ls:*" }], behavior: "allow", destination: "session" },
    ]);
  });

  test("a denied tool does not run, and the model is told", async () => {
    const dir = project();
    const id = await scenario([{ tool: "Bash", input: { command: "touch should-not-exist" } }, { text: "error={{last_tool_error}}" }]);
    const perms = permissions(() => ({ behavior: "deny", message: "no thanks" }));
    const session = start(dir, { canUseTool: perms.canUseTool });
    const result = await session.turn(`[[scenario:${id}]] try`);
    await session.close();
    expect(existsSync(join(dir, "should-not-exist"))).toBe(false);
    expect(resultText(result)).toBe("error=true");
  });

  test("plan mode never runs an edit without the host's approval", async () => {
    const dir = project({ "plan.txt": "original\n" });
    const id = await scenario([{ tool: "Write", input: { file_path: join(dir, "plan.txt"), content: "changed\n" } }, { text: "error={{last_tool_error}}" }]);
    const perms = permissions(() => ({ behavior: "deny", message: "plan mode" }));
    const session = start(dir, { permissionMode: "plan", canUseTool: perms.canUseTool });
    const result = await session.turn(`[[scenario:${id}]] plan`);
    await session.close();
    expect(readFileSync(join(dir, "plan.txt"), "utf8")).toBe("original\n");
    expect(resultText(result)).toBe("error=true");
  });
});

/** Tool search defers MCP tools behind ToolSearch; the scripted model calls them directly instead. */
const noToolSearch = { ...process.env, ENABLE_TOOL_SEARCH: "false" };

describe("MCP servers", () => {
  test("an http server on the local machine's loopback, set with setMcpServers, is reached locally; its token stays here", async () => {
    const dir = project();
    const token = `secret-${Math.random().toString(36).slice(2)}`;
    const http = startHttpMcp(token);
    const log = join(mkdtempSync(join(tmpdir(), "log-")), "claudette.log");
    const id = await scenario([{ tool: "whoami", input: {} }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, { env: { ...noToolSearch, CLAUDETTE_LOG: log }, canUseTool: permissions().canUseTool });
    await session.query.setMcpServers({ t3: { type: "http", url: http.url, headers: { Authorization: `Bearer ${token}` } } });
    const result = await session.turn(`[[scenario:${id}]] who`);
    await session.close();
    http.stop();

    expect(resultText(result)).toBe("http-server-on:local");
    expect(http.authorizations).toContain(`Bearer ${token}`);
    const sentToClaude = readFileSync(log, "utf8")
      .split("\n")
      .filter(Boolean)
      .map((l) => JSON.parse(l))
      .filter((e) => e.dir === "in-rewritten" || e.dir === "spawn")
      .map((e) => e.line)
      .join("\n");
    expect(sentToClaude).toContain('"mcp_set_servers"');
    expect(sentToClaude).not.toContain(token);
  });

  test("a stdio server from the host's config runs on the local machine", async () => {
    const dir = project();
    const id = await scenario([{ tool: "where", input: {} }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, {
      env: noToolSearch,
      canUseTool: permissions().canUseTool,
      mcpServers: { local: { type: "stdio", command: "bun", args: ["/app/fixtures/stdio-mcp.ts"] } },
    });
    const result = await session.turn(`[[scenario:${id}]] where`);
    await session.close();
    expect(resultText(result)).toBe(`stdio-server-on:local cwd:${dir}`);
  });

  test("a local MCP server's resource is listed and read by remote claude", async () => {
    const dir = project();
    const id = await scenario([
      { tool: "ListMcpResourcesTool", input: { server: "local" } },
      { tool: "ReadMcpResourceTool", input: { server: "local", uri: "local://note" } },
      { text: "resource={{last_tool_result}}" },
    ]);
    const session = start(dir, {
      env: noToolSearch,
      mcpServers: { local: { type: "stdio", command: "bun", args: ["/app/fixtures/stdio-mcp.ts"] } },
    });
    const result = await session.turn(`[[scenario:${id}]] read the local resource`);
    await session.close();

    expect(resultText(result)).toContain("resource-from-local");
    const requests = await modelRequests(id);
    expect(requests[0]!.tools).toContain("ListMcpResourcesTool");
    expect(requests[0]!.tools).toContain("ReadMcpResourceTool");
  });

  test("the host's own in-process SDK server still answers through the relay", async () => {
    const dir = project();
    const host = createSdkMcpServer({
      name: "hostsdk",
      tools: [tool("hosttool", "Answers from the host process", { word: z.string() }, async ({ word }) => ({ content: [{ type: "text", text: `host-says:${word}` }] }))],
    });
    const id = await scenario([{ tool: "hosttool", input: { word: "hi" } }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, { env: noToolSearch, canUseTool: permissions().canUseTool, mcpServers: { hostsdk: host } });
    const result = await session.turn(`[[scenario:${id}]] host`);
    await session.close();
    expect(resultText(result)).toBe("host-says:hi");
  });

  test("a server set to run remotely reaches the remote claude in SDK mode, and stays after the host's setMcpServers", async () => {
    const server = installRemoteServer();
    const dir = project();
    const add = Bun.spawnSync(["claudette", "mcp", "add", "--remote", "remote-where", "node", server], { cwd: dir, stdin: "ignore" });
    expect(add.exitCode, add.stderr.toString()).toBe(0);
    const http = startHttpMcp("t");
    const id = await scenario([{ tool: "where", input: {} }, { tool: "whoami", input: {} }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, { env: noToolSearch, canUseTool: permissions().canUseTool });
    await session.query.setMcpServers({ t3: { type: "http", url: http.url, headers: { Authorization: "Bearer t" } } });
    const result = await session.turn(`[[scenario:${id}]] where`);
    await session.close();
    http.stop();
    expect(resultText(result)).toBe("http-server-on:local");
    const requests = await modelRequests(id);
    expect(JSON.stringify(requests[1]!.messages.at(-1))).toContain("stdio-server-on:remote");
  });
});

describe("sessions", () => {
  test("the transcript is written to the local machine through the mounted sessions folder, with local paths, and the session resumes", async () => {
    const dir = project();
    const id = await scenario([{ text: "turn-done" }]);
    const first = start(dir);
    const r1 = await first.turn(`[[scenario:${id}]] remember the word kumquat`);
    await first.close();
    const sessionId = r1.session_id;

    const transcript = join(homedir(), ".claude", "projects", projectKey(dir), `${sessionId}.jsonl`);
    expect(await until(() => existsSync(transcript))).toBe(true);
    const text = readFileSync(transcript, "utf8");
    expect(text).toContain("kumquat");
    expect(text).toContain(`"cwd":"${dir}"`);
    expect(text).not.toContain("/.claudette/ws/");

    const second = start(dir, { resume: sessionId });
    const r2 = await second.turn(`[[scenario:${id}]] and again`);
    await second.close();
    expect(r2.session_id).toBe(sessionId);
    const last = (await modelRequests(id)).at(-1)!;
    expect(JSON.stringify(last.messages)).toContain("kumquat");
  });

  test("a fork made on the local machine resumes on the remote machine with its history, and stays on the local machine", async () => {
    const dir = project();
    const id = await scenario([{ text: "turn-done" }]);
    const first = start(dir);
    const r1 = await first.turn(`[[scenario:${id}]] the secret is persimmon`);
    await first.close();
    const transcript = join(homedir(), ".claude", "projects", projectKey(dir), `${r1.session_id}.jsonl`);
    await until(() => existsSync(transcript) && readFileSync(transcript, "utf8").includes("turn-done"));

    const fork = await forkSession(r1.session_id, { dir });
    expect(onRemote(`ls ~/.claude/projects/*/${fork.sessionId}.jsonl 2>/dev/null | wc -l`)).toBe("0");
    const second = start(dir, { resume: fork.sessionId });
    await second.turn(`[[scenario:${id}]] continue the fork`);
    await second.close();
    expect(onRemote(`ls ~/.claude/projects/*/${fork.sessionId}.jsonl 2>/dev/null | wc -l`)).toBe("0");
    expect(readFileSync(join(homedir(), ".claude", "projects", projectKey(dir), `${fork.sessionId}.jsonl`), "utf8")).toContain("continue the fork");
    expect(JSON.stringify((await modelRequests(id)).at(-1)!.messages)).toContain("persimmon");
  });

  test("an interrupt stops the local command and everything it started", async () => {
    const dir = project();
    const marker = `sleep-${Math.random().toString(36).slice(2)}`;
    const id = await scenario([{ tool: "Bash", input: { command: `bash -c 'exec -a ${marker} sleep 30' & wait; touch finished` } }, { text: "done" }]);
    const session = start(dir, { canUseTool: permissions().canUseTool });
    const running = () => Bun.spawnSync(["pgrep", "-f", marker]).exitCode === 0;
    // The outer shell's command line holds "touch finished"; it must die with the group, not run on.
    const shellAlive = () => Bun.spawnSync(["pgrep", "-f", "touch finished"]).exitCode === 0;
    const done = session.turn(`[[scenario:${id}]] long`);
    expect(await until(running)).toBe(true);
    await session.query.interrupt();
    await done;
    await session.close();
    expect(await until(() => !running() && !shellAlive(), 5000)).toBe(true);
    expect(existsSync(join(dir, "finished"))).toBe(false);
  });

  test("added directories become prompt context and never reach the remote command line", async () => {
    const dir = project();
    const extra = mkdtempSync(join(tmpdir(), "extra-"));
    writeFileSync(join(extra, "e.txt"), "extra-dir-content");
    const log = join(mkdtempSync(join(tmpdir(), "log-")), "claudette.log");
    const id = await scenario([{ tool: "Read", input: { file_path: join(extra, "e.txt") } }, { text: "{{last_tool_result}}" }]);
    const session = start(dir, { additionalDirectories: [extra], env: { ...process.env, CLAUDETTE_LOG: log } });
    const result = await session.turn(`[[scenario:${id}]] extra`);
    await session.close();

    expect(resultText(result)).toContain("extra-dir-content");
    const [first] = await modelRequests(id);
    expect(first!.system).toContain(`Additional working directories: ${extra}`);
    const spawn = readFileSync(log, "utf8").split("\n").find((l) => l.includes('"dir":"spawn"'))!;
    expect(spawn).not.toContain("--add-dir");
  });
});

describe("the remote machine's own configuration", () => {
  test("the remote machine user's skills and settings never reach a session", async () => {
    onRemote("mkdir -p ~/.claude/skills/remote-only && printf -- '---\\nname: remote-only\\ndescription: lives on the remote machine\\n---\\nhi\\n' > ~/.claude/skills/remote-only/SKILL.md");
    try {
      const id = await scenario([{ text: "done" }]);
      const session = start(project(), { settingSources: ["user", "project", "local"] });
      await session.turn(`[[scenario:${id}]] hi`);
      await session.close();
      const [first] = await modelRequests(id);
      expect(JSON.stringify(first)).not.toContain("remote-only");
    } finally {
      onRemote("rm -rf ~/.claude/skills/remote-only");
    }
  });
});

describe("this machine's user configuration", () => {
  const claudeDir = join(homedir(), ".claude");
  /** Writes files under the local machine user's ~/.claude for one test, and takes them away after. */
  function withUserFiles(files: Record<string, string>): () => void {
    for (const [name, content] of Object.entries(files)) {
      mkdirSync(join(claudeDir, name, ".."), { recursive: true });
      writeFileSync(join(claudeDir, name), content);
    }
    return () => {
      for (const name of Object.keys(files)) rmSync(join(claudeDir, name.split("/")[0]!), { recursive: true, force: true });
    };
  }

  test("the user's skills, agents and settings reach the session; login, plugins and Claude Code's own files do not", async () => {
    const settings = {
      apiKeyHelper: "/usr/local/bin/local-key-helper",
      enabledPlugins: { "local-plugin@market": true },
      env: { ANTHROPIC_API_KEY: "sk-local-secret", USER_SETTING_VAR: "kept" },
      hooks: { PostToolUse: [{ matcher: "Read", hooks: [{ type: "command", command: 'hostname > "$CLAUDE_PROJECT_DIR/user-hook.txt"' }] }] },
    };
    const cleanup = withUserFiles({
      "skills/local-user-skill/SKILL.md": "---\nname: local-user-skill\ndescription: In the local machine user's folder\n---\nhi\n",
      "agents/local-user-agent.md": "---\nname: local-user-agent\ndescription: In the local machine user's folder\n---\nhi\n",
      "settings.json": JSON.stringify(settings),
      ".credentials.json": '{"claudeAiOauth":{"accessToken":"local-credential"}}',
    });
    try {
      const dir = project({ "notes.txt": "n\n" });
      const log = join(mkdtempSync(join(tmpdir(), "log-")), "claudette.log");
      const id = await scenario([{ tool: "Read", input: { file_path: join(dir, "notes.txt") } }, { text: "done" }]);
      const session = start(dir, { settingSources: ["user", "project", "local"], env: { ...process.env, CLAUDETTE_LOG: log }, canUseTool: permissions().canUseTool });
      await session.turn(`[[scenario:${id}]] read`);
      await session.close();

      const [first] = await modelRequests(id);
      expect(JSON.stringify(first)).toContain("local-user-skill");
      expect(JSON.stringify(first)).toContain("local-user-agent");
      expect(readFileSync(join(dir, "user-hook.txt"), "utf8")).toBe("local\n");
      // The user's settings reach the remote claude as its Claude Code folder's settings.json, which
      // the frame log records as given; the command line carries none of them.
      const given = readFileSync(log, "utf8").split("\n").find((l) => l.includes('"dir":"user-settings"'))!;
      expect(given).toContain("USER_SETTING_VAR");
      const spawn = readFileSync(log, "utf8").split("\n").find((l) => l.includes('"dir":"spawn"'))!;
      for (const secret of ["sk-local-secret", "local-key-helper", "local-plugin@market", "local-credential"]) {
        expect(given).not.toContain(secret);
        expect(spawn).not.toContain(secret);
      }
      expect(JSON.stringify(await modelRequests(id))).not.toContain("local-credential");
      // Nothing is copied to the remote machine: its own folder never gets the local machine user's files.
      expect(onRemote("ls ~/.claude/skills 2>/dev/null; grep -rl local-credential ~/.claude ~/.claude.json 2>/dev/null")).toBe("");
      expect(onRemote("ls -d /tmp/claudette-*.user 2>/dev/null")).toBe("");
    } finally {
      cleanup();
    }
  });

  test("a plugin installed on the local machine (on by default): its skill gets its options, its hook and MCP server run locally", async () => {
    const root = join(claudeDir, "plugins/cache/market/lint/1.0.0");
    const cleanup = withUserFiles({
      // No enabledPlugins entry: an installed plugin is on unless its manifest says otherwise.
      "settings.json": JSON.stringify({ pluginConfigs: { "lint@market": { options: { level: "7" } } } }),
      "plugins/installed_plugins.json": JSON.stringify({ version: 2, plugins: { "lint@market": [{ scope: "user", installPath: root, version: "1.0.0" }] } }),
      "plugins/cache/market/lint/1.0.0/.claude-plugin/plugin.json": JSON.stringify({ name: "lint", version: "1.0.0", userConfig: { level: { type: "string", title: "Level", description: "How strict" } } }),
      "plugins/cache/market/lint/1.0.0/skills/lint-skill/SKILL.md": "---\nname: lint-skill\ndescription: From the local machine's plugin\n---\nLEVEL=${user_config.level}\n",
      "plugins/cache/market/lint/1.0.0/hooks/hooks.json": JSON.stringify({
        hooks: { PostToolUse: [{ matcher: "Read", hooks: [{ type: "command", command: 'echo "$(hostname) $CLAUDE_PLUGIN_ROOT $CLAUDE_PLUGIN_OPTION_LEVEL" > "$CLAUDE_PROJECT_DIR/plugin-hook.txt"' }] }] },
      }),
      "plugins/cache/market/lint/1.0.0/.mcp.json": JSON.stringify({ mcpServers: { local: { command: "bun", args: ["/app/fixtures/stdio-mcp.ts"] } } }),
    });
    try {
      const dir = project({ "notes.txt": "n\n" });
      const id = await scenario([
        { tool: "Read", input: { file_path: join(dir, "notes.txt") } },
        { tool: "Skill", input: { skill: "lint:lint-skill" } },
        { tool: "mcp__plugin_lint_local__where", input: {} },
        { text: "{{last_tool_result}}" },
      ]);
      const session = start(dir, { settingSources: ["user", "project", "local"], env: noToolSearch, canUseTool: permissions().canUseTool });
      const result = await session.turn(`[[scenario:${id}]] read, use the skill, then ask the plugin's server`);
      await session.close();

      const requests = await modelRequests(id);
      expect(JSON.stringify(requests[0])).toContain("lint-skill");
      expect(requests[0]!.tools).toContain("mcp__plugin_lint_local__where");
      expect(JSON.stringify(requests)).toContain("LEVEL=7");
      expect(resultText(result)).toBe(`stdio-server-on:local cwd:${dir}`);
      expect(readFileSync(join(dir, "plugin-hook.txt"), "utf8")).toBe(`local ${root} 7\n`);
      // The remote machine installs nothing and keeps no mount.
      expect(onRemote("ls ~/.claude/plugins/cache 2>/dev/null; ls -d /tmp/claudette-*.plugins 2>/dev/null")).toBe("");
    } finally {
      cleanup();
    }
  });

  test("an 'always allow' the host saves to user settings lands in the local machine's ~/.claude/settings.json", async () => {
    const cleanup = withUserFiles({ "settings.json": "{}" });
    try {
      const dir = project();
      const id = await scenario([
        { tool: "Write", input: { file_path: join(dir, "a.txt"), content: "a" } },
        { tool: "Write", input: { file_path: join(dir, "b.txt"), content: "b" } },
        { text: "done" },
      ]);
      const perms = permissions((ask) => ({
        behavior: "allow",
        updatedInput: ask.input,
        updatedPermissions: [{ type: "addRules", rules: [{ toolName: "Write" }], behavior: "allow", destination: "userSettings" }],
      }));
      const session = start(dir, { settingSources: ["user", "project", "local"], canUseTool: perms.canUseTool });
      await session.turn(`[[scenario:${id}]] write twice`);
      await session.close();

      expect(perms.asked).toHaveLength(1);
      expect(JSON.parse(readFileSync(join(claudeDir, "settings.json"), "utf8")).permissions.allow).toEqual(["Write"]);
      expect(onRemote("cat ~/.claude/settings.json 2>/dev/null")).not.toContain("Write");
    } finally {
      cleanup();
    }
  });
});

describe("an existing Claude Code setup on this machine carries over", () => {
  const projectSources = { settingSources: ["project", "local"] as ("project" | "local")[] };
  const toolResult = (request: { messages: unknown[] } | undefined) => JSON.stringify(request!.messages.at(-1));

  test("Read and Edit path rules hold for the local tools: a deny beats every approval, an ask beats a bare allow", async () => {
    const settings = { permissions: { allow: ["Write"], ask: ["Edit(src/**)"], deny: ["Read(./.env)"] } };
    const dir = project({ ".env": "SECRET=local\n", "notes.txt": "SECRET in notes\n", "src/a.ts": "a\n", ".claude/settings.json": JSON.stringify(settings) });
    const id = await scenario([
      { tool: "Read", input: { file_path: join(dir, ".env") } },
      { tool: "Grep", input: { pattern: "SECRET" } },
      { tool: "Write", input: { file_path: join(dir, "src/new.ts"), content: "asked\n" } },
      { tool: "Write", input: { file_path: join(dir, "free.txt"), content: "free\n" } },
      { tool: "Write", input: { file_path: join(dir, ".env"), content: "overwritten\n" } },
      { text: "done" },
    ]);
    const perms = permissions();
    const session = start(dir, { ...projectSources, canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] rules`);
    await session.close();

    const requests = await modelRequests(id);
    expect(toolResult(requests[1])).toContain("denied by your permission settings");
    expect(toolResult(requests[1])).not.toContain("SECRET=local");
    expect(toolResult(requests[2])).toContain("notes.txt");
    expect(toolResult(requests[2])).not.toContain(".env");
    // Only the ask rule's path reached the host; the bare Write allow covered the other file.
    expect(perms.asked.map((a) => a.input.file_path)).toEqual([join(dir, "src/new.ts")]);
    expect(readFileSync(join(dir, "src/new.ts"), "utf8")).toBe("asked\n");
    expect(readFileSync(join(dir, "free.txt"), "utf8")).toBe("free\n");
    // A Read deny also blocks writing the file, whatever allows Write.
    expect(readFileSync(join(dir, ".env"), "utf8")).toBe("SECRET=local\n");
  });

  test("settings env reaches the local machine's commands, and additionalDirectories are working directories here", async () => {
    const extra = realpathSync(mkdtempSync(join(tmpdir(), "extra-")));
    const settings = { env: { LOCAL_SETTING: "from-settings" }, permissions: { additionalDirectories: [extra] } };
    const dir = project({ ".claude/settings.json": JSON.stringify(settings) });
    const id = await scenario([
      { tool: "Bash", input: { command: "echo $LOCAL_SETTING" } },
      { tool: "Write", input: { file_path: join(extra, "x.txt"), content: "x" } },
      { text: "done" },
    ]);
    const perms = permissions();
    const session = start(dir, { ...projectSources, permissionMode: "acceptEdits", canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] env`);
    await session.close();

    const requests = await modelRequests(id);
    expect(toolResult(requests[1])).toContain("from-settings");
    // acceptEdits covers the added directory as it covers the project.
    expect(perms.asked.map((a) => a.toolName)).toEqual(["Bash"]);
    expect(readFileSync(join(extra, "x.txt"), "utf8")).toBe("x");
  });
});

describe("the project's own configuration, through the mounted workspace", () => {
  test("a project skill reaches the remote claude", async () => {
    const dir = project({ ".claude/skills/local-skill/SKILL.md": "---\nname: local-skill\ndescription: Lives in the local project\n---\nSay hi.\n" });
    const id = await scenario([{ text: "done" }]);
    const session = start(dir, { settingSources: ["project"] });
    await session.turn(`[[scenario:${id}]] hi`);
    await session.close();
    const [first] = await modelRequests(id);
    expect(JSON.stringify(first)).toContain("local-skill");
  });

  test("an 'always allow' saved for the project lands in the local machine's settings.local.json, under the built-in name", async () => {
    const dir = project();
    const id = await scenario([
      { tool: "Write", input: { file_path: join(dir, "a.txt"), content: "a" } },
      { tool: "Write", input: { file_path: join(dir, "b.txt"), content: "b" } },
      { text: "done" },
    ]);
    const perms = permissions((ask) => ({
      behavior: "allow",
      updatedInput: ask.input,
      updatedPermissions: [{ type: "addRules", rules: [{ toolName: "Write" }], behavior: "allow", destination: "localSettings" }],
    }));
    const session = start(dir, { settingSources: ["project", "local"], canUseTool: perms.canUseTool });
    await session.turn(`[[scenario:${id}]] write twice`);
    await session.close();

    expect(perms.asked).toHaveLength(1);
    expect(readFileSync(join(dir, "b.txt"), "utf8")).toBe("b");
    const saved = JSON.parse(readFileSync(join(dir, ".claude", "settings.local.json"), "utf8"));
    expect(saved.permissions.allow).toEqual(["Write"]);
    expect(onRemote(`ls -A ${remoteDir(dir)}`)).toBe("");
  });

  test("a project hook runs on the local machine, with local paths, built-in tool names, and a transcript_path it can read", async () => {
    const settings = {
      hooks: {
        PostToolUse: [
          { matcher: "Read", hooks: [{ type: "command", command: 'cat > "$CLAUDE_PROJECT_DIR/hook-input.json"; hostname > "$CLAUDE_PROJECT_DIR/hook-host.txt"' }] },
        ],
      },
    };
    const dir = project({ "notes.txt": "hello\n", ".claude/settings.json": JSON.stringify(settings) });
    const id = await scenario([{ tool: "Read", input: { file_path: join(dir, "notes.txt") } }, { text: "done" }]);
    const session = start(dir, { settingSources: ["project"], canUseTool: permissions().canUseTool });
    await session.turn(`[[scenario:${id}]] read`);
    await session.close();

    expect(readFileSync(join(dir, "hook-host.txt"), "utf8")).toBe("local\n");
    const input = JSON.parse(readFileSync(join(dir, "hook-input.json"), "utf8"));
    expect(input.tool_name).toBe("Read");
    expect(input.tool_input.file_path).toBe(join(dir, "notes.txt"));
    expect(input.cwd).toBe(dir);
    expect(input.transcript_path).toBe(join(homedir(), ".claude", "projects", projectKey(dir), `${input.session_id}.jsonl`));
    expect(readFileSync(input.transcript_path, "utf8")).toContain("notes.txt");
    expect(JSON.parse(readFileSync(join(dir, ".claude", "settings.json"), "utf8"))).toEqual(settings);
  });

  test("a blocking PreToolUse hook (exit 2) on the local machine stops the tool", async () => {
    const settings = { hooks: { PreToolUse: [{ matcher: "Bash", hooks: [{ type: "command", command: "echo 'not on this local machine' >&2; exit 2" }] }] } };
    const dir = project({ ".claude/settings.json": JSON.stringify(settings) });
    const id = await scenario([{ tool: "Bash", input: { command: "touch should-not-exist" } }, { text: "error={{last_tool_error}}" }]);
    const session = start(dir, { settingSources: ["project"], canUseTool: permissions().canUseTool });
    const result = await session.turn(`[[scenario:${id}]] try`);
    await session.close();
    expect(existsSync(join(dir, "should-not-exist"))).toBe(false);
    expect(resultText(result)).toBe("error=true");
  });
});

describe("one-shot invocations", () => {
  test("--version and -p run claude on the remote machine as is", () => {
    const version = Bun.spawnSync(["claudette", "--version"]).stdout.toString();
    expect(version).toMatch(/\d+\.\d+\.\d+ \(Claude Code\)/);
    const print = Bun.spawnSync(["claudette", "-p", "--output-format", "json", "title please"], { stdin: "ignore" });
    expect(JSON.parse(print.stdout.toString()).result).toBe("ok");
  });

  test("+doctor reports the remote claude, the mount and the sandbox", () => {
    const out = Bun.spawnSync(["claudette", "+doctor"]).stdout.toString();
    expect(out).toContain("remote claude: 2.1.319 (Claude Code)");
    expect(out).toContain("remote sshfs: found");
    expect(out).toContain("remote sandbox: bubblewrap");
  });
});
