#!/bin/bash
# Reference solution: install the LSM-tree engine (write-ahead log, checksummed sorted
# tables, atomic manifest, compaction) into the agent's workspace.
set -euo pipefail
SRC=$(dirname "$(readlink -f "$0")")/kvstore
DEST=/app/kvstore
rm -rf "$DEST/src"
mkdir -p "$DEST/src"
cp "$SRC/Cargo.toml" "$DEST/Cargo.toml"
cp "$SRC/Cargo.lock" "$DEST/Cargo.lock"
cp "$SRC"/src/*.rs "$DEST/src/"
cd "$DEST"
cargo build --release --offline
./run_examples.sh
