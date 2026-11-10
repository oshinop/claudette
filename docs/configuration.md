# Installation and configuration

## Requirements

The remote machine needs:

- Claude Code (tested with 2.1.319), logged in, and a POSIX shell;
- sshd, with no forwarding enabled, since everything travels on the session's own channel;
- `nc` with `-U`, which macOS ships;
- `sshfs`, to mount the project: the `sshfs` package (FUSE 3) on Linux, or sshfs from
  [macFUSE](https://macfuse.github.io) or [FUSE-T](https://www.fuse-t.org) on macOS;
- a sandbox, because claude always runs in one: bubblewrap (`bwrap`) on Linux, the built-in
  `sandbox-exec` on macOS.

Nothing else is installed there. claudette copies itself to `~/.claudette/bin` on first use (see
[design](design.md#the-helper)).

The laptop needs ssh that reaches the remote machine without a prompt. Keys, users and ports go in
`~/.ssh/config`, and a key with a passphrase belongs in an agent. Run `ssh mini` once in a terminal
to accept the host key, then check with `ssh -o BatchMode=yes mini claude --version`.

ssh may prompt only on the first connection of a run started in a terminal. Runs without a terminal
(hosts, scripts, daemons) and every reconnect add `BatchMode=yes`, so they fail at once instead of
waiting on a prompt nobody sees. That also means password login works for that first terminal
connection and nothing else.

`claudette +doctor` checks all of the above.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/oshinop/claudette/main/scripts/install.sh | CLAUDETTE_HOST=mini sh
```

The script downloads every platform's build from the latest GitHub release, checks them against the
release's `SHA256SUMS`, puts them in `~/.local/share/claudette` and links `~/.local/bin/claudette` to
this machine's. claudette copies the remote machine's build from beside its own, so a Mac laptop
driving a Linux machine uses `claudette-linux-arm64` or `-x64` from that folder.
`CLAUDETTE_VERSION` picks a release (`v0.1.0`), and `CLAUDETTE_BIN_DIR` and `CLAUDETTE_LIB_DIR`
change the folders.

Any [setting](#configure) given as a variable, such as `CLAUDETTE_HOST` or `CLAUDETTE_DETACH_TIMEOUT`,
goes into a new `~/.config/claudette/config.toml`; an existing one is left alone. Put the variables
on `sh`, after the pipe: on `curl`, they never reach the script.

### Update

```sh
claudette +update            # the latest release
claudette +update 0.1.0      # a given one, older too
```

`+update` downloads every platform's build of the release, checks them against its `SHA256SUMS`,
makes sure this machine's build runs, and only then renames them over the ones beside the running
claudette. A failure leaves the installed builds as they were. It works for an install the script
made (builds named `claudette-<platform>`), and `CLAUDETTE_DOWNLOAD_URL` points it at a mirror, as
for the script. Running the install script again does the same.

Sessions already running keep the build they started with, and finish on it. The remote machine
gets the new build on the next session (see [design](design.md#the-helper)); nothing needs doing
there.

claudette's own commands (`+help`, `+version`, `+doctor`), run on a terminal, end with a line when a
newer release is out. Each asks GitHub while it runs, waits at most 2 seconds for the answer, and
keeps nothing on disk. Nothing else claudette runs, `claude` sessions and scripts included, looks
for releases.

To build instead, you need Rust through rustup, zig and cargo-zigbuild (see the
[README](../README.md#development)):

```sh
scripts/build.sh     # dist/claudette-{darwin-arm64,darwin-x64,linux-arm64,linux-x64}
```

The Linux binaries are static (musl) and linked by zig. The macOS ones use the Apple toolchain, so
they need a Mac. Keep the builds together in one folder, as the install script does.

To make it your `claude`, link it under that name in a folder that comes before any other `claude`
on `PATH`. claudette follows the link to find its other builds, and it never runs a local `claude`.
The laptop doesn't need Claude Code installed. If a native install stays, put the link in an earlier
folder: its updater rewrites `~/.local/bin/claude`.

```sh
ln -s ~/.local/bin/claudette ~/bin/claude
```

## Configure

Settings live in `~/.config/claudette/config.toml`. Only `host` is required:

```toml
host = "mini"                                  # the remote machine's ssh name
# ssh = ["ssh", "-o", "ProxyJump=bastion"]     # instead of the default ssh, which shares one connection
# detach_timeout = "30m"
# forward_env = ["AWS_REGION", "AWS_PROFILE"]
# forward_claude_env = false
# log = "/tmp/claudette.log"
# prefix = "::"
```

Then run `claudette +doctor`. Each setting can also be set as a `CLAUDETTE_` variable with the key in
capitals, such as `CLAUDETTE_HOST` or `CLAUDETTE_DETACH_TIMEOUT`, which wins over the file, for one
shell or one host's provider settings. A variable takes the same values as a string: words separated
by spaces for `ssh`, by commas for `forward_env`, and `1` or `0` for `forward_claude_env`. claudette
refuses a key it doesn't know, so a typo doesn't go unnoticed.

| Setting | Meaning |
|---|---|
| `host` | the remote machine, as ssh takes it: an alias from `~/.ssh/config`, `user@host`, or an `ssh://user@host:port` URL. A URL carries no key or other options; those go in `~/.ssh/config` or the `ssh` setting. |
| `ssh` | ssh command and options, as a list or a string. By default, sessions share one connection with keepalives (`ControlMaster`, kept 10 minutes, its socket in the private folder `/tmp/claudette-<uid>`), and a session reconnecting after a drop opens its own. These options come before claudette's and ssh keeps the first value it sees, so `-o BatchMode=no` here overrides claudette's; a `BatchMode` line in `~/.ssh/config` does not. |
| `detach_timeout` | how long the remote machine keeps a session waiting for the laptop to reconnect before it stops the remote claude: seconds, or `"90s"`, `"30m"`, `"12h"` (default `"12h"`) |
| `remote_claude` | claude command on the remote machine (default `claude`; `~/.local/bin` is on its PATH) |
| `remote_nc` | Unix-socket relay on the remote machine, run as `<cmd> -U <path>` (default `nc`) |
| `forward_claude_env` | whether Claude Code's own variables set on this machine reach the remote claude (default `true`). These are the `ANTHROPIC_*`, `CLAUDE_*`, `DISABLE_*`, `ENABLE_*`, `MCP_*` and `VERTEX_REGION_*` variables, Claude Code's others such as `MAX_THINKING_TOKENS`, `API_TIMEOUT_MS` and `DO_NOT_TRACK`, and the credentials it reads: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN` (with its refresh token and scopes), the Foundry, AWS and Bedrock keys and `MCP_CLIENT_SECRET`. Provider and endpoint settings (`ANTHROPIC_BASE_URL`, `CLAUDE_CODE_USE_*`, Bedrock, Vertex and Foundry settings) go as set, so the remote claude uses the provider, endpoint and credential you set here, as a local claude would; the endpoint has to be reachable from the remote machine. Held back: the variables that name a file or folder on this machine (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_TMPDIR`, the mTLS key and certificate, …), what a claude on this machine sets for its own children (`CLAUDE_CODE_SESSION_ID`, its messaging token, …), and other keys and tokens in those families, such as an `MCP_*_TOKEN` for a server. The `env` of your settings files is passed the same way, except its credentials, which stay here with the rest of this machine's login (`apiKeyHelper` and the like). |
| `forward_env` | more variables to pass to the remote claude, whatever they are and whether or not `forward_claude_env` is on: `["AWS_REGION", "AWS_PROFILE"]` for Bedrock, or `["GITHUB_TOKEN"]`, for example. Forwarded values never appear on the remote command line, where other users of the remote machine could read them (see [design](design.md#the-helper)). |
| `log` | file to append every relayed frame to, and every request the remote claude makes through the mounts. It contains prompts, file contents and the forwarded variables' values. |
| `prefix` | what marks claudette's own commands (default `+`: `claudette +doctor`, `+update`, `+version`, `+help`). Any first argument that starts with it goes to claudette, so pick something no prompt starts with. A prefix starting with `-` is refused. |

### Plugins

Plugins are this machine's: `claude plugin install`, `claude plugin marketplace add` and the rest
run Claude Code's own commands on `~/.claude/plugins` and your settings here, as a local claude
would, and sessions load what is installed and enabled. Their hooks and MCP servers run on the
laptop, with the options you gave them.
