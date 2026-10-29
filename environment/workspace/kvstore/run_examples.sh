#!/bin/bash
# Builds kvstore and replays each examples/*.in session against a fresh database,
# comparing the replies with examples/*.out. ERR messages are compared by prefix only.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --offline
status=0
for input in examples/*.in; do
    expected="${input%.in}.out"
    db=$(mktemp -d)
    actual=$(target/release/kvstore --dir "$db/db" < "$input" | sed 's/^ERR.*/ERR/')
    rm -rf "$db"
    if diff <(echo "$actual") "$expected" > /dev/null; then
        echo "PASS $input"
    else
        echo "FAIL $input"
        diff <(echo "$actual") "$expected" || true
        status=1
    fi
done
exit $status
