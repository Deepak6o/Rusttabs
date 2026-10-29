//! Data must survive clean restarts, stay per-directory, and move with `cp -a`.

use crate::correctness::key_pool;
use crate::proc::{Ctx, Opts, Proc};
use crate::util::*;
use std::fs;

pub fn small_value(rng: &mut Rng) -> Val {
    let n = if rng.chance(0.05) { rng.range(2000, 20_000) } else { rng.range(0, 2000) };
    rng.bytes(n)
}

/// A random mutation: a single PUT/DEL (70%) or a BATCH of 2..40 operations.
pub fn gen_unit(rng: &mut Rng, keys: &[Key]) -> (Unit, bool) {
    let n = if rng.chance(0.3) { rng.range(2, 40) } else { 1 };
    let u = (0..n)
        .map(|_| {
            let k = rng.pick(keys).clone();
            let v = if rng.chance(0.8) { Some(small_value(rng)) } else { None };
            (k, v)
        })
        .collect();
    (u, n > 1)
}

/// Sends `n` mutations in pipelined chunks, requiring OK for each; updates the model.
pub fn mutate(p: &mut Proc, rng: &mut Rng, keys: &[Key], model: &mut Model, n: usize) -> Result<(), String> {
    let mut left = n;
    while left > 0 {
        let chunk = rng.range(1, 32).min(left);
        let mut buf = Vec::new();
        for _ in 0..chunk {
            let (u, as_batch) = gen_unit(rng, keys);
            encode_unit(&mut buf, &u, as_batch, rng.chance(0.1));
            apply(model, &u);
        }
        if !p.send(&buf) || !p.flush() {
            return Err("engine stopped reading input".into());
        }
        for _ in 0..chunk {
            expect_ok(p, "mutation")?;
        }
        left -= chunk;
    }
    Ok(())
}

pub fn verify(p: &mut Proc, model: &Model, when: &str) -> Result<(), String> {
    let got = full_scan(p).map_err(|e| format!("{when}: {e}"))?;
    match diff(&got, model) {
        Some(d) => Err(format!("{when}: {d}")),
        None => Ok(()),
    }
}

fn check_gets(p: &mut Proc, rng: &mut Rng, keys: &[Key], model: &Model, when: &str) -> Result<(), String> {
    let probes: Vec<Key> = (0..50).map(|_| rng.pick(keys).clone()).collect();
    let mut buf = Vec::new();
    for k in &probes {
        buf.extend_from_slice(b"GET ");
        hex_into(&mut buf, k, false);
        buf.push(b'\n');
    }
    p.send(&buf);
    p.flush();
    for k in &probes {
        let got = p.recv(T)?;
        let mut want = Vec::new();
        match model.get(k) {
            Some(v) => {
                want.extend_from_slice(b"VAL ");
                val_into(&mut want, v, false);
            }
            None => want.extend_from_slice(b"NONE"),
        }
        if got != want {
            return Err(format!("{when}: GET {} returned the wrong answer", short(k)));
        }
    }
    Ok(())
}

/// Several sessions on one directory with varying flags; every reopen is checked.
pub fn restarts(ctx: &Ctx, seed: u64, sessions: usize) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let keys = key_pool(&mut rng);
    let dir = ctx.fresh_dir("restart");
    let mut model = Model::new();
    let mems = [Some(16_384), None, Some(65_536), Some(300_000)];
    for s in 0..=sessions {
        let opts = Opts { sync: rng.chance(0.3), mem: *rng.pick(&mems), ..Default::default() };
        let mut p = Proc::spawn(ctx, &dir, &opts)?;
        if s > 0 {
            verify(&mut p, &model, &format!("after restart {s}"))?;
        }
        if s < sessions {
            check_gets(&mut p, &mut rng, &keys, &model, &format!("session {s}"))?;
            mutate(&mut p, &mut rng, &keys, &mut model, 400)?;
            if rng.chance(0.25) {
                p.send(b"COMPACT\n");
                p.flush();
                expect_ok(&p, "COMPACT")?;
            }
        }
        p.shutdown(T)?;
    }
    Ok(())
}

/// Two directories used alternately must never see each other's data.
pub fn isolation(ctx: &Ctx, seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let keys = key_pool(&mut rng);
    let dirs = [ctx.fresh_dir("iso-a"), ctx.fresh_dir("iso-b")];
    let mut models = [Model::new(), Model::new()];
    for round in 0..3 {
        for i in 0..2 {
            let mut p = Proc::spawn(ctx, &dirs[i], &Opts { mem: Some(32_768), ..Default::default() })?;
            verify(&mut p, &models[i], &format!("round {round}, database {i}"))?;
            mutate(&mut p, &mut rng, &keys, &mut models[i], 250)?;
            p.shutdown(T)?;
        }
    }
    Ok(())
}

/// A cleanly closed directory copied elsewhere (original deleted) must still work.
pub fn relocation(ctx: &Ctx, seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let keys = key_pool(&mut rng);
    let a = ctx.fresh_dir("reloc-a");
    let mut model = Model::new();
    let mut p = Proc::spawn(ctx, &a, &Opts { mem: Some(65_536), ..Default::default() })?;
    mutate(&mut p, &mut rng, &keys, &mut model, 1500)?;
    p.shutdown(T)?;
    let c = ctx.fresh_dir("reloc-c");
    ctx.copy_dir(&a, &c)?;
    fs::remove_dir_all(&a).map_err(|e| e.to_string())?;
    let mut p = Proc::spawn(ctx, &c, &Opts::default())?;
    verify(&mut p, &model, "copied directory")?;
    mutate(&mut p, &mut rng, &keys, &mut model, 300)?;
    p.shutdown(T)?;
    let mut p = Proc::spawn(ctx, &c, &Opts::default())?;
    verify(&mut p, &model, "copied directory after more writes")?;
    p.shutdown(T)
}
