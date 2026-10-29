#!/bin/bash
# Verifier for crash-safe-kv-engine.
# Builds the agent's engine as the unprivileged agent user, builds the hidden harness,
# runs every check against the engine and writes the weighted score to reward.txt.
set -u

LOG=/logs/verifier
mkdir -p "$LOG"
# Harbor prefers reward.json, so remove anything left behind during the agent phase.
rm -f "$LOG/reward.json" "$LOG/reward.txt"

finish() {
    pkill -9 -u agent 2>/dev/null || true
    rm -f "$LOG/reward.json"
    echo "$1" > "$LOG/reward.txt"
    echo "reward: $1"
    exit 0
}

# Nothing the agent started may keep running during verification.
pkill -9 -u agent 2>/dev/null || true
sleep 1

TESTS_DIR=$(dirname "$(readlink -f "$0")")
W=/tmp/kvverify
rm -rf "$W"
mkdir -p "$W/src" "$W/agent-home" "$W/agent-target" "$W/bin"
export RUSTUP_HOME=/usr/local/rustup
CARGO=/usr/local/cargo/bin/cargo

# 1. Build the agent's project as the agent user, offline, with fixed flags.
if [ ! -f /app/kvstore/Cargo.toml ]; then
    echo "no /app/kvstore/Cargo.toml"
    finish 0
fi
tar -C /app/kvstore --exclude=./target -cf - . | tar -C "$W/src" -xf -
chown -R agent:agent "$W/src" "$W/agent-home" "$W/agent-target"
if ! runuser -u agent -- env -i HOME="$W/agent-home" PATH=/usr/local/cargo/bin:/usr/bin:/bin \
        RUSTUP_HOME=/usr/local/rustup CARGO_HOME="$W/agent-home/.cargo" \
        CARGO_TARGET_DIR="$W/agent-target" RUSTFLAGS="-C opt-level=3" \
        timeout 900 "$CARGO" build --release --offline --manifest-path "$W/src/Cargo.toml" \
        > "$LOG/build.log" 2>&1; then
    echo "build failed; see build.log"
    tail -20 "$LOG/build.log"
    finish 0
fi
pkill -9 -u agent 2>/dev/null || true

# 2. Standard library only: the lock file may name nothing but the kvstore package.
packages=$(grep -c '^\[\[package\]\]' "$W/src/Cargo.lock" 2>/dev/null || echo 0)
if [ "$packages" != "1" ] || ! grep -q '^name = "kvstore"' "$W/src/Cargo.lock"; then
    echo "Cargo.lock must contain only the kvstore package (found $packages packages)"
    finish 0
fi
if [ ! -x "$W/agent-target/release/kvstore" ]; then
    echo "target/release/kvstore was not produced"
    finish 0
fi
cp "$W/agent-target/release/kvstore" "$W/bin/kvstore"
chmod 755 "$W/bin" "$W/bin/kvstore"

# 3. Build the harness (as root, from the hidden tests).
cp -r "$TESTS_DIR/harness" "$W/harness"
if ! CARGO_HOME="$W/cargo-home" CARGO_TARGET_DIR="$W/harness-target" \
        "$CARGO" build --release --offline --manifest-path "$W/harness/Cargo.toml" \
        > "$LOG/harness-build.log" 2>&1; then
    echo "harness build failed"
    tail -20 "$LOG/harness-build.log"
    finish 0
fi

# 4. Run every check.
timeout 3300 "$W/harness-target/release/kvharness" \
    --bin "$W/bin/kvstore" \
    --work "$W/run" \
    --uid "$(id -u agent)" --gid "$(id -g agent)" \
    --details "$LOG/details.json" \
    --reward-out "$W/reward" \
    2>&1 | tee "$LOG/harness.log"

score=$(cat "$W/reward" 2>/dev/null || echo 0)
finish "$score"
