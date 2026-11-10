#!/bin/sh
# Sets up ssh the way the README asks a real local machine to: a key, and a `remote` alias that needs no prompt.
set -eu
if [ -f /keys/id_ed25519 ]; then
  install -d -m 700 "$HOME/.ssh"
  install -m 600 /keys/id_ed25519 "$HOME/.ssh/id_ed25519"
  cat >"$HOME/.ssh/config" <<'EOF'
Host remote
  HostName remote
  User remote
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
  BatchMode yes
  StrictHostKeyChecking accept-new
EOF
  chmod 600 "$HOME/.ssh/config"
fi
git config --global user.email local@example.test
git config --global user.name local
git config --global init.defaultBranch main
exec "$@"
