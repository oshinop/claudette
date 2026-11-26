# Agent instructions

claudette is a drop-in `claude`: Claude Code runs on a remote machine reached over ssh, and its
tools run on the local machine. Files, builds, git and configuration stay local; the login, Claude
Code's internal state and its telemetry stay on the remote machine.

## Stock Claude Code is the spec

- Judge every change by one question: does a user see what a local Claude Code would show and do?
  Anything that launches `claude` must launch claudette unchanged.
- Reuse Claude Code's own stores, paths, file formats and commands: its plugin store, settings
  files, `.mcp.json`, `claude plugin`, `claude mcp`. `config.toml` holds only claudette's own
  settings.
- Before designing a mechanism, look for the one Claude Code already has and use it.
- Use ssh's own keepalive, retries and connection sharing.
- claudette prints nothing into Claude Code's interface while it runs. Its own notices appear only
  in its own commands (`claudette +help`).

## Claude Code's rendering

Don't make claudette print its own imitation of Claude Code's output: the layout, wording, symbols
or statuses of Claude Code's commands and interface. Where Claude Code can render something itself,
pass it through. Imitate it only when there is an explicit reason or a limitation that leaves no
other way, such as `claude mcp`, which claudette has to run on the local machine.

Adding an imitation, or changing an existing one, needs the maintainer's agreement: name the reason
or limitation that requires it in the issue or pull request.

## Where things run

- A thing runs where the thing it acts on lives. [Design](docs/design.md) says where each tool,
  hook and server runs.
- Only settings the user configured explicitly cross to the remote machine, never local internal
  state or credentials, and they cross as the user set them.
- Several local machines can share one remote machine: each keeps its own configuration, while the
  login and history are shared on the remote.

## Privacy

- The local machine sends no telemetry and nothing that identifies it, and claudette sends none
  itself.
- The remote claude sees no trace of the ssh session or of claudette.
- Traffic that leaves on behalf of the remote machine exits through it. The link browser fails
  closed: if a browser version can't route all of its traffic (DNS, WebRTC and the rest) through the
  remote machine, it isn't used. Its fingerprint is per remote machine and carries nothing
  specific to claudette.

## Moving parts

- Prefer no new files or persistent state. Derive identity from what the OS already provides, and
  keep transient files in temporary locations.

## Code

- No comments, doc comments included. Names, types and structure carry the meaning: name a timeout
  constant for its purpose. A script keeps its usage header, and directives the toolchain requires
  stay.
- Check input once, where it enters claudette: arguments, `config.toml`, Claude Code's settings and
  messages, frames from the other machine. Reject bad input there with an error naming what is wrong;
  past that point, types carry the guarantee. Parsing stays pure, apart from the code that does I/O.
- Give an error path only to failures that can happen, and wrap an error only where its meaning
  changes.
- Define each fact once (names, limits, paths, protocol shapes) and derive the rest from it.
- Add a trait or layer only when it has more than one real implementation.
- Use a mature crate before writing your own, and keep claudette's limits, retry rules and timing
  explicit in its own code.
- Delete what a change makes obsolete, along with its tests and docs.

## Docs

- Say "local machine" and "remote machine", not anything else.
- The README lists the main selling points once. Details go in `docs/usage.md`, settings in
  `docs/configuration.md`, internals in `docs/design.md`, test notes in `docs/testing.md`.
- `docs/limitations.md` lists only practical differences from stock Claude Code, from the user's
  side.
- `docs/hosts.md` recommends hosts expected to work.
- Keep help text short and consistent across commands.
- Every doc change updates its Chinese version (`docs/zh-CN/`, `README_zh-CN.md`). In Chinese,
  agentic terms stay in English: tools, agent, skills, MCP.

## Tests

Unit tests sit beside the code. The end-to-end suites, their harnesses and fixtures live in
`tests/`; they drive the real Claude Code in containers against a scripted API, and T3 Code and
Paseo against claudette ([testing](docs/testing.md)). The README's Contributing section lists what
a pull request needs.

- Write tests once the change works when run for real, through the playground
  (`scripts/playground.sh`) or the end-to-end suites, not before.
- A test protects behavior a user relies on, a bug that happened, or an invariant that isn't
  obvious. It asserts the outcome: what lands on the local machine, what Claude Code received, the
  exit status.
- Test limits just before and at the limit, with controlled time.
- When a change alters behavior on purpose, update the tests that pin the old behavior. A test is
  never weakened, skipped or deleted to make it pass.
- Changes to startup are measured on a simulated 500 ms round trip at 10 Mbit/s, where `claude -p`
  must exit in under 5 s ([latency](docs/latency.md)).

## Releases

Bump the version in `Cargo.toml` and `Cargo.lock`, commit `chore: release X.Y.Z`, wait for CI, then
push the tag `vX.Y.Z`. Before 1.0, a release that adds features or changes behavior users may rely
on bumps the minor version; fixes alone bump the patch.
