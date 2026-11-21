import { startHttpMcp } from "./http-mcp";

const server = startHttpMcp(process.argv[2]!);
console.log(server.url);
