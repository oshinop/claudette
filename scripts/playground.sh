#!/bin/sh
# A remote machine to try claudette against by hand: the test image's `remote` container on
# 127.0.0.1:2222 (tests/docker/playground.yaml), with internet access and its own Claude Code login.
#   scripts/playground.sh up              build claudette for this machine and the container, start
#                                         the container, set up ssh to it
#   scripts/playground.sh login           sign Claude Code in, in the container (kept in a volume)
#   eval "$(scripts/playground.sh env)"   point claudette at the container, with that login
#   eval "$(scripts/playground.sh env mock)"
#                                         the same, with Claude Code talking to the mock API instead
#   scripts/playground.sh scenario <id> '<steps>'
#                                         script the mock API: put [[scenario:<id>]] in a prompt and
#                                         each model request plays the next step, a tool call or a text
#                                         that may quote {{last_tool_result}} (tests/mock-api)
#   scripts/playground.sh ssh [command]   a shell, or a command, in the container
#   scripts/playground.sh down [-v]       stop it; -v also deletes the login and the ssh key
# ssh to the container uses its own config in target/playground; ~/.ssh is not read or changed.
set -eu
# The script names its own files in full, so it works from any folder.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export COMPOSE_FILE="$ROOT/tests/docker/compose.yaml:$ROOT/tests/docker/playground.yaml"
STATE="$ROOT/target/playground"
# ssh's connection-sharing socket path must fit in 104 bytes, so it gets a short folder of its own.
CONTROL="/tmp/claudette-playground-$(id -u)"

die() { echo "playground: $*" >&2; exit 1; }

platform() {
  case "$1-$2" in
    Darwin-arm64) echo darwin-arm64 ;;
    Darwin-x86_64) echo darwin-x64 ;;
    Linux-aarch64 | Linux-arm64) echo linux-arm64 ;;
    Linux-x86_64) echo linux-x64 ;;
    *) die "no claudette build for $1 $2" ;;
  esac
}
HERE=$(platform "$(uname -s)" "$(uname -m)")
CLAUDETTE="$ROOT/dist/claudette-$HERE"

# CLAUDETTE_SSH is split on spaces, so the config's path can't hold one.
case "$STATE" in *[[:space:]]*) die "the repository's path has a space in it, which CLAUDETTE_SSH can't carry" ;; esac
SSH="ssh -F $STATE/ssh_config"

ready() {
  [ -x "$CLAUDETTE" ] && [ -f "$STATE/ssh_config" ] || die "not set up: run scripts/playground.sh up"
}

up() {
  command -v docker >/dev/null || die "needs docker"
  case "$(docker version --format '{{.Server.Arch}}')" in
    arm64 | aarch64) there=linux-arm64 ;;
    amd64 | x86_64) there=linux-x64 ;;
    *) die "no claudette build for the container's architecture" ;;
  esac
  if [ "$there" = "$HERE" ]; then "$ROOT/scripts/build.sh" "$HERE"; else "$ROOT/scripts/build.sh" "$HERE" "$there"; fi

  docker compose up --detach --build --wait remote

  mkdir -p "$STATE"
  mkdir -p -m 700 "$CONTROL"
  [ -O "$CONTROL" ] || die "$CONTROL belongs to another user"
  chmod 700 "$CONTROL"
  docker compose cp remote:/keys/id_ed25519 "$STATE/id_ed25519"
  chmod 600 "$STATE/id_ed25519"
  # A rebuilt container has new host keys; the old ones would fail the check.
  rm -f "$STATE/known_hosts"
  cat >"$STATE/ssh_config" <<EOF
Host playground
  HostName 127.0.0.1
  Port 2222
  User remote
  IdentityFile "$STATE/id_ed25519"
  IdentitiesOnly yes
  IdentityAgent none
  UserKnownHostsFile "$STATE/known_hosts"
  StrictHostKeyChecking accept-new
  ControlMaster auto
  ControlPath $CONTROL/%C
  ControlPersist 10m
EOF
  $SSH playground true || die "ssh to the container failed"

  if docker compose exec -T -u remote remote claude auth status >/dev/null 2>&1; then
    login="Claude Code is signed in."
  else
    login="Claude Code is not signed in yet: scripts/playground.sh login (or use mock)."
  fi
  cat <<EOF

The playground is up. $login
  eval "\$(scripts/playground.sh env)"        point claudette at it, with the container's login
  eval "\$(scripts/playground.sh env mock)"   the same, with the mock API
EOF
}

case "${1:-}" in
  up) up ;;
  login)
    docker compose exec -it -u remote remote claude auth login ;;
  scenario)
    [ "$#" -eq 3 ] || die "usage: scripts/playground.sh scenario <id> '<steps json>'"
    # The mock API is on the containers' network only, so the request goes from inside.
    docker compose exec -T -e ID="$2" -e STEPS="$3" remote node -e '
      const fail = (message) => { console.error(`playground: ${message}`); process.exit(1); };
      let steps;
      try { steps = JSON.parse(process.env.STEPS); } catch (e) { fail(`the steps are not JSON: ${e.message}`); }
      fetch("http://mockapi:8080/_scenario", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id: process.env.ID, steps }) })
        .then((r) => { if (!r.ok) throw new Error(`the mock API answered ${r.status}`); console.log(`scenario ${process.env.ID}: ${steps.length} steps`); })
        .catch((e) => fail(e.message));' ;;
  env)
    case "${2:-}" in "" | mock) ;; *) die "usage: scripts/playground.sh env [mock]" ;; esac
    ready
    echo "export CLAUDETTE_HOST=playground"
    echo "export CLAUDETTE_SSH='$SSH'"
    echo "alias claudette='$CLAUDETTE'"
    if [ "${2:-}" = mock ]; then
      echo "export ANTHROPIC_BASE_URL=http://mockapi:8080 ANTHROPIC_API_KEY=sk-ant-api03-claudette-test"
    fi ;;
  ssh)
    shift; ready
    exec $SSH playground "$@" ;;
  down)
    shift
    [ -f "$STATE/ssh_config" ] && $SSH -O exit playground 2>/dev/null || true
    docker compose down "$@"
    case " $* " in
      *" -v "* | *" --volumes "*) rm -f "$STATE/id_ed25519" "$STATE/known_hosts" "$STATE/ssh_config" ;;
    esac ;;
  *)
    sed -n '2,17s/^# \{0,1\}//p' "$0" >&2
    exit 2 ;;
esac
