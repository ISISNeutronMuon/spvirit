#!/usr/bin/env sh
# Build the spvirit-loadtest:head image from a CLEAN git-HEAD tree.
#
# We export `git archive HEAD` into a temp dir and build there, so any
# uncommitted working-tree changes (e.g. the accept-loop fix) are EXCLUDED.
# That makes the gateway reproduce committed-HEAD behavior (the original
# EMFILE death). To instead build WITH your working-tree changes, run:
#   docker build -f loadtest/Dockerfile.spvirit -t spvirit-loadtest:head .
# from the repo root (respects .dockerignore, includes uncommitted edits).
set -eu

ROOT=$(git rev-parse --show-toplevel)
# Build context must live under $HOME: snap-packaged Docker is confined by the
# `home` interface and cannot read /tmp. mktemp under $HOME works for both snap
# and normal Docker. Override with CTX_BASE if $HOME is unsuitable.
TMP=$(mktemp -d "${CTX_BASE:-$HOME}/spvirit-head.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

echo "build.sh: exporting committed HEAD sources -> $TMP"
git -C "$ROOT" archive HEAD | tar -x -C "$TMP"
cp "$ROOT/loadtest/Dockerfile.spvirit" "$TMP/Dockerfile"

echo "build.sh: building spvirit-loadtest:head (working-tree changes excluded)"
docker build -t spvirit-loadtest:head "$TMP"

echo "build.sh: done -> spvirit-loadtest:head"
