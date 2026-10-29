//! Randomized protocol checking against an in-memory model.

use crate::proc::{Ctx, Opts, Proc};
use crate::util::*;
use std::ops::Bound;

enum Exp {
    Ok,
    Err,
    Val(Option<Val>),
    Rows(Vec<(Key, Val)>),
}

pub fn key_pool(rng: &mut Rng) -> Vec<Key> {
    let mut pool: Vec<Key> = Vec::new();
    for _ in 0..40 {
        pool.push(vec![rng.below(256) as u8]);
    }
    pool.extend([vec![0x00], vec![0xff], vec![0x00, 0x00], vec![0xff, 0xff, 0xff], vec![0x00, 0xff]]);
    for _ in 0..150 {
        let n = rng.range(2, 24);
        pool.push(rng.bytes(n));
    }
    // Families of keys that are prefixes of each other.
    for _ in 0..20 {
        let n = rng.range(3, 12);
        let base = rng.bytes(n);
        for cut in 1..=base.len() {
            pool.push(base[..cut].to_vec());
        }
        let mut ext = base.clone();
        ext.push(0x00);
        pool.push(ext.clone());
        ext.push(0xff);
        pool.push(ext);
    }
    for _ in 0..6 {
        pool.push(rng.bytes(1024));
    }
    for _ in 0..4 {
        let n = rng.range(900, 1023);
        pool.push(rng.bytes(n));
    }
    pool.sort();
    pool.dedup();
    rng.shuffle(&mut pool);
    pool
}

pub fn gen_value(rng: &mut Rng) -> Val {
    let r = rng.below(100);
    let n = if r < 10 {
        0
    } else if r < 75 {
        rng.range(1, 64)
    } else if r < 95 {
        rng.range(65, 4096)
    } else if r < 98 {
        rng.range(4097, 100_000)
    } else {
        262_144
    };
    rng.bytes(n)
}

fn pick_key(rng: &mut Rng, pool: &[Key]) -> Key {
    if rng.chance(0.12) {
        let n = rng.range(1, 40);
        rng.bytes(n)
    } else {
        rng.pick(pool).clone()
    }
}

fn bound(rng: &mut Rng, pool: &[Key]) -> Option<Key> {
    match rng.below(10) {
        0..=2 => None,
        3..=8 => Some(rng.pick(pool).clone()),
        _ => {
            let n = rng.range(1, 8);
            Some(rng.bytes(n))
        }
    }
}

fn error_command(rng: &mut Rng, pool: &[Key], buf: &mut Vec<u8>) -> &'static str {
    let k = hex(&pick_key(rng, pool));
    let k2 = hex(&pick_key(rng, pool));
    let line = match rng.below(17) {
        0 => "PUT 616 61".to_string(),
        1 => "PUT zz 00".to_string(),
        2 => format!("PUT {} 00", hex(&rng.bytes(1025))),
        3 => format!("PUT {k} {}", hex(&rng.bytes(262_145))),
        4 => "GET".to_string(),
        5 => format!("GET {k} {k2}"),
        6 => format!("FROB {k}"),
        7 => "SCAN - - 0".to_string(),
        8 => "SCAN - - 1000001".to_string(),
        9 => "SCAN - - x".to_string(),
        10 => "BATCH 0".to_string(),
        11 => "BATCH 10001".to_string(),
        12 => "DEL".to_string(),
        13 => format!("PUT {k}"),
        14 => format!("SCAN {k} -"),
        15 => format!("PUT {k} 0"),
        _ => {
            // A batch with one malformed line: nothing in it may be applied.
            let n = rng.range(2, 8);
            let bad = rng.below(n as u64) as usize;
            let mut s = format!("BATCH {n}");
            for i in 0..n {
                if i == bad {
                    s.push_str(&format!("\nPUT {k} 0g"));
                } else {
                    s.push_str(&format!("\nPUT {} {}", hex(&pick_key(rng, pool)), hex(&rng.bytes(5))));
                }
            }
            buf.extend_from_slice(s.as_bytes());
            buf.push(b'\n');
            return "BATCH with a malformed line";
        }
    };
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    "malformed command"
}

