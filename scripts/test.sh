#!/bin/sh
# Builds the images and runs the checks inside the containers. Nothing is installed on the host.
#   scripts/test.sh                 unit tests, typecheck, end-to-end, then the real T3 Code and Paseo
#   scripts/test.sh <command...>    run a command in the local container instead
set -eu
cd "$(dirname "$0")/.."
export COMPOSE_FILE=tests/docker/compose.yaml
docker compose --profile planes build --quiet
trap 'docker compose --profile planes down --volumes --remove-orphans >/dev/null 2>&1' EXIT
if [ "$#" -gt 0 ]; then
  docker compose run --rm local "$@"
else
  docker build --quiet --file tests/docker/local.Dockerfile --target unit . >/dev/null
  docker compose run --rm local sh -c 'bun run typecheck && bun run test:e2e'
  docker compose --profile planes run --rm t3 bun run test:t3
  docker compose --profile planes run --rm paseo bun run test:paseo
fi
