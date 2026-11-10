#!/bin/sh
# Installs claudette from its GitHub releases:
#   curl -fsSL https://raw.githubusercontent.com/oshinop/claudette/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/oshinop/claudette/main/scripts/install.sh | CLAUDETTE_HOST=mini sh
#
# Every platform's build goes into one folder, because claudette copies the remote machine's build
# from beside its own (a Mac driving a Linux machine needs claudette-linux-*). The `claudette` link
# points at this machine's build.
#
#   CLAUDETTE_VERSION       release to install, such as v0.1.0 (default: the latest)
#   CLAUDETTE_BIN_DIR       where the `claudette` link goes (default: ~/.local/bin)
#   CLAUDETTE_LIB_DIR       where the builds go (default: ~/.local/share/claudette)
#   CLAUDETTE_DOWNLOAD_URL  where the release files are (default: the GitHub release), for mirrors
#
# claudette's settings given as variables (CLAUDETTE_HOST, CLAUDETTE_DETACH_TIMEOUT, …; see
# docs/configuration.md) are written to ~/.config/claudette/config.toml, unless that file exists.
# Set them on `sh`, which runs this script, not on `curl`.
set -eu

REPO=oshinop/claudette
PLATFORMS="darwin-arm64 darwin-x64 linux-arm64 linux-x64"
version=${CLAUDETTE_VERSION:-latest}
bin_dir=${CLAUDETTE_BIN_DIR:-$HOME/.local/bin}
lib_dir=${CLAUDETTE_LIB_DIR:-$HOME/.local/share/claudette}

fail() {
  echo "claudette install: $*" >&2
  exit 1
}

# claudette's settings from the environment, as config.toml lines. Checked before anything is
# downloaded, so a bad value fails early.
toml_string() {
  case $1 in *"
"*) fail "a setting can't hold a line break" ;; esac
  printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"
}
settings=""
for key in host ssh remote_claude remote_nc forward_env detach_timeout log prefix; do
  var="CLAUDETTE_$(printf '%s' "$key" | tr '[:lower:]' '[:upper:]')"
  eval "value=\${$var:-}"
  [ -n "$value" ] && settings="$settings$key = $(toml_string "$value")
"
done
if [ -n "${CLAUDETTE_FORWARD_CLAUDE_ENV:-}" ]; then
  case $(printf '%s' "$CLAUDETTE_FORWARD_CLAUDE_ENV" | tr '[:upper:]' '[:lower:]') in
    1 | true | yes | on) settings="${settings}forward_claude_env = true
" ;;
    0 | false | no | off) settings="${settings}forward_claude_env = false
" ;;
    *) fail "CLAUDETTE_FORWARD_CLAUDE_ENV=$CLAUDETTE_FORWARD_CLAUDE_ENV: use 1 or 0" ;;
  esac
fi
config="$HOME/.config/claudette/config.toml"

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) fail "no build for $(uname -s); claudette runs on macOS and Linux" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=x64 ;;
  *) fail "no build for $(uname -m); claudette runs on arm64 and x86-64" ;;
esac

if [ -n "${CLAUDETTE_DOWNLOAD_URL:-}" ]; then
  base=${CLAUDETTE_DOWNLOAD_URL%/}
elif [ "$version" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$version"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "needs sha256sum or shasum to check the download"
fi

tmp=$(mktemp -d)
trap 'rm -rf -- "${tmp:?}"' EXIT

echo "Downloading claudette ($version) from $base"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "cannot download $base/SHA256SUMS"
for p in $PLATFORMS; do
  f="claudette-$p"
  fetch "$base/$f" "$tmp/$f" || fail "cannot download $base/$f"
  want=$(awk -v f="$f" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$want" ] || fail "SHA256SUMS has no entry for $f"
  [ "$(sha256 "$tmp/$f")" = "$want" ] || fail "$f does not match its checksum"
  chmod 755 "$tmp/$f"
done

mkdir -p "$lib_dir" "$bin_dir"
for p in $PLATFORMS; do
  mv -f "$tmp/claudette-$p" "$lib_dir/claudette-$p"
done
ln -sf "$lib_dir/claudette-$os-$arch" "$bin_dir/claudette"

if [ -n "$settings" ]; then
  if [ -e "$config" ]; then
    echo "$config already exists and is left as it is; the settings given were:"
    printf '%s' "$settings" | sed 's/^/  /'
  else
    mkdir -p "${config%/*}"
    printf '%s' "$settings" >"$config"
    echo "Wrote $config"
  fi
fi

installed=$(CLAUDETTE_PREFIX=+ "$bin_dir/claudette" +version) || fail "the installed claudette does not run here"
echo "Installed claudette $installed: $bin_dir/claudette -> $lib_dir/claudette-$os-$arch"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "$bin_dir is not on PATH; add it to your shell's profile." ;;
esac
if [ -e "$config" ] && grep -q '^host *=' "$config"; then
  echo "Next: run \`claudette +doctor\` to check ssh and the remote machine."
else
  echo "Next: put host = \"<the remote machine's ssh name>\" in $config and run \`claudette +doctor\`."
fi
