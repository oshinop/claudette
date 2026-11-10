// A stdio MCP server the host configures; claudette must run it on the local machine.
import { hostname } from "node:os";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

const server = new McpServer({ name: "local-stdio", version: "1.0.0" });
server.registerTool("where", { description: "Reports where this server runs" }, async () => ({
  content: [{ type: "text", text: `stdio-server-on:${hostname()} cwd:${process.cwd()}` }],
}));
server.registerResource("note", "local://note", { description: "A note from the local machine", mimeType: "text/plain" }, async (uri) => ({
  contents: [{ uri: uri.href, text: "resource-from-local" }],
}));
server.registerPrompt("local-prompt", { description: "A prompt supplied by the local machine" }, async () => ({
  messages: [{ role: "user", content: { type: "text", text: "prompt-from-local" } }],
}));
await server.connect(new StdioServerTransport());
server.server.sendResourceListChanged();
