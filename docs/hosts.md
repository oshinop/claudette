# Hosts

claudette answers to `claude`'s arguments, so anything that launches `claude` can launch it instead.
A host finds it one of two ways:

- Through the `claude` link from [installation](configuration.md#install), when the host looks
  `claude` up on `PATH` and the link's folder is on the host's `PATH`. A desktop app started from
  the Dock or Finder may not get your shell's `PATH`. If its Claude provider finds no `claude`, or
  reports a version other than the remote machine's, use the binary setting instead.
- Through the host's own binary setting, set to claudette's absolute path. That works whatever the
  host's `PATH` holds, and it is what the tests use.

The host must run on the laptop, with your ssh agent, and with `host` in claudette's
[settings](configuration.md#configure) or `CLAUDETTE_HOST` in its environment. A host's phone app
connects to that laptop process, so the laptop has to stay awake while you use it.

## T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) drives Claude through its built-in Claude provider,
and with claudette behind it T3 works as usual: approvals, diffs, checkpoints, worktrees and thread
history all refer to files on your laptop, while Claude runs on the mini.

The provider runs `claude` by default (**Binary path** `claude`), so the link is enough. To set it
explicitly, put claudette's absolute path in **Binary path** under **Settings → Providers → Claude**,
and add `CLAUDETTE_HOST` to the provider's environment variables if the server lacks it. In
`~/.t3/userdata/settings.json` that is:

```json
{ "providerInstances": { "claudeAgent": { "driver": "claudeAgent", "enabled": true, "config": { "binaryPath": "/Users/me/.local/bin/claudette" } } } }
```

Start with the desktop app, or `t3 serve` in a terminal, before moving to the background service.

T3 also runs `claude -p` to write thread titles, commit messages and pull request descriptions. Those
runs go through claudette as well, and when T3 lets the model read the repository, it reads your
local checkout under the tool list and `git` rules T3 sets.

Tested against T3 Code 0.1.7: a thread's Bash, Write and Read run on the laptop, and the thread
continues after the T3 server restarts. Approvals were tested through the Agent SDK instead, since
threads started from T3's command line run in full-access mode.

## Paseo

[Paseo](https://paseo.sh) also drives Claude through a Claude provider. With the link, its built-in
provider, which runs `claude`, should reach claudette; that route is untested. The tested route is a
provider of its own whose `command` is claudette, which also gives it a name and a host. Merge this
into `~/.paseo/config.json` and run `paseo reload`:

```json
{
  "agents": {
    "providers": {
      "claude-mini": {
        "extends": "claude",
        "label": "Claude on Mac mini",
        "command": ["/Users/me/.local/bin/claudette"],
        "env": { "CLAUDETTE_HOST": "mini" }
      }
    }
  }
}
```

"Claude on Mac mini" then shows up as a provider with Claude's usual modes (plan, default,
acceptEdits, auto, bypassPermissions).

Tested against Paseo 0.15.3: an agent's commands and file reads run on the laptop, and the agent
continues after the daemon restarts. Paseo's approval prompts were not exercised.

## magpie

[magpie](https://usemagpie.ai) lends a Claude subscription to the other agents on the machine, such
as Codex, OpenCode and Gemini CLI, by driving the local `claude` binary. Each turn runs Claude Code
with magpie's MCP server as its only tools, and the agent's tool calls go back to the agent. magpie
uses the `claude` it finds on `PATH`, so with the link it drives claudette, and the subscription
signed in on the remote machine serves every agent on the laptop. `magpie accounts add claude` runs
`claude auth`, which claudette passes to the remote machine.

This is untested. From how the two work, expect:

- magpie's MCP server arrives in the run's `--mcp-config`, which claudette runs on the laptop, so it
  reaches magpie's gateway on `127.0.0.1` as it would without claudette.
- In its default interactive mode, magpie reads the session's transcript from `~/.claude/projects`
  on the laptop, which is where claudette writes sessions.
- Replies arrive one message at a time instead of streaming. magpie streams by loading a script into
  Claude Code through `BUN_OPTIONS`, and that variable and the script stay on the laptop, so magpie
  falls back to whole messages, as it does for any Claude Code that doesn't load the script.
- Only the remote machine's login is used. Saved accounts with their own `CLAUDE_CONFIG_DIR` don't
  carry over, since a laptop path means nothing on the remote machine, which leaves magpie's
  failover between Claude accounts with one account.

## ACP clients

Zed, JetBrains AI Assistant, [Toad](https://github.com/batrachianai/toad),
[acpc](https://github.com/AkaraChen/acpc), acpx and other clients of the
[Agent Client Protocol](https://agentclientprotocol.com) run Claude through the
[claude-agent-acp](https://github.com/agentclientprotocol/claude-agent-acp) adapter, which drives
Claude Code through the Agent SDK. The adapter runs the binary named in `CLAUDE_CODE_EXECUTABLE`
when that is set, so pointing it at claudette moves Claude to the remote machine while its tools act
on the laptop's files. In Zed, add a custom agent to `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "Claude on mini": {
      "type": "custom",
      "command": "npx",
      "args": ["-y", "@agentclientprotocol/claude-agent-acp"],
      "env": { "CLAUDE_CODE_EXECUTABLE": "/Users/me/.local/bin/claudette", "CLAUDETTE_HOST": "mini" }
    }
  }
}
```

In acpc, add an agent to `~/.config/acpc/config.toml`:

```toml
[[agents]]
slug = "claude-mini"
name = "Claude on mini"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
env = { CLAUDE_CODE_EXECUTABLE = "/Users/me/.local/bin/claudette", CLAUDETTE_HOST = "mini" }
```

The adapter's standalone executable, from its releases, can be the `command` instead and needs no
Node.js. Other clients take the same command and environment in their agent settings.

This is untested. From how the adapter works, expect:

- It starts Claude Code with the full built-in tool set, which claudette replaces with the laptop's
  tools, so edits land in the editor's checkout and the editor's diffs show laptop paths, as T3
  Code's do.
- The editor's MCP servers arrive in the run's MCP configuration and run on the laptop, as an SDK
  host's do.
- The adapter lists sessions from `~/.claude/projects` on the laptop, which is where claudette
  writes them, so the editor's thread history shows the same sessions as `claude --resume`.

## Other Agent SDK hosts

Any program built on the Claude Agent SDK can use claudette by setting the executable:

```ts
query({ prompt, options: { pathToClaudeCodeExecutable: "/Users/me/.local/bin/claudette", cwd } });
```

## Untested control planes

These should work the same way, but none has been run against claudette yet:

| Control plane | How it would use claudette |
|---|---|
| [Vibe Kanban](https://github.com/BloopAI/vibe-kanban) | Its Claude Code executor speaks the same stream-json protocol, but implements it itself rather than through the Agent SDK, so test it first. Set the executor's `base_command_override` to claudette. |
| [Claude Code UI](https://github.com/siteboon/claudecodeui) | Set `CLAUDE_CLI_PATH` to claudette. |
