# Limitations

What works differently from Claude Code running on your laptop, and what doesn't work at all.

## Where it runs

- The laptop can be macOS or Linux. Windows isn't supported.
- The remote machine can be Linux or macOS. Linux remotes are the supported setup; a Mac remote should
  work but hasn't been verified in whole sessions yet.
- The remote machine needs Claude Code 2.1.319 or later. `claudette +doctor` warns about an older
  one.

## Features that are off

- LSP, Monitor, the worktree tools and agent teams aren't available. Hosts that make their own
  worktrees, as T3 Code and Paseo do, aren't affected.
- Background sessions are refused: `claude agents`, `--bg`, `/bg`, and `←` on an empty prompt.
  Claude Code's background service would run them on the remote machine outside the sandbox. To keep
  a terminal session going without its window, run it in tmux or screen on the laptop.
- Claude can't pick a finished subagent back up by name. It starts a new one instead.

## In Claude Code's own interface

Hosts such as T3 Code and Paseo don't have these; they apply when you run `claude` in a terminal.

- The interface is drawn on the remote machine, so every keystroke and redraw crosses the network
  (see [usage](usage.md#prefer-a-host-to-claude-codes-interface)).
- The tools that run on your laptop show up as `mcp__cc__Read`, `mcp__cc__Bash` and so on.
- `/rewind` can't undo the laptop tools' edits. Use git for that.
- If you start the interface with a prompt (`claude "fix it"`), the first turn gets the laptop tools
  only through tool search, as deferred tools. Typing the prompt into the interface instead avoids
  this; setting `alwaysLoad` on the server or `MCP_CONNECTION_NONBLOCKING=0` doesn't.
- `/mcp` lists a server called `claudette-inbox`. It has no tools; it delivers messages from your
  other sessions.

## Your configuration

- Your settings, keybindings, permission rules, hooks, skills, agents, commands, output styles,
  `CLAUDE.md` files, MCP servers and enabled plugins all come from the laptop. The rest of
  `~/.claude`, and the preferences in `~/.claude.json`, aren't used.
- Onboarding and the "trust this folder" prompt belong to the remote machine. You answer them there
  once, and every laptop using that machine shares the answer.
- The remote machine's own plugins and MCP servers are never loaded.
- If a session changes your settings or keybindings, the change reaches your next session, not the
  one that's running.

## Plugins

- A plugin's MCP bundles (`.mcpb`, `.dxt`, or a URL) and its monitors run on the remote machine. Its
  LSP servers stay off, like the LSP tool.
- A sensitive plugin option you set through `claude plugin` isn't remembered.
- If the remote machine's managed settings forbid `--plugin-dir`, no plugins load.

## MCP servers

- Servers that run on the laptop can't ask the client for anything: sampling, elicitation and roots
  are refused, and they can't complete an OAuth login.
- In SDK hosts such as T3 Code, those servers' prompts aren't offered as slash commands. Terminal
  and `-p` sessions do offer them.
- Servers you add with `--remote` run on the remote machine inside the sandbox. They see the project
  and Claude Code's own state, and nothing else in the remote home folder.

## Sessions talking to each other

- ListAgents and SendMessage reach the claudette sessions on the same laptop, and only those.
- Sessions started directly on the remote machine can still message yours. If the target session
  skips permission prompts, such a message waits for approval; a host or `-p` session can't show the
  approval, so the message expires after five minutes.

## Connection and speed

- Each local tool call makes a round trip to the remote machine, and each session start takes a
  round trip or two longer than a local one ([latency](latency.md)). A direct connection, such as
  Tailscale between the two machines, keeps this short.
- While the laptop sleeps or is offline, the session waits on the remote machine and carries on when
  the laptop is back.
- A session belongs to the claudette process that started it. If that process ends (a crash, a
  closed terminal, a restart), no other process can take the session over, and the remote claude
  stops after `detach_timeout`, 12 hours by default.
- A file you create, rename or delete on the laptop can take up to 10 s to show up in what Claude
  Code reads by itself: settings, skills and `@` mentions. The tools always see the current files.
- If the remote machine loses its view of your project mid-session, claudette says so, but claude
  keeps going without the project, and the session's transcript on the laptop stops at that point.

## Smaller differences

- On a Mac remote, a second session in a project that already has one works in a folder next to the
  usual one. Its transcripts still go to the same place on the laptop.
- Background Bash commands write to a file that the model reads. Hosts that show Claude Code's own
  background-task output live, as T3 Code does, see only the final result.
- Claude Code's temporary files on the remote machine are deleted when the session ends.
