# Testing

```sh
scripts/test.sh
```

Everything runs in containers (`tests/docker/compose.yaml`) on an internal network without internet access.
The end-to-end suites are their own Bun package in `tests/`, which the laptop image installs at `/app`:

- `mini` runs sshd, sshfs, bubblewrap and the real Claude Code CLI, which talks to `mockapi`.
- `mockapi` is a scripted Anthropic Messages API (`tests/mock-api/server.ts`). Tests register a
  conversation, made of tool calls and then text that can quote the last tool result, and check every
  request claude made, including its tool list and system prompt.
- `laptop` holds claudette, built from this crate, plus Bun, the Agent SDK, ssh and git for the
  tests. The Agent SDK's bundled `claude` is left out, and the image build fails if any `claude`
  binary is present, so every tool call has to run here. There is no ripgrep either: Grep is built in.
- `t3` and `paseo` are the laptop with the real T3 Code server (0.1.7) or Paseo daemon (0.15.3),
  driven through their command-line clients.

| Suite | Covers |
|---|---|
| `cargo test` | argument rewriting and the caller's tool restrictions; ssh command quoting and cleanup, run through a real `sh`; the environment allowlist; permission rules; the relay's rewriting; the socket servers; each local tool; the SFTP servers' containment and write rules, and the sessions folder's path rewriting; a mount's warm start as its server answers it; the near end in front of a real sessions server: lookups answered from what it knows until a change drops them, transcripts written behind ending up on the laptop exactly as the laptop writes them, new or resumed, a transcript larger than one read fetched whole though it grew, and one whose open the laptop refuses not kept; the settings view and its round trip, exec-form hooks run without a shell, CwdChanged and FileChanged fired here and `CLAUDE_ENV_FILE` reaching the local Bash; plugins: which installation, their MCP servers named with their options filled in, the options' sources, their manifest's view, the plugin store's path translation; MCP configuration and approval; folder names and path mapping; sessions finding each other without a registry, addressing by name and id, idle notices, removing a dead run's folder |
| `tests/e2e/session` | Agent SDK `query()` with T3-style options, through claudette and ssh to the real claude: tool routing, the built-in allowlist, permission modes and prefix rules, denials, plan mode, http, stdio and in-process MCP servers, a remote MCP server across `setMcpServers`, transcripts written to the laptop through the sessions folder, resume, a fork that stays on the laptop, interrupt, `--add-dir`, a project skill, a saved "always allow", project hooks run on the laptop with a readable `transcript_path`, the user's skills, agents, settings and hooks delivered while their login and credentials stay behind, an installed laptop plugin on by default with its skill (its options filled in), hook and MCP server run on the laptop, a user-level "always allow" saved on the laptop, `Read(…)`/`Edit(…)` deny and ask rules against bare allows, settings `env` in laptop commands, `additionalDirectories`, the remote user's configuration kept out, passthrough, doctor |
| `tests/e2e/direct` | `claudette -p`: tools on the laptop, acceptEdits through the hook, no edits without approval, T3 Code's repository-reading text generation, `--tools ""`, `--mcp-config` servers and their prompts as slash commands, `.mcp.json` approval, a remote server with its secret kept off the command line, the remote machine's own servers kept out, `claude mcp add/list/remove` writing `~/.claude.json` and `.mcp.json` (an unparsable `~/.claude.json` left alone), `claude plugin marketplace add` and `install` writing the laptop's plugin store and settings with laptop paths, the next session loading the plugin, the sandbox, resuming a session an SDK host started, `--continue`, the mini's earlier sessions moved to the laptop, cleanup |
| `tests/e2e/agents` | ListAgents and SendMessage between this laptop's sessions: the local versions replace Claude Code's, the listing shows projects and status, a message starts a turn in an idle host session marked as from another session, `notify_when_idle`, a session in another runtime folder and one run on the mini itself are not listed; background-session commands and `--bg` refused, agent teams and the mini's own agent view forced off |
| `tests/e2e/interactive` | Claude Code's interactive interface in a pseudo-terminal: the trust prompt, an edit and a command on the laptop, exit, cleanup, and Claude Code's own `/resume` list showing that session beside one a local claude wrote before the switch; `/theme` and a permission rule saved to user settings during a terminal session landing in the laptop's `~/.claude/settings.json` under the built-in name, beside the laptop's own keys, with the remote machine's own settings untouched; `←` and `/bg` starting no background service on the mini |
| `tests/e2e/helper` | the helper copied to the mini once and reused, nothing of a run left behind; a first run that copies the helper while carrying a 3 MB resumed transcript; a host session whose connection is cut mid-tool-call finishing the call and the reply; a `-p` run stopped (SIGSTOP) with its link cut, resumed; a session whose laptop never comes back ending on the mini after the detach timeout and cleaning up; a message reaching a session that skips permission prompts |
| `tests/e2e/fixes` | the Cron tools offered; a host's session-scoped "always allow" covering a tool's other paths while an ask rule still holds |
| `tests/t3` | T3 Code runs a thread whose Bash, Write and Read run on the laptop, and resumes it after a server restart |
| `tests/paseo` | Paseo runs an agent whose commands and file reads run on the laptop, and resumes it after a daemon restart |

Not covered yet:

- A macOS remote in a whole session. Its `sandbox-exec` profile has been checked on its own (the
  Keychain login, Claude Code's files, the closed home folder), but sshfs on macOS (macFUSE or FUSE-T)
  hasn't been run.
- `CLAUDE_SECURESTORAGE_CONFIG_DIR`, which Claude Code doesn't document, is what keeps the login shared
  with the run's own Claude Code folder. It is checked on Linux with 2.1.319; on macOS its effect is
  read from Claude Code's code, not tested.

## Playground

`scripts/playground.sh` gives you a remote machine to try claudette against by hand: the test image's
`remote` container on `127.0.0.1:2222`, with internet access. `up` builds claudette for this machine
and the container, starts it and sets up ssh with a config of its own in `target/playground`, so
`~/.ssh` is neither read nor changed. Building the Linux binary needs zig and cargo-zigbuild (see
`scripts/build.sh`).

```sh
scripts/playground.sh up
scripts/playground.sh login                    # once: Claude Code's login, kept in a volume
eval "$(scripts/playground.sh env)"            # point claudette at it, in this shell
eval "$(scripts/playground.sh env mock)"       # or: the same against the mock API, no login needed
cd ~/some/project && claudette -p "hi"
```

`env` prints `CLAUDETTE_HOST`, `CLAUDETTE_SSH` and an alias for the `claudette` it built; `env mock`
adds the variables that send Claude Code to the mock API. With the mock, `scenario <id> '<steps>'`
scripts its replies for prompts that contain `[[scenario:<id>]]`: each step is a tool call
(`{"tool": "Bash", "input": {…}}`) or a text, which may quote `{{last_tool_result}}`. `ssh` opens a
shell in the container, and `down` stops it (`down -v` also deletes the login and the ssh key).
