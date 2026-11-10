#!/bin/sh
set -eu
install -d -m 700 -o remote -g remote /home/remote/.ssh
install -m 600 -o remote -g remote /keys/id_ed25519.pub /home/remote/.ssh/authorized_keys
ssh-keygen -A >/dev/null
exec /usr/sbin/sshd -D -e
