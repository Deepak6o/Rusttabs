# Build a crash-safe, ordered key-value storage engine in Rust

Implement a persistent, ordered key-value store in the Cargo project at `/app/kvstore/`.
It must survive the process being killed at any instant, keep its memory use bounded while holding far more data than fits
in RAM, and reclaim disk space taken by overwritten and deleted data.

The project builds one binary, `kvstore`. Your work is graded by building it with

```
cargo build --release --offline
```

inside `/app/kvstore/` and running `target/release/kvstore` against a hidden test suite.

## Constraints

- Rust, standard library only. No third-party crates: `Cargo.lock` must list only the
  `kvstore` package, and the build is done offline. Do not vendor other storage engines.
- The engine must not start other processes (`fork`, `exec`, `posix_spawn`, shells, ...).
- All persistent state lives inside the directory given by `--dir`. Nothing else on the
  filesystem may be used. A directory produced by a clean shutdown must still work after
  being copied to another path with `cp -a` and opened there.
- Only one `kvstore` process uses a directory at a time.
- The machine is offline; everything you need is installed.

## Command line

```
kvstore --dir <path> [--sync] [--memtable-bytes <N>]
```

- `--dir <path>`: the database directory. Create it (and any missing parents) if needed.
- `--sync`: every mutating command (`PUT`, `DEL`, `BATCH`) is durable before its reply
  is written.
- `--memtable-bytes <N>`: the most key+value bytes the engine may hold in memory that are
  not yet in its sorted on-disk files. Default 4194304 (4 MiB). Tests use values as small
  as 16384, so flushing and compaction happen often.

## Protocol

The engine reads commands from stdin, one per line (`\n`-terminated, tokens separated by
a single space), and writes replies to stdout. Keys and values are arbitrary bytes,
written as hex (the engine must accept upper- and lower-case hex and must reply in
lower-case). An empty value is written as `-`.

- Keys are 1 to 1024 bytes. Values are 0 to 262144 bytes.
- Keys are ordered by unsigned byte-wise comparison (shorter key first when one is a
  prefix of the other).

| Command | Reply |
|---|---|
| `PUT <key> <value>` | `OK` |
| `DEL <key>` | `OK` (also when the key does not exist) |
| `GET <key>` | `VAL <value>` or `NONE` |
| `BATCH <n>` followed by `n` lines, each `PUT <key> <value>` or `DEL <key>` | one `OK` after the last line; applied atomically, in order |
| `SCAN <start> <end> <limit>` | zero or more `ROW <key> <value>` lines, then `END` |
| `SYNC` | `OK` once every earlier acknowledged mutation is durable |
| `COMPACT` | `OK` once space used by overwritten and deleted data has been reclaimed |

- `SCAN` returns live pairs with `start <= key < end`, in ascending key order, at most
  `limit` of them. `start` or `end` may be `-`, meaning unbounded. `limit` is an integer
  from 1 to 1000000.
- `BATCH` takes `n` from 1 to 10000. Later operations in a batch see earlier ones (a
  `PUT` then `DEL` of the same key leaves it deleted).
- Malformed input (unknown command, wrong number of tokens, invalid hex, key or value
  out of range, bad `n` or `limit`) gets the reply `ERR <message>` and changes nothing.
  If any line inside a `BATCH` is malformed, the engine still reads all `n` lines,
  applies none of them, and replies with a single `ERR`.
- Exactly one reply per command (a `SCAN`'s `ROW` lines plus its `END` count as one).
- Clients pipeline commands. Never keep a reply buffered while waiting for more input.
- When stdin reaches end-of-file, make every acknowledged mutation durable, then exit
  with status 0.

## Crash safety

The tests kill the engine with `SIGKILL` at random moments: while it is idle, while it
is reading a pipelined stream of commands, and while it is flushing or compacting. Use
`fsync`/`fdatasync` where durability requires it, even though a process kill alone does
not discard data that already reached the operating system.

After a crash, opening the directory again must succeed, and the store must equal the
state after some prefix of the mutations that were sent, in order, where a `BATCH` is
one indivisible mutation. That prefix must include:

- with `--sync`: every mutation that was acknowledged with `OK`;
- without `--sync`: every mutation acknowledged before the last acknowledged `SYNC`.

The recovered store must keep working: more writes, clean shutdowns and further crashes
(several in a row on the same directory) must all behave correctly.

## Scale, memory and disk space

- The store must handle millions of keys and hundreds of megabytes of data. The engine's
  resident memory (peak RSS) must stay under 128 MiB while it loads about 2 million keys
  (roughly 270 MB of key and value bytes) with the default memtable size, serves random
  reads and scans, and reopens.
- Operations must be fast enough for that load: expect time limits of a few minutes for
  each phase (bulk load in `BATCH`es, 100,000 random `GET`s, thousands of `SCAN`s), and
  opening an existing database must take seconds, not minutes.
- Disk usage (allocated blocks of all files in `--dir`) must stay below 4 × live data
  + 64 MiB at every point while keys are repeatedly overwritten and deleted, without
  any `COMPACT`. After `COMPACT` it must be at most 1.5 × live data + 8 MiB. Live data
  means the total bytes of keys and values currently in the store. Values in the tests
  are random bytes.

## Scoring

The score is the weighted fraction of hidden checks that pass: protocol correctness
against a reference model (randomized, including binary keys, prefixes, large values,
scans with bounds and limits, batches and error handling), persistence across restarts
and directory copies, `SIGKILL` crash recovery (including repeated crashes), scale and
memory, and disk space reclamation. Memory, speed and space checks only count when the data read
back is correct.

## Getting started

`/app/kvstore/src/main.rs` is a stub that answers every command with `ERR`.
`/app/kvstore/examples/` has a sample session with its expected output;
`/app/kvstore/run_examples.sh` builds the project and compares the output. Passing it is
only a first step: the hidden tests are much larger.
