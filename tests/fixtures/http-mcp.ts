import { hostname } from "node:os";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";

type Session = { mcp: McpServer; transport: WebStandardStreamableHTTPServerTransport };

/**
 * A token-protected MCP server on the local machine's loopback, like the one an SDK host (T3 Code) runs for
 * its own tools. The remote claude cannot reach 127.0.0.1 here, so only a local bridge can call it.
 */
export function startHttpMcp(token: string) {
  const authorizations: (string | null)[] = [];
  const sessions = new Map<string, Session>();
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      authorizations.push(req.headers.get("authorization"));
      if (req.headers.get("authorization") !== `Bearer ${token}`) return new Response("unauthorized", { status: 401 });
      const sessionId = req.headers.get("mcp-session-id");
      let session = sessionId ? sessions.get(sessionId) : undefined;
      if (!session) {
        if (req.method !== "POST") return new Response("session required", { status: 400 });
        const mcp = new McpServer({ name: "local-http", version: "1.0.0" });
        mcp.registerTool("whoami", { description: "Reports where this server runs" }, async () => ({
          content: [{ type: "text", text: `http-server-on:${hostname()}` }],
        }));
        mcp.registerResource("note", "local://http-note", { description: "A note from the local machine", mimeType: "text/plain" }, async (uri) => ({
          contents: [{ uri: uri.href, text: "http-resource-from-local" }],
        }));
        mcp.registerPrompt("local-prompt", { description: "A prompt supplied by the local machine" }, async () => ({
          messages: [{ role: "user", content: { type: "text", text: "http-prompt-from-local" } }],
        }));
        const transport = new WebStandardStreamableHTTPServerTransport({ sessionIdGenerator: () => crypto.randomUUID(), enableJsonResponse: true });
        await mcp.connect(transport);
        session = { mcp, transport };
        const response = await transport.handleRequest(req);
        const id = response.headers.get("mcp-session-id");
        if (id) sessions.set(id, session);
        return response;
      }
      const response = await session.transport.handleRequest(req);
      if (req.method === "GET") session.mcp.server.sendResourceListChanged();
      return response;
    },
  });
  return { url: `http://127.0.0.1:${server.port}/mcp`, authorizations, stop: () => server.stop(true) };
}
