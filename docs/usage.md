# Usage

Once claudette is installed and linked as `claude` ([installation](configuration.md)), you use it as
you use Claude Code. This page covers what's specific to claudette and what to watch out for.

## Running claude

```sh
claude                                  # Claude Code's interface
claude -p "summarize the last commit"   # one-shot, for scripts
claude --continue                       # the project's last session
claude --resume <session-id>            # any session of the project
```

Sessions are stored on the laptop, in `~/.claude/projects`, as a local claude stores them. So
`--continue` and `--resume` find every session of the project: the ones started from a terminal, the
ones T3 Code or Paseo started, and the ones a local claude wrote before you switched.

Claude Code's own commands, such as `claude auth` or `claude doctor`, run on the remote machine as
they are. claudette's own commands start with `+`: `claude +doctor` checks ssh and the remote
machine, `claude +update` installs the latest release, and `claude +help` lists the commands and
settings. On a terminal, these say when a newer release is out. If `+` gets in your way, choose
another `prefix` in the [settings](configuration.md#configure).

To run claudette from T3 Code, Paseo, magpie or an Agent SDK program, see [hosts](hosts.md).

## MCP servers

MCP servers are set up on the laptop with `claude mcp`, which writes the same files a local claude
writes: `~/.claude.json` for user and local scope, the project's `.mcp.json` for project scope.
These servers run on the laptop.

```sh
claude mcp add --scope user fs -- npx @modelcontextprotocol/server-filesystem .
claude mcp add --scope project lint -- node tools/lint-mcp.js   # writes .mcp.json
claude mcp list                                                 # every server a session gets
claude mcp remove fs
```

Add `--remote` to run a server on the remote machine instead, for example one that needs to be near
the remote machine's network or files:

```sh
claude mcp add --remote --transport http docs https://example.com/mcp -H 'Authorization: Bearer …'
```

`claude mcp add-json` and `claude mcp get` work too. `${VAR}` in a server's settings is filled in
from the laptop's environment, for remote servers as well. A project's `.mcp.json` servers start once
you approve them, as in Claude Code.

## Plugins

`claude plugin install`, `claude plugin marketplace add` and the other plugin commands work on the
laptop's plugins, as with a local claude. Sessions load what's installed and enabled, and the
plugins' hooks and MCP servers run on the laptop.

## Sessions talking to each other

Sessions on the same laptop, whether in terminals or in hosts, can find each other with ListAgents
and message each other with SendMessage. Sessions on other laptops that use the same remote machine
don't see them.

## Things to watch out for

### Prefer a host to Claude Code's interface

Claude Code's interface is drawn on the remote machine and shown through ssh. Every keystroke and
redraw crosses the network, each start takes about a second longer than a local one
([latency](latency.md)), and a few of its features are missing: the tools show up as `mcp__cc__*`,
`/rewind` can't undo their edits, and background sessions are off. T3 Code and Paseo draw their
interface on the laptop and have none of these problems. Use the interface for quick checks.

The first time you open a project, Claude Code asks on the remote machine whether to trust the
project's folder there. That folder is your project, seen from the remote machine.

### Keep claudette running

A session belongs to the claudette process that started it. If the laptop sleeps or loses the
network, the session waits and picks up when it's back. If the claudette process itself ends, for
example because you closed the terminal, the session can't be picked up again, and the remote
claude stops after `detach_timeout` (12 hours by default). Background sessions (`--bg`,
`/bg`, `claude agents`) aren't available; run a long terminal session in tmux or screen on the laptop
instead.

### Your environment goes with you

Claude Code's own variables set on the laptop reach the remote claude, credentials and provider
settings included ([configuration](configuration.md#configure)). If your shell exports
`ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`, remote sessions use that credential instead of the
remote machine's login, with the billing that goes with it. Set `forward_claude_env = false` to
keep the remote machine's login whatever your shell has. The values travel privately and never show
up in the remote machine's process list.

### The log holds everything

The `log` setting writes every message between claudette and claude to a file on the laptop: your
prompts, file contents, and the values of forwarded variables. Turn it on to debug, and delete the
file afterwards.

### New files show up late in Claude Code's own reads

When you create, rename or delete a file on the laptop, it can take up to 10 seconds to appear in
what Claude Code reads by itself, such as settings, skills and `@` mentions. The tools always see
the current files.

The full list of differences from a local Claude Code is in [limitations](limitations.md).
