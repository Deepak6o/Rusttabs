//! Hidden verifier for the kvstore task. Runs every check group against the agent's
//! engine and writes the weighted pass fraction as the reward.

mod correctness;
mod crash;
mod persistence;
mod proc;
mod scale;
mod space;
mod util;

use proc::{Ctx, Opts};
use std::fs;
use std::io::Read;
use std::os::unix::fs::chown;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Check {
    group: &'static str,
    name: String,
    pass: bool,
    detail: String,
    secs: f64,
}

const GROUPS: [(&str, f64); 5] = [
    ("correctness", 0.25),
    ("persistence", 0.20),
    ("crash", 0.25),
    ("scale", 0.20),
    ("space", 0.10),
];

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn random_seed() -> u64 {
    let mut b = [0u8; 8];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    u64::from_le_bytes(b)
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn reset_dir(p: &Path, uid: u32, gid: u32) {
    let _ = fs::remove_dir_all(p);
    fs::create_dir_all(p).expect("create work dir");
    chown(p, Some(uid), Some(gid)).expect("chown work dir");
}

fn guarded<T>(f: impl FnOnce() -> T, on_panic: impl FnOnce(String) -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            on_panic(format!("verifier error: {msg}"))
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let need = |n: &str| arg(&args, n).unwrap_or_else(|| panic!("missing {n}"));
    let bin = PathBuf::from(need("--bin"));
    let work = PathBuf::from(need("--work"));
    let uid: u32 = need("--uid").parse().unwrap();
    let gid: u32 = need("--gid").parse().unwrap();
    let details = arg(&args, "--details");
    let reward_out = arg(&args, "--reward-out");
    let only = arg(&args, "--only");
    let seed = arg(&args, "--seed").map(|s| s.parse().unwrap()).unwrap_or_else(random_seed);
    let scale_keys: usize = arg(&args, "--scale-keys").map(|s| s.parse().unwrap()).unwrap_or(2_000_000);

    reset_dir(&work, uid, gid);
    for sub in ["db", "home", "logs"] {
        reset_dir(&work.join(sub), uid, gid);
    }
    let ctx = Ctx::new(bin, work.clone(), uid, gid);
    println!("seed {seed}");

    let mut checks: Vec<Check> = Vec::new();
    let mut record = |group: &'static str, name: String, r: Result<(), String>, secs: f64| {
        let (pass, detail) = match r {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e),
        };
        println!("{} {group}/{name} ({secs:.1}s){}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(": {detail}") });
        checks.push(Check { group, name, pass, detail, secs });
    };
    let wants = |g: &str| only.as_deref().map_or(true, |o| o == g);
    let s = |k: u64| util::mix(seed ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15));

    // Single checks: (group, name, closure).
    type Single<'a> = (&'static str, String, Box<dyn FnOnce() -> Result<(), String> + 'a>);
    let mut singles: Vec<Single> = Vec::new();
    let c = &ctx;
    let model_runs: [(&str, Opts, usize, u64); 6] = [
        ("default memtable", Opts::default(), 4000, 0),
        ("16 KiB memtable", Opts { mem: Some(16_384), ..Default::default() }, 4000, 0),
        ("--sync, 64 KiB memtable", Opts { sync: true, mem: Some(65_536), ..Default::default() }, 3000, 0),
        ("1 MiB memtable", Opts { mem: Some(1 << 20), ..Default::default() }, 4000, 0),
        ("16 KiB memtable, second seed", Opts { mem: Some(16_384), ..Default::default() }, 4000, 0),
        ("malformed input", Opts { mem: Some(32_768), ..Default::default() }, 800, 35),
    ];
    for (i, (name, opts, ops, err)) in model_runs.into_iter().enumerate() {
        let sd = s(100 + i as u64);
        singles.push(("correctness", name.to_string(), Box::new(move || correctness::random_session(c, sd, opts, ops, err))));
    }
    for i in 0..3 {
        let sd = s(200 + i);
        singles.push(("persistence", format!("restarts #{}", i + 1), Box::new(move || persistence::restarts(c, sd, 8))));
    }
    let sd = s(210);
    singles.push(("persistence", "directory isolation".into(), Box::new(move || persistence::isolation(c, sd))));
    let sd = s(211);
    singles.push(("persistence", "copied directory".into(), Box::new(move || persistence::relocation(c, sd))));
    for i in 0..24 {
        let sd = s(300 + i);
        singles.push(("crash", format!("trial #{}", i + 1), Box::new(move || crash::crash_trial(c, sd))));
    }

    for (group, name, f) in singles {
        if !wants(group) {
            continue;
        }
        let t0 = Instant::now();
        let r = guarded(f, Err);
        record(group, name, r, t0.elapsed().as_secs_f64());
        reset_dir(&work.join("db"), uid, gid);
    }

    type Multi<'a> = (&'static str, Box<dyn FnOnce() -> Vec<(String, Result<(), String>)> + 'a>);
    let sd_scale = s(400);
    let sd_space = s(500);
    let multis: Vec<Multi> = vec![
        ("scale", Box::new(move || scale::scale(c, sd_scale, scale_keys))),
        ("space", Box::new(move || space::space(c, sd_space))),
    ];
    for (group, f) in multis {
        if !wants(group) {
            continue;
        }
        let t0 = Instant::now();
        let results = guarded(f, |e| vec![("verifier".to_string(), Err(e))]);
        let secs = t0.elapsed().as_secs_f64();
        for (name, r) in results {
            record(group, name, r, secs);
        }
        reset_dir(&work.join("db"), uid, gid);
    }

    // Weighted score; a group with no checks run (via --only) is skipped.
    let mut score = 0.0;
    let mut total_w = 0.0;
    let mut group_json = Vec::new();
    for (g, w) in GROUPS {
        let cs: Vec<&Check> = checks.iter().filter(|c| c.group == g).collect();
        if cs.is_empty() {
            continue;
        }
        let frac = cs.iter().filter(|c| c.pass).count() as f64 / cs.len() as f64;
        score += w * frac;
        total_w += w;
        println!("group {g}: {}/{} passed (weight {w})", cs.iter().filter(|c| c.pass).count(), cs.len());
        group_json.push(format!("{}: {{\"weight\": {w}, \"fraction\": {frac:.4}}}", json_str(g)));
    }
    if only.is_some() && total_w > 0.0 {
        score /= total_w;
    }
    let score = (score * 10_000.0).round() / 10_000.0;
    println!("score {score:.4}");

    if let Some(path) = details {
        let items: Vec<String> = checks
            .iter()
            .map(|c| {
                format!(
                    "    {{\"group\": {}, \"name\": {}, \"pass\": {}, \"seconds\": {:.1}, \"detail\": {}}}",
                    json_str(c.group),
                    json_str(&c.name),
                    c.pass,
                    c.secs,
                    json_str(&c.detail)
                )
            })
            .collect();
        let doc = format!(
            "{{\n  \"seed\": {seed},\n  \"score\": {score:.4},\n  \"groups\": {{{}}},\n  \"checks\": [\n{}\n  ]\n}}\n",
            group_json.join(", "),
            items.join(",\n")
        );
        let _ = fs::write(path, doc);
    }
    if let Some(path) = reward_out {
        fs::write(path, format!("{score:.4}\n")).expect("write reward");
    }
}
