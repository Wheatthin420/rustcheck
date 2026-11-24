// rustcheck — an agent-friendly incremental cargo check.
//
// Modes:
//   rustcheck            cargo check, print only diagnostics NEW since last run
//   rustcheck clippy     cargo clippy --all-targets, same diff logic
//   rustcheck fmt        cargo fmt --check (--write: format in place, like format-on-save)
//   rustcheck fix        cargo clippy --fix (apply machine-applicable lints)
//   rustcheck save       pipeline: fmt --write -> check-diff (the "on save" loop)
//   rustcheck test       cargo test, filtered to results/errors only
//   --reset              re-baseline without reporting churn
//   -p PKG               restrict to one package    --jobs N   (default 3)
//
// Baseline lives in <workspace target>/.rustcheck-<mode>.json (git-ignored).
// Exit codes: 0 = nothing new (or nothing broken), 1 = new problems, 2 = tool error.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Diag {
    level: String,
    code: String,
    loc: String,
    msg: String,
    rendered: String,
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            while let Some(&n) = it.peek() {
                it.next();
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn workspace_root() -> PathBuf {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if let Some(parent) = PathBuf::from(&p).parent() {
                return parent.to_path_buf();
            }
            env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
        _ => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn run_cargo_json(
    sub: &str,
    jobs: &str,
    extra: &[&str],
) -> std::io::Result<(bool, String, String)> {
    let mut child = Command::new("cargo")
        .arg(sub)
        .args(["-j", jobs, "--message-format=json"])
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut so = child.stdout.take().expect("stdout piped");
    let mut se = child.stderr.take().expect("stderr piped");
    let mut stdout = String::new();
    let mut stderr = String::new();
    so.read_to_string(&mut stdout)?;
    se.read_to_string(&mut stderr)?;
    let status = child.wait()?;
    Ok((status.success(), stdout, stderr))
}

fn parse_diags(stdout: &str) -> BTreeMap<String, Diag> {
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim_start();
        if !line.starts_with('{') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let m = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let level = m
            .get("level")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string();
        if level != "error" && level != "warning" {
            continue; // drop failure-note and other pseudo-diagnostics
        }
        let code = m
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("-")
            .to_string();
        let msg = m
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let (file, line_start, col) = match m.get("spans").and_then(|s| s.get(0)) {
            Some(s) => (
                s.get("file_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                s.get("line_start").and_then(|x| x.as_u64()).unwrap_or(0),
                s.get("line_col")
                    .and_then(|lc| lc.get("column_start"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
            ),
            None => ("?".to_string(), 0, 0),
        };
        let pkg = v.get("package_id").and_then(|x| x.as_str()).unwrap_or("?");
        let rendered = m
            .get("rendered")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let key = format!("{pkg}|{level}|{code}|{file}:{line_start}:{col}|{msg}");
        map.insert(
            key,
            Diag {
                level,
                code,
                loc: format!("{file}:{line_start}"),
                msg,
                rendered: strip_ansi(&rendered),
            },
        );
    }
    map
}

fn run_test(jobs: &str, pkg: Option<&str>) -> i32 {
    let mut cmd = Command::new("cargo");
    cmd.args(["test", "-j", jobs]);
    if let Some(p) = pkg {
        cmd.args(["-p", p]);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustcheck: failed to run cargo test: {e}");
            return 2;
        }
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let hits: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.contains("test result")
                || l.contains("error")
                || l.contains("FAILED")
                || l.contains("panicked")
                || l.contains("finished in")
                || l.trim_start().starts_with("running ")
        })
        .collect();
    for l in hits.iter().take(80) {
        println!("{l}");
    }
    if !out.status.success() && hits.is_empty() {
        println!("{}", text.lines().take(40).collect::<Vec<_>>().join("\n"));
    }
    i32::from(!out.status.success())
}

fn run_fmt(_jobs: &str, pkg: Option<&str>, write: bool) -> i32 {
    let mut cmd = Command::new("cargo");
    if write {
        cmd.arg("fmt");
    } else {
        cmd.arg("fmt").arg("--check");
    }
    if let Some(p) = pkg {
        cmd.args(["-p", p]);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustcheck: failed to run cargo fmt: {e}");
            return 2;
        }
    };
    let text = strip_ansi(&format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    if out.status.success() {
        if write {
            println!("rustcheck[fmt]: formatted in place");
        } else {
            println!("rustcheck[fmt]: clean");
        }
        0
    } else {
        for l in text.lines().take(60) {
            println!("{l}");
        }
        if write {
            println!("rustcheck[fmt]: FAILED");
        } else {
            println!("rustcheck[fmt]: diffs found (run `rustcheck fmt --write`)");
        }
        1
    }
}

// cargo clippy --fix: apply machine-applicable suggestions in place.
fn run_fix(jobs: &str, pkg: Option<&str>) -> i32 {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "clippy",
        "--fix",
        "--allow-dirty",
        "--allow-staged",
        "-j",
        jobs,
        "--all-targets",
    ]);
    if let Some(p) = pkg {
        cmd.args(["-p", p]);
    }
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustcheck: failed to run cargo clippy --fix: {e}");
            return 2;
        }
    };
    let text = strip_ansi(&format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    let fixed: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.contains("Fixed")
                || l.contains("updated")
                || l.contains("error")
                || l.contains("warning: unused")
        })
        .collect();
    for l in fixed.iter().take(30) {
        println!("{l}");
    }
    if out.status.success() {
        println!("rustcheck[fix]: done (git diff to review)");
        0
    } else {
        println!("rustcheck[fix]: FAILED (does the crate compile?)");
        1
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut mode = String::from("check");
    let mut jobs = env::var("RUSTCHECK_JOBS").unwrap_or_else(|_| "3".to_string());
    let mut reset = false;
    let mut write = false;
    let mut pkg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "check" | "clippy" | "test" | "fmt" | "fix" | "save" => mode = args[i].clone(),
            "--reset" => reset = true,
            "--write" => write = true,
            "--jobs" => {
                i += 1;
                if i < args.len() {
                    jobs = args[i].clone();
                }
            }
            "-p" => {
                i += 1;
                if i < args.len() {
                    pkg = Some(args[i].clone());
                }
            }
            "-h" | "--help" => {
                println!(
                    "usage: rustcheck [check|clippy|fmt|fix|save|test] [-p PKG] [--jobs N] [--reset] [--write]\n\
                     prints only diagnostics NEW since the previous run; baseline lives in target/.rustcheck-<mode>.json\n\
                     save = fmt --write then check-diff (the on-save loop); fix = clippy autofix in place"
                );
                return;
            }
            other => eprintln!("rustcheck: ignoring unknown arg {other}"),
        }
        i += 1;
    }

    match mode.as_str() {
        "test" => std::process::exit(run_test(&jobs, pkg.as_deref())),
        "fmt" => std::process::exit(run_fmt(&jobs, pkg.as_deref(), write)),
        "fix" => std::process::exit(run_fix(&jobs, pkg.as_deref())),
        "save" => {
            // format-on-save first; a fmt tool error aborts, unformatted diffs don't
            if run_fmt(&jobs, pkg.as_deref(), true) == 2 {
                std::process::exit(2);
            }
        }
        _ => {}
    }

    let sub = if mode == "clippy" { "clippy" } else { "check" };
    let mut extra: Vec<&str> = Vec::new();
    if mode == "clippy" {
        extra.push("--all-targets");
    }
    if let Some(p) = pkg.as_deref() {
        extra.push("-p");
        extra.push(p);
    }

    let t0 = Instant::now();
    let (ok, stdout, stderr) = match run_cargo_json(sub, &jobs, &extra) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rustcheck: failed to run cargo {sub}: {e}");
            std::process::exit(2);
        }
    };

    let diags = parse_diags(&stdout);
    if diags.is_empty() && !ok {
        // cargo failed without emitting parseable diagnostics (bad args, missing toolchain…)
        eprintln!(
            "rustcheck: cargo {sub} failed with no diagnostics:\n{}",
            strip_ansi(&stderr)
        );
        std::process::exit(2);
    }

    let base_mode = if mode == "save" {
        "check"
    } else {
        mode.as_str()
    };
    let base_file = workspace_root()
        .join("target")
        .join(format!(".rustcheck-{base_mode}.json"));
    let baseline: BTreeMap<String, Diag> = if reset {
        BTreeMap::new()
    } else {
        fs::read_to_string(&base_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    };

    let new: Vec<&String> = diags
        .keys()
        .filter(|k| !baseline.contains_key(*k))
        .collect();
    let resolved: Vec<&String> = baseline
        .keys()
        .filter(|k| !diags.contains_key(*k))
        .collect();
    let errors = diags.values().filter(|d| d.level == "error").count();
    let warnings = diags.values().filter(|d| d.level == "warning").count();

    println!(
        "rustcheck[{mode}]: {errors} errors, {warnings} warnings | {} new, {} resolved vs baseline ({:.1}s)",
        new.len(),
        resolved.len(),
        t0.elapsed().as_secs_f64()
    );

    if reset {
        println!(
            "rustcheck: baseline reset ({n} current diagnostics stored)",
            n = diags.len()
        );
    }

    for k in new.iter().take(40) {
        let d = diags.get(*k).expect("key from diags");
        println!("NEW [{}] {} @ {}", d.level, d.code, d.loc);
        for l in d.rendered.lines().take(25) {
            println!("  {l}");
        }
    }
    if new.len() > 40 {
        println!("  … {} more new diagnostics omitted", new.len() - 40);
    }
    for k in resolved.iter().take(15) {
        let d = baseline.get(*k).expect("key from baseline");
        println!("RESOLVED [{}] {} @ {}", d.level, d.code, d.loc);
    }

    if let Some(dir) = base_file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&diags) {
        let _ = fs::write(&base_file, json);
    }

    let new_errors = new
        .iter()
        .any(|k| diags.get(*k).is_some_and(|d| d.level == "error"));
    std::process::exit(if new_errors { 1 } else { 0 });
}
