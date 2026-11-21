import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { basename, dirname, join } from "node:path";
import {
  type CanUseTool,
  type Options,
  type PermissionResult,
  query,
  type Query,
  type SDKMessage,
  type SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";

export const MOCK_API = process.env.MOCK_API ?? "http://mockapi:8080";
export const CLAUDETTE = "/usr/local/bin/claudette";

export type Step = { tool: string; input: Record<string, unknown> } | { text: string };

export type RecordedRequest = {
  scenario: string | null;
  step: number | null;
  model: string;
  tools: string[];
  system: string;
  messages: { role: string; content: unknown }[];
  reply: { content: { type: string; name?: string; text?: string }[] };
};

export async function scenario(steps: Step[]): Promise<string> {
  const id = `s${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`${MOCK_API}/_scenario`, { method: "POST", body: JSON.stringify({ id, steps }) });
  if (!res.ok) throw new Error(`mock api: ${res.status}`);
  return id;
}

export async function modelRequests(id: string): Promise<RecordedRequest[]> {
  const res = await fetch(`${MOCK_API}/_log?scenario=${id}`);
  const all = (await res.json()) as RecordedRequest[];
  return all.filter((r) => r.step !== null);
}

export function project(files: Record<string, string> = {}): string {
  const dir = mkdtempSync(join(homedir(), "proj-"));
  for (const [name, content] of Object.entries(files)) {
    mkdirSync(dirname(join(dir, name)), { recursive: true });
    writeFileSync(join(dir, name), content);
  }
  Bun.spawnSync(["git", "init", "-q"], { cwd: dir });
  Bun.spawnSync(["git", "add", "-A"], { cwd: dir });
  Bun.spawnSync(["git", "commit", "-q", "-m", "init", "--allow-empty"], { cwd: dir });
  return dir;
}

class Prompts implements AsyncIterable<SDKUserMessage> {
  private queue: SDKUserMessage[] = [];
  private waiting: ((r: IteratorResult<SDKUserMessage>) => void) | undefined;
  private closed = false;

  push(text: string) {
    const message: SDKUserMessage = { type: "user", message: { role: "user", content: text }, parent_tool_use_id: null, session_id: "" };
    if (this.waiting) {
      const resolve = this.waiting;
      this.waiting = undefined;
      resolve({ value: message, done: false });
    } else this.queue.push(message);
  }

  close() {
    this.closed = true;
    this.waiting?.({ value: undefined, done: true });
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    return {
      next: () => {
        const next = this.queue.shift();
        if (next) return Promise.resolve({ value: next, done: false });
        if (this.closed) return Promise.resolve({ value: undefined, done: true });
        const { promise, resolve } = Promise.withResolvers<IteratorResult<SDKUserMessage>>();
        this.waiting = resolve;
        return promise;
      },
    };
  }
}

export type Session = {
  query: Query;
  messages: SDKMessage[];
  stderr: string[];
  turn(text: string): Promise<Extract<SDKMessage, { type: "result" }>>;
  close(): Promise<void>;
};

export function start(cwd: string, options: Partial<Options> & { canUseTool?: CanUseTool } = {}): Session {
  const prompts = new Prompts();
  const stderr: string[] = [];
  const q = query({
    prompt: prompts,
    options: {
      pathToClaudeCodeExecutable: CLAUDETTE,
      cwd,
      includePartialMessages: true,
      permissionMode: "default",
      systemPrompt: { type: "preset", preset: "claude_code", append: "You are running inside a T3-shaped test host." },
      stderr: (s) => stderr.push(s),
      env: { ...process.env },
      ...options,
    },
  });
  const messages: SDKMessage[] = [];
  let resultWaiter: ((m: Extract<SDKMessage, { type: "result" }>) => void) | undefined;
  const pump = (async () => {
    for await (const m of q) {
      messages.push(m);
      if (m.type === "result") resultWaiter?.(m);
    }
  })();
  return {
    query: q,
    messages,
    stderr,
    turn(text) {
      const { promise, resolve } = Promise.withResolvers<Extract<SDKMessage, { type: "result" }>>();
      resultWaiter = resolve;
      prompts.push(text);
      return promise;
    },
    async close() {
      prompts.close();
      await pump.catch(() => {});
    },
  };
}

export function toolUses(messages: SDKMessage[]): { name: string; input: Record<string, unknown> }[] {
  return messages.flatMap((m) =>
    m.type === "assistant"
      ? m.message.content.flatMap((b) => (b.type === "tool_use" ? [{ name: b.name, input: b.input as Record<string, unknown> }] : []))
      : [],
  );
}

export function resultText(result: Extract<SDKMessage, { type: "result" }>): string {
  return result.subtype === "success" ? result.result : `ERROR(${result.subtype})`;
}

export function onRemote(command: string): string {
  const proc = Bun.spawnSync(["ssh", "remote", command], { stdout: "pipe", stderr: "pipe" });
  return proc.stdout.toString().trim();
}

export const plain = (screen: string) =>
  screen
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?<>=]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[()][A-Za-z0-9]/g, "")
    .replace(/\x1b[=>78]/g, "")
    .replace(/\s+/g, "");

export async function launch(dir: string, args: string[], until: string) {
  let screen = "";
  const decoder = new TextDecoder();
  const proc = Bun.spawn(["claudette", ...args], {
    cwd: dir,
    env: { ...process.env, TERM: "xterm-256color" },
    terminal: { cols: 140, rows: 45, data: (_t, d) => void (screen += decoder.decode(d, { stream: true })) },
  });
  const text = () => plain(screen);
  let trusted = false;
  let lastKey = 0;
  const waitFor = async (what: string, ms = 60_000) => {
    const end = Date.now() + ms;
    while (Date.now() < end && !text().includes(what)) {
      const now = text();
      if (!trusted && now.includes("Entertoconfirm") && Date.now() - lastKey > 500) {
        lastKey = Date.now();
        if (now.slice(now.lastIndexOf("❯") + 1).startsWith("Yes")) {
          trusted = true;
          proc.terminal!.write("\r");
        } else {
          proc.terminal!.write("\x1b[B");
        }
      }
      await Bun.sleep(100);
    }
    return text().includes(what);
  };
  await waitFor(until);
  const exited = async (ms = 15_000) => Promise.race([proc.exited, Bun.sleep(ms).then(() => "timeout" as const)]);
  return {
    proc,
    text,
    waitFor,
    exited,
    write: (keys: string) => proc.terminal!.write(keys),
    async type(line: string) {
      proc.terminal!.write(line);
      await Bun.sleep(400);
      proc.terminal!.write("\r");
    },
    async exit() {
      proc.terminal!.write("\x03");
      await Bun.sleep(300);
      proc.terminal!.write("\x03");
      if ((await exited()) === "timeout") proc.kill();
    },
  };
}

const REMOTE_SERVER = `const os = require("os"); const { spawnSync } = require("child_process"); let b = "";
process.stdin.on("data", (d) => { b += d; let i;
  while ((i = b.indexOf("\\n")) >= 0) { const l = b.slice(0, i); b = b.slice(i + 1); if (!l.trim()) continue; const m = JSON.parse(l);
    const r = (x) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result: x }) + "\\n");
    const text = (t) => r({ content: [{ type: "text", text: t }] });
    if (m.method === "initialize") r({ protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "remote", version: "1" } });
    else if (m.method === "tools/list") r({ tools: [{ name: "where", description: "Reports where this server runs", inputSchema: { type: "object" } }, { name: "open", description: "Opens a link with $BROWSER", inputSchema: { type: "object", properties: { url: { type: "string" } } } }, { name: "run", description: "Runs a shell command", inputSchema: { type: "object", properties: { command: { type: "string" } } } }] });
    else if (m.method === "tools/call" && m.params.name === "open") { const o = spawnSync(process.env.BROWSER, [m.params.arguments.url], { encoding: "utf8" }); text("status:" + o.status + " " + (o.stderr || "").trim()); }
    else if (m.method === "tools/call" && m.params.name === "run") { const o = spawnSync("sh", ["-c", m.params.arguments.command], { encoding: "utf8" }); text((o.stdout || "") + (o.stderr || "")); }
    else if (m.method === "tools/call") text("stdio-server-on:" + os.hostname() + " token:" + (process.env.TOKEN || ""));
    else if (m.id !== undefined) r({}); } });`;

export function installRemoteServer(): string {
  onRemote(`cat > /tmp/where.cjs <<'EOF'\n${REMOTE_SERVER}\nEOF`);
  return "/tmp/where.cjs";
}

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

export function remoteDir(dir: string): string {
  const id = ["/etc/machine-id", "/var/lib/dbus/machine-id"].map((f) => {
    try {
      return readFileSync(f, "utf8").trim();
    } catch {
      return "";
    }
  }).find(Boolean) ?? hostname();
  const path = realpathSync(dir);
  const name = basename(path).replace(/[^A-Za-z0-9._-]/g, "_") || "root";
  return `~/.claudette/ws/${sha256(`claudette:${id}`).slice(0, 12)}/${name}-${sha256(path).slice(0, 8)}`;
}

export const projectKey = (dir: string) => realpathSync(dir).replace(/[^a-zA-Z0-9]/g, "-");

export type Ask = { toolName: string; input: Record<string, unknown>; suggestions: unknown };

export function permissions(decide?: (ask: Ask) => PermissionResult) {
  const asked: Ask[] = [];
  const canUseTool: CanUseTool = async (toolName, input, { suggestions }) => {
    const ask = { toolName, input, suggestions };
    asked.push(ask);
    return decide ? decide(ask) : { behavior: "allow", updatedInput: input };
  };
  return { asked, canUseTool };
}

export async function until(check: () => boolean, timeoutMs = 10_000): Promise<boolean> {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    if (check()) return true;
    await Bun.sleep(50);
  }
  return check();
}
