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

/** Registers a scripted model conversation; the prompt must contain `[[scenario:<id>]]`. */
export async function scenario(steps: Step[]): Promise<string> {
  const id = `s${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`${MOCK_API}/_scenario`, { method: "POST", body: JSON.stringify({ id, steps }) });
  if (!res.ok) throw new Error(`mock api: ${res.status}`);
  return id;
}

export async function modelRequests(id: string): Promise<RecordedRequest[]> {
  const res = await fetch(`${MOCK_API}/_log?scenario=${id}`);
  // The mock API is this suite's own fixture; its log shape is RecordedRequest by construction.
  const all = (await res.json()) as RecordedRequest[];
  return all.filter((r) => r.step !== null);
}

/** A fresh git repository on the local machine, with the given files committed. */
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

/** Streaming input, the way T3 Code and other long-lived hosts feed a session. */
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
  /** Sends one user turn and waits for its result message. */
  turn(text: string): Promise<Extract<SDKMessage, { type: "result" }>>;
  close(): Promise<void>;
};

/**
 * Starts a session the way T3 Code's Claude provider does: streaming input, partial messages, a
 * preset system prompt with an append, a permission callback, claudette as the executable.
 */
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

/** Every tool_use block the host saw in assistant messages. */
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

/** Runs a shell command on the remote machine over the same ssh setup claudette uses. */
export function onRemote(command: string): string {
  const proc = Bun.spawnSync(["ssh", "remote", command], { stdout: "pipe", stderr: "pipe" });
  return proc.stdout.toString().trim();
}

/**
 * The screen as text with escape sequences and whitespace removed: the interface moves the cursor
 * instead of printing spaces, so words arrive run together.
 */
export const plain = (screen: string) =>
  screen
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?<>=]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[()][A-Za-z0-9]/g, "")
    .replace(/\x1b[=>78]/g, "")
    .replace(/\s+/g, "");

/**
 * Starts claudette in a pseudo-terminal, accepts the trust prompt if Claude Code shows it, and
 * waits until the screen shows `until`.
 */
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
  /** Waits for the screen to show `what`; true when it did. */
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
  /** The exit code, or "timeout". */
  const exited = async (ms = 15_000) => Promise.race([proc.exited, Bun.sleep(ms).then(() => "timeout" as const)]);
  return {
    proc,
    text,
    waitFor,
    exited,
    write: (keys: string) => proc.terminal!.write(keys),
    /** Types a line into the interface, as a person does: the text, then Enter. */
    async type(line: string) {
      proc.terminal!.write(line);
      await Bun.sleep(400);
      proc.terminal!.write("\r");
    },
    /** Ctrl-C twice: Claude Code's own exit. */
    async exit() {
      proc.terminal!.write("\x03");
      await Bun.sleep(300);
      proc.terminal!.write("\x03");
      if ((await exited()) === "timeout") proc.kill();
    },
  };
}

/** A stdio MCP server for the remote machine, which has node but no MCP SDK. Its `where` tool reports the host and $TOKEN. */
const REMOTE_SERVER = `const os = require("os"); let b = "";
process.stdin.on("data", (d) => { b += d; let i;
  while ((i = b.indexOf("\\n")) >= 0) { const l = b.slice(0, i); b = b.slice(i + 1); if (!l.trim()) continue; const m = JSON.parse(l);
    const r = (x) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result: x }) + "\\n");
    if (m.method === "initialize") r({ protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "remote", version: "1" } });
    else if (m.method === "tools/list") r({ tools: [{ name: "where", description: "Reports where this server runs", inputSchema: { type: "object" } }] });
    else if (m.method === "tools/call") r({ content: [{ type: "text", text: "stdio-server-on:" + os.hostname() + " token:" + (process.env.TOKEN || "") }] });
    else if (m.id !== undefined) r({}); } });`;

/** Puts the remote machine's MCP server at /tmp/where.cjs on the remote machine and returns that path. */
export function installRemoteServer(): string {
  onRemote(`cat > /tmp/where.cjs <<'EOF'\n${REMOTE_SERVER}\nEOF`);
  return "/tmp/where.cjs";
}

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

/** The remote folder claudette uses for a local directory, as `~/…` for onRemote. */
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

/** The projects folder name Claude Code uses for a local directory (short paths only). */
export const projectKey = (dir: string) => realpathSync(dir).replace(/[^a-zA-Z0-9]/g, "-");

export type Ask = { toolName: string; input: Record<string, unknown>; suggestions: unknown };

/** A permission callback that records every question and answers with `decide` (allow by default). */
export function permissions(decide?: (ask: Ask) => PermissionResult) {
  const asked: Ask[] = [];
  const canUseTool: CanUseTool = async (toolName, input, { suggestions }) => {
    const ask = { toolName, input, suggestions };
    asked.push(ask);
    return decide ? decide(ask) : { behavior: "allow", updatedInput: input };
  };
  return { asked, canUseTool };
}

/**
 * Polls until `check` passes or the timeout ends. The conditions live in other processes (a
 * transcript file, the process table), which offer no event to await.
 */
export async function until(check: () => boolean, timeoutMs = 10_000): Promise<boolean> {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    if (check()) return true;
    await Bun.sleep(50);
  }
  return check();
}
