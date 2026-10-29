//! kvstore: line protocol front end for the storage engine.

mod engine;
mod sst;
mod util;
mod wal;

use engine::Engine;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use util::{hex_decode, hex_encode_into};
use wal::Op;

const MAX_KEY: usize = 1024;
const MAX_VALUE: usize = 262_144;
const MAX_BATCH: usize = 10_000;
const MAX_LIMIT: usize = 1_000_000;

fn parse_key(tok: &[u8]) -> Result<Vec<u8>, &'static str> {
    let k = hex_decode(tok).ok_or("invalid hex key")?;
    if k.is_empty() || k.len() > MAX_KEY {
        return Err("key length out of range");
    }
    Ok(k)
}

fn parse_value(tok: &[u8]) -> Result<Vec<u8>, &'static str> {
    if tok == b"-" {
        return Ok(Vec::new());
    }
    if tok.is_empty() {
        return Err("empty value token");
    }
    let v = hex_decode(tok).ok_or("invalid hex value")?;
    if v.len() > MAX_VALUE {
        return Err("value too long");
    }
    Ok(v)
}

fn parse_bound(tok: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    if tok == b"-" {
        Ok(None)
    } else {
        parse_key(tok).map(Some)
    }
}

fn parse_count(tok: &[u8], max: usize) -> Result<usize, &'static str> {
    let s = std::str::from_utf8(tok).map_err(|_| "bad number")?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("bad number");
    }
    let n: usize = s.parse().map_err(|_| "bad number")?;
    if n == 0 || n > max {
        return Err("number out of range");
    }
    Ok(n)
}

/// Parses a PUT or DEL line (top level or inside a BATCH).
fn parse_mutation(toks: &[&[u8]]) -> Result<Op, &'static str> {
    match toks {
        [b"PUT", k, v] => Ok(Op::Put(parse_key(k)?, parse_value(v)?)),
        [b"DEL", k] => Ok(Op::Del(parse_key(k)?)),
        [b"PUT", ..] | [b"DEL", ..] => Err("wrong number of arguments"),
        _ => Err("expected PUT or DEL"),
    }
}

fn tokens(line: &[u8]) -> Vec<&[u8]> {
    line.split(|&c| c == b' ').collect()
}

fn trim_eol(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

struct Args {
    dir: PathBuf,
    sync: bool,
    mem: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut dir = None;
    let mut sync = false;
    let mut mem = 4 << 20;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = Some(PathBuf::from(it.next().ok_or("--dir needs a path")?)),
            "--sync" => sync = true,
            "--memtable-bytes" => {
                mem = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|&n: &usize| n > 0)
                    .ok_or("--memtable-bytes needs a positive integer")?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args { dir: dir.ok_or("--dir is required")?, sync, mem })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kvstore: {e}");
            std::process::exit(2);
        }
    };
    let mut eng = match Engine::open(&args.dir, args.sync, args.mem) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("kvstore: cannot open {}: {e}", args.dir.display());
            std::process::exit(1);
        }
    };
    if let Err(e) = serve(&mut eng) {
        eprintln!("kvstore: {e}");
        std::process::exit(1);
    }
}

fn serve(eng: &mut Engine) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut rd = BufReader::with_capacity(1 << 20, stdin.lock());
    let mut out = BufWriter::with_capacity(1 << 16, stdout.lock());
    let mut line = Vec::new();
    let mut sub = Vec::new();
    loop {
        // Flush replies before we might block waiting for input.
        if !rd.buffer().contains(&b'\n') {
            out.flush()?;
        }
        line.clear();
        if rd.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        trim_eol(&mut line);
        let toks = tokens(&line);
        let reply: Result<(), String> = match toks.as_slice() {
            [b"PUT", ..] | [b"DEL", ..] => match parse_mutation(&toks) {
                Ok(op) => eng.apply(vec![op]).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            },
            [b"GET", k] => match parse_key(k) {
                Ok(k) => match eng.get(&k) {
                    Ok(Some(v)) => {
                        out.write_all(b"VAL ")?;
                        write_value(&mut out, &v)?;
                        out.write_all(b"\n")?;
                        continue;
                    }
                    Ok(None) => {
                        out.write_all(b"NONE\n")?;
                        continue;
                    }
                    Err(e) => Err(e.to_string()),
                },
                Err(e) => Err(e.to_string()),
            },
            [b"BATCH", n] => {
                let count = parse_count(n, MAX_BATCH);
                match count {
                    Err(e) => Err(e.to_string()),
                    Ok(count) => {
                        let mut ops = Vec::with_capacity(count);
                        let mut err: Option<String> = None;
                        for _ in 0..count {
                            sub.clear();
                            if rd.read_until(b'\n', &mut sub)? == 0 {
                                err = Some("end of input inside BATCH".into());
                                break;
                            }
                            trim_eol(&mut sub);
                            if err.is_none() {
                                match parse_mutation(&tokens(&sub)) {
                                    Ok(op) => ops.push(op),
                                    Err(e) => err = Some(e.to_string()),
                                }
                            }
                        }
                        match err {
                            Some(e) => Err(e),
                            None => eng.apply(ops).map_err(|e| e.to_string()),
                        }
                    }
                }
            }
            [b"SCAN", s, e, l] => {
                match (parse_bound(s), parse_bound(e), parse_count(l, MAX_LIMIT)) {
                    (Ok(s), Ok(e), Ok(l)) => {
                        let mut buf = Vec::new();
                        let res = eng.scan(s.as_deref(), e.as_deref(), l, |k, v| {
                            buf.clear();
                            buf.extend_from_slice(b"ROW ");
                            hex_encode_into(&mut buf, k);
                            buf.push(b' ');
                            if v.is_empty() {
                                buf.push(b'-');
                            } else {
                                hex_encode_into(&mut buf, v);
                            }
                            buf.push(b'\n');
                            out.write_all(&buf)
                        });
                        match res {
                            Ok(()) => {
                                out.write_all(b"END\n")?;
                                continue;
                            }
                            Err(e) => Err(e.to_string()),
                        }
                    }
                    (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Err(e.to_string()),
                }
            }
            [b"SYNC"] => eng.sync().map_err(|e| e.to_string()),
            [b"COMPACT"] => eng.compact().map_err(|e| e.to_string()),
            _ => Err("unknown command or wrong number of arguments".into()),
        };
        match reply {
            Ok(()) => out.write_all(b"OK\n")?,
            Err(e) => writeln!(out, "ERR {e}")?,
        }
    }
    out.flush()?;
    eng.sync()?;
    Ok(())
}

fn write_value(out: &mut impl Write, v: &[u8]) -> io::Result<()> {
    if v.is_empty() {
        return out.write_all(b"-");
    }
    let mut buf = Vec::with_capacity(v.len() * 2);
    hex_encode_into(&mut buf, v);
    out.write_all(&buf)
}