fn gen_cmd(rng: &mut Rng, pool: &[Key], model: &mut Model, buf: &mut Vec<u8>, err_pct: u64) -> (Exp, &'static str) {
    let upper = rng.chance(0.1);
    if rng.below(100) < err_pct {
        let d = error_command(rng, pool, buf);
        return (Exp::Err, d);
    }
    let r = rng.below(100);
    if r < 33 {
        let u: Unit = vec![(pick_key(rng, pool), Some(gen_value(rng)))];
        encode_unit(buf, &u, false, upper);
        apply(model, &u);
        (Exp::Ok, "PUT")
    } else if r < 45 {
        let u: Unit = vec![(pick_key(rng, pool), None)];
        encode_unit(buf, &u, false, upper);
        apply(model, &u);
        (Exp::Ok, "DEL")
    } else if r < 68 {
        let k = pick_key(rng, pool);
        buf.extend_from_slice(b"GET ");
        hex_into(buf, &k, upper);
        buf.push(b'\n');
        (Exp::Val(model.get(&k).cloned()), "GET")
    } else if r < 80 {
        let s = bound(rng, pool);
        let e = bound(rng, pool);
        let limit = match rng.below(100) {
            0..=59 => rng.range(1, 10),
            60..=84 => rng.range(11, 300),
            _ => 1_000_000,
        };
        buf.extend_from_slice(b"SCAN ");
        match &s {
            Some(k) => hex_into(buf, k, upper),
            None => buf.push(b'-'),
        }
        buf.push(b' ');
        match &e {
            Some(k) => hex_into(buf, k, upper),
            None => buf.push(b'-'),
        }
        buf.extend_from_slice(format!(" {limit}\n").as_bytes());
        let rows = match (&s, &e) {
            (Some(a), Some(b)) if a >= b => Vec::new(),
            _ => {
                let lo = s.as_deref().map_or(Bound::Unbounded, Bound::Included);
                let hi = e.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                model
                    .range::<[u8], _>((lo, hi))
                    .take(limit)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            }
        };
        (Exp::Rows(rows), "SCAN")
    } else if r < 91 {
        let n = match rng.below(100) {
            0..=69 => rng.range(1, 10),
            70..=94 => rng.range(11, 200),
            _ => rng.range(1000, 2000),
        };
        let small = n > 10;
        let hot: Vec<Key> = (0..rng.range(1, 3)).map(|_| pick_key(rng, pool)).collect();
        let mut u: Unit = Vec::with_capacity(n);
        for _ in 0..n {
            let k = if rng.chance(0.2) { rng.pick(&hot).clone() } else { pick_key(rng, pool) };
            let v = if rng.chance(0.75) {
                Some(if small {
                    let len = rng.range(0, 200);
                    rng.bytes(len)
                } else {
                    gen_value(rng)
                })
            } else {
                None
            };
            u.push((k, v));
        }
        encode_unit(buf, &u, true, upper);
        apply(model, &u);
        (Exp::Ok, "BATCH")
    } else if r < 93 {
        buf.extend_from_slice(b"SYNC\n");
        (Exp::Ok, "SYNC")
    } else if r < 94 {
        buf.extend_from_slice(b"COMPACT\n");
        (Exp::Ok, "COMPACT")
    } else {
        // Unbounded scan with a small limit from a random start.
        let s = rng.pick(pool).clone();
        buf.extend_from_slice(b"SCAN ");
        hex_into(buf, &s, upper);
        buf.extend_from_slice(b" - 3\n");
        let rows = model
            .range::<[u8], _>((Bound::Included(s.as_slice()), Bound::Unbounded))
            .take(3)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        (Exp::Rows(rows), "SCAN")
    }
}

fn check_reply(p: &Proc, e: &Exp) -> Result<(), String> {
    match e {
        Exp::Rows(want) => {
            let got = read_rows(p)?;
            if got != *want {
                let i = got.iter().zip(want.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
                return Err(format!("SCAN returned {} rows, expected {}; first difference at row {i}", got.len(), want.len()));
            }
            Ok(())
        }
        _ => {
            let l = p.recv(T)?;
            let ok = match e {
                Exp::Ok => l == b"OK",
                Exp::Err => l.starts_with(b"ERR"),
                Exp::Val(None) => l == b"NONE",
                Exp::Val(Some(v)) => {
                    let mut want = b"VAL ".to_vec();
                    val_into(&mut want, v, false);
                    l == want
                }
                Exp::Rows(_) => unreachable!(),
            };
            if ok {
                return Ok(());
            }
            let want = match e {
                Exp::Ok => "OK".to_string(),
                Exp::Err => "ERR ...".to_string(),
                Exp::Val(None) => "NONE".to_string(),
                Exp::Val(Some(v)) => format!("VAL {}", short(v)),
                Exp::Rows(_) => unreachable!(),
            };
            Err(format!("expected {want}, got {:?}", String::from_utf8_lossy(&l[..l.len().min(60)])))
        }
    }
}

/// One engine session of `ops` random commands, every reply checked, then a full
/// SCAN and a clean shutdown.
pub fn random_session(ctx: &Ctx, seed: u64, opts: Opts, ops: usize, err_pct: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let pool = key_pool(&mut rng);
    let mut model = Model::new();
    let dir = ctx.fresh_dir("model");
    let mut p = Proc::spawn(ctx, &dir, &opts)?;
    let mut done = 0;
    while done < ops {
        let chunk = rng.range(1, 64).min(ops - done);
        let mut buf = Vec::new();
        let mut exps = Vec::with_capacity(chunk);
        for _ in 0..chunk {
            exps.push(gen_cmd(&mut rng, &pool, &mut model, &mut buf, err_pct));
        }
        if !p.send(&buf) || !p.flush() {
            return Err(format!("engine stopped reading input near command {}", done + 1));
        }
        for (i, (e, what)) in exps.iter().enumerate() {
            check_reply(&p, e).map_err(|err| format!("command {} ({what}): {err}", done + i + 1))?;
        }
        done += chunk;
    }
    let got = full_scan(&mut p)?;
    if let Some(d) = diff(&got, &model) {
        return Err(format!("final full SCAN: {d}"));
    }
    p.shutdown(T)
}
