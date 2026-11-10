# Startup latency

English · [简体中文](zh-CN/latency.md)

A session starts with an ssh exchange, the mounts (the project, its sessions folder, your user content
and plugins, the Claude Code folder) and claude's own startup. On a slow link each request claude
waits on is a round trip to the local machine, one after another, so the design keeps claude from waiting on
the local machine at all where it can:

- **The near end of each mount** (see [design](design.md#the-near-end-of-a-mount)). What claude reads
  first from a mount comes with the run, answered from the local machine's own server, so the mounts come up
  and claude starts without a round trip. Lookups and listings are cached for 10 s, and absent names
  are answered from complete listings. Transcripts are written behind: claude's opens, writes and
  reads of its own transcript never wait on the local machine.
- **Everything the run needs, in its first frames.** The session's settings, appended prompt, MCP
  configuration, the local tools' list and the mounts' warm starts travel ahead of the start, and the
  first connection sends them right behind its greeting instead of after the helper's.
- **Handshakes answered on the remote machine.** The local tools' MCP handshake (`initialize`,
  `tools/list`, the `server/discover` probe), claudette's status hooks, which claude no longer waits
  on, and the permission decision for a Bash call, which rests on the session's Bash rules alone.
- **The shared ssh connection.** Sessions open their channel on one shared connection (`ControlMaster`,
  kept 10 minutes) instead of a new one, and everything a session carries rides that channel through
  the helper.
- **The cached helper.** The first session copies claudette's binary to the remote machine (about
  6.5 MB, once per version); later sessions find it in `~/.claudette/bin`.

What still costs round trips: opening the run's channel (ssh starts the remote command, then sends
its stdin a round trip later), each local tool call, an edit's permission decision (path rules
resolve the local machine's files), reads beyond the warm start, and the end of a session, which waits for
the local machine to confirm the transcript's last writes.

## Measured

On one machine: claudette on macOS against the playground container (`tests/docker/playground.yaml`)
over ssh on 127.0.0.1, Claude Code talking to the mock API with `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`,
a clean home folder, medians of five to seven runs. "Plain claude" runs the same Claude Code on the
container over the same shared ssh connection, without claudette.

| | Plain claude | claudette |
|---|---|---|
| `claude -p hi`, to exit | 0.18 s | 0.53 s |
| `claude -p` with 10 Bash calls, to exit | 0.29 s | 0.76 s |
| stream-json session: start to `initialize` answered | 0.12 s | 0.22 s |
| stream-json session: a turn without tools | 0.01 to 0.05 s | 0.01 to 0.06 s |
| stream-json session: a turn with 10 Bash calls | 0.12 s | 0.22 s |
| interface: first screen | 0.19 s | 0.33 s |
| interface: keystroke to redraw | 16 ms | 7 ms |

Claude Code's own background traffic costs more than claudette does. On a machine with internet
access, plain `claude -p hi` takes about 1.1 s with it and 0.18 s without; telemetry is most of it
(`DISABLE_TELEMETRY=1` alone brings it to 0.5 s). claudette passes Claude Code's variables to the
remote claude, so `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` set on the local machine applies to every
session.

### On a slow link: 500 ms round trip, 10 Mbit/s

The same setup with every byte to the remote machine held 250 ms each way and the link limited to
10 Mbit/s (an ssh `ProxyCommand` relay), medians of three to five runs. Opening a command over the
shared connection and seeing it exit takes two round trips, 1.04 s. "Before" is claudette with plain
relays under its mounts and every file, handshake and hook fetched or answered over the link.

| | Plain claude | Before | claudette |
|---|---|---|---|
| `claude -p hi`, to exit | 1.67 s | 24.2 s | 3.05 s |
| `claude -p hi` with one user skill, to exit | | | 3.13 s |
| `claude -p` with 3 Bash calls, to exit | 1.71 s | 28.5 s | 4.74 s |
| stream-json session: start to `initialize` answered | 1.58 s | 7.4 s | 2.15 s |
| stream-json session: first turn without tools | 0.58 s | 9.2 s | 0.69 s |
| stream-json session: later turn without tools | 0.54 s | 1.6 s | 0.55 s |
| stream-json session: a turn with 3 Bash calls | 0.59 s | 4.7 s | 2.19 s |
| stream-json session: closing | 0.54 s | 6.4 s | 1.30 s |
| interface: first screen | 1.19 s | 6.9 s | 1.83 s |
| interface: keystroke to redraw | 534 ms | 513 ms | 527 ms |

The first run of a new version also copies the helper over, about 10 s at this speed.

Of the 3.05 s for `-p hi`:

- 0.8 s for ssh to open the run's channel and start the helper, as for any command.
- 0.5 s until the run's frames reach the helper: ssh sends a command's stdin only once the command
  has started.
- 20 ms of setup on the remote machine: the mounts come up from their warm starts.
- 0.7 s of claude's startup and the turn, as without claudette; its answer reaches the local machine at
  about 2.4 s.
- 0.55 s waiting for the local machine to confirm the transcript's last writes, then the exit code's way
  back and the goodbye.

Each local tool call costs one round trip, the call itself: a Bash call's permission is decided on
the remote machine, and its transcript lines are written behind. An edit costs one more, for its
permission decision on the local machine.

## Not done

| Option | Would save | Cost |
|---|---|---|
| The local machine drains its servers after the exit code instead of the remote side waiting for the last transcript writes to be confirmed | ~0.5 s at the end of every session | The holder would have to send every channel's data before the exit code, and the local machine wait for its own servers to process it; today the order is guaranteed by the confirmation. |
| Answer bridged MCP servers' handshakes on the remote machine, as for the local tools | 3 round trips per server from the local machine's configuration | Each server would have to start and list its tools before the run starts. |
| A long-lived agent on the local machine keeping the connection open per project between sessions | The round trip of opening a channel, and the one before ssh passes stdin | A daemon to install and supervise. |
