/**
 * A scripted stand-in for the Anthropic Messages API, for the real Claude Code CLI on `remote`.
 *
 * Tests register a scenario (POST /_scenario) and put `[[scenario:<id>]]` in the prompt. The latest
 * user turn carrying a marker picks the scenario; each model request plays the next step, chosen by
 * how many tool results followed that turn. A tool step names a tool by its model-visible name or suffix (`Read`
 * matches `mcp__cc__Read`), so a test fails loudly when the tool the step needs is not offered.
 * Requests without tools, or without a scenario marker (titles, quota checks), get a short text reply.
 *
 * Every /v1/messages request is recorded; GET /_log?scenario=<id> returns them.
 */

type Block = { type: string; text?: string; content?: unknown; name?: string; id?: string; input?: unknown; is_error?: boolean };
type Message = { role: string; content: string | Block[] };
type Step = { tool: string; input: Record<string, unknown> } | { text: string };
type Recorded = {
  scenario: string | null;
  step: number | null;
  model: string;
  stream: boolean;
  tools: string[];
  system: string;
  messages: Message[];
  reply: unknown;
};

const scenarios = new Map<string, Step[]>();
const log: Recorded[] = [];
const MARKER = /\[\[scenario:([\w-]+)\]\]/;

const blocks = (m: Message): Block[] => (typeof m.content === "string" ? [{ type: "text", text: m.content }] : m.content);

function textOf(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textOf).join("");
  if (value && typeof value === "object") {
    if ("type" in value && value.type === "text" && "text" in value && typeof value.text === "string") return value.text;
    if ("content" in value) return textOf(value.content);
  }
  return "";
}

/** The latest user turn that names a scenario, and its position; tool output never counts as a turn. */
function findScenario(messages: Message[]): { id: string; at: number } | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]!;
    if (m.role !== "user") continue;
    for (const b of blocks(m)) {
      if (b.type === "tool_result") continue;
      const found = textOf(b).match(MARKER);
      if (found) return { id: found[1]!, at: i };
    }
  }
  return null;
}

function toolResults(messages: Message[]): Block[] {
  return messages.flatMap((m) => (m.role === "user" ? blocks(m).filter((b) => b.type === "tool_result") : []));
}

let ids = 0;
const nextId = (prefix: string) => `${prefix}_${(++ids).toString().padStart(6, "0")}`;

type Reply = { content: Block[]; stop_reason: "end_turn" | "tool_use" };

function plan(body: { messages: Message[]; tools?: { name: string }[] }, scenario: { id: string; at: number } | null): { reply: Reply; step: number | null } {
  const steps = scenario ? scenarios.get(scenario.id) : undefined;
  if (!scenario || !steps || !body.tools?.length) return { reply: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" }, step: null };
  const results = toolResults(body.messages.slice(scenario.at + 1));
  const index = results.length;
  const step = steps[Math.min(index, steps.length - 1)]!;
  if ("text" in step) {
    const last = results.at(-1);
    const text = step.text
      .replaceAll("{{last_tool_result}}", last ? textOf(last.content) : "")
      .replaceAll("{{last_tool_error}}", last?.is_error ? "true" : "false");
    return { reply: { content: [{ type: "text", text }], stop_reason: "end_turn" }, step: index };
  }
  const tool = body.tools.find((t) => t.name === step.tool) ?? body.tools.find((t) => t.name.endsWith(`__${step.tool}`));
  if (!tool) {
    const offered = body.tools.map((t) => t.name).join(",");
    return { reply: { content: [{ type: "text", text: `TOOL_NOT_OFFERED:${step.tool} offered=${offered}` }], stop_reason: "end_turn" }, step: index };
  }
  return {
    reply: { content: [{ type: "tool_use", id: nextId("toolu"), name: tool.name, input: step.input }], stop_reason: "tool_use" },
    step: index,
  };
}

const usage = { input_tokens: 12, output_tokens: 6, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 };

function sse(model: string, reply: Reply): Response {
  const encoder = new TextEncoder();
  const events: [string, unknown][] = [];
  const id = nextId("msg");
  events.push(["message_start", { type: "message_start", message: { id, type: "message", role: "assistant", model, content: [], stop_reason: null, stop_sequence: null, usage } }]);
  reply.content.forEach((block, index) => {
    if (block.type === "text") {
      events.push(["content_block_start", { type: "content_block_start", index, content_block: { type: "text", text: "" } }]);
      events.push(["content_block_delta", { type: "content_block_delta", index, delta: { type: "text_delta", text: block.text } }]);
    } else {
      events.push(["content_block_start", { type: "content_block_start", index, content_block: { type: "tool_use", id: block.id, name: block.name, input: {} } }]);
      events.push(["content_block_delta", { type: "content_block_delta", index, delta: { type: "input_json_delta", partial_json: JSON.stringify(block.input) } }]);
    }
    events.push(["content_block_stop", { type: "content_block_stop", index }]);
  });
  events.push(["message_delta", { type: "message_delta", delta: { stop_reason: reply.stop_reason, stop_sequence: null }, usage: { output_tokens: 6 } }]);
  events.push(["message_stop", { type: "message_stop" }]);
  const body = events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join("");
  return new Response(encoder.encode(body), {
    headers: { "content-type": "text/event-stream", "cache-control": "no-cache", "request-id": nextId("req") },
  });
}

function systemText(system: unknown): string {
  if (typeof system === "string") return system;
  if (Array.isArray(system)) return system.map(textOf).join("\n");
  return "";
}

const server = Bun.serve({
  port: 8080,
  hostname: "0.0.0.0",
  idleTimeout: 0,
  async fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === "/_health") return new Response("ok");
    if (url.pathname === "/_scenario" && req.method === "POST") {
      const { id, steps } = (await req.json()) as { id: string; steps: Step[] };
      scenarios.set(id, steps);
      return Response.json({ ok: true });
    }
    if (url.pathname === "/_log") {
      const scenario = url.searchParams.get("scenario");
      return Response.json(scenario ? log.filter((r) => r.scenario === scenario) : log);
    }
    if (url.pathname === "/v1/messages/count_tokens") return Response.json({ input_tokens: 12 });
    if (url.pathname === "/v1/messages" && req.method === "POST") {
      const body = (await req.json()) as { model: string; stream?: boolean; messages: Message[]; tools?: { name: string }[]; system?: unknown };
      const scenario = findScenario(body.messages);
      const { reply, step } = plan(body, scenario);
      log.push({
        scenario: scenario?.id ?? null,
        step,
        model: body.model,
        stream: body.stream === true,
        tools: (body.tools ?? []).map((t) => t.name),
        system: systemText(body.system),
        messages: body.messages,
        reply,
      });
      if (body.stream) return sse(body.model, reply);
      return Response.json({ id: nextId("msg"), type: "message", role: "assistant", model: body.model, content: reply.content, stop_reason: reply.stop_reason, stop_sequence: null, usage });
    }
    console.error(`mock-api: unhandled ${req.method} ${url.pathname}`);
    return Response.json({ type: "error", error: { type: "not_found_error", message: `mock-api has no ${url.pathname}` } }, { status: 404 });
  },
});

console.error(`mock-api listening on ${server.port}`);
