//! Runs the SPL programs in `tests/` and `simulations/` through the `spl`
//! binary with a fixed seed and compares their output with the `.out` file
//! next to each. Regenerate the expected output with:
//!
//!     UPDATE_EXPECT=1 cargo test --test programs
//!
//! Also checks that every example in examples.md runs, and that an unseeded
//! run can be replayed exactly from the seed it reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SEED: &str = "0";

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn spl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spl"))
        .args(args)
        .current_dir(root())
        .output()
        .expect("run spl")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn programs(dir: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(root().join(dir))
        .expect("read program dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "spl"))
        .collect();
    found.sort();
    found
}

/// Runs each program and compares (or, with UPDATE_EXPECT, rewrites) its `.out`.
fn check_dir(dir: &str) {
    let update = std::env::var_os("UPDATE_EXPECT").is_some();
    let mut failures = Vec::new();
    let all = programs(dir);
    assert!(!all.is_empty(), "no programs in {dir}");
    for path in all {
        let rel = path
            .strip_prefix(root())
            .expect("under root")
            .to_string_lossy()
            .into_owned();
        let out = spl(&["--seed", SEED, &rel]);
        let expected_path = path.with_extension("out");
        if !out.status.success() {
            failures.push(format!(
                "{rel}: exited {}\n{}",
                out.status,
                text(&out.stderr)
            ));
        } else if update {
            fs::write(&expected_path, &out.stdout).expect("write .out");
        } else {
            match fs::read_to_string(&expected_path) {
                Err(_) => failures.push(format!("{rel}: missing .out (run with UPDATE_EXPECT=1)")),
                Ok(expected) if expected != text(&out.stdout) => failures.push(format!(
                    "{rel}: output differs\n--- expected ---\n{expected}--- got ---\n{}",
                    text(&out.stdout)
                )),
                Ok(_) => {}
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn test_programs() {
    check_dir("tests");
}

#[test]
fn simulations() {
    check_dir("simulations");
}

#[test]
fn examples_run() {
    let doc = fs::read_to_string(root().join("examples.md")).expect("read examples.md");
    let blocks: Vec<&str> = doc.split("```").skip(1).step_by(2).collect();
    assert!(!blocks.is_empty(), "no examples found");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    for (n, block) in blocks.iter().enumerate() {
        let code = block.split_once('\n').map_or("", |(_info, code)| code);
        let path = dir.join(format!("example{}.spl", n + 1));
        fs::write(&path, code).expect("write example");
        let out = spl(&["--seed", SEED, path.to_str().expect("utf-8 path")]);
        assert!(
            out.status.success(),
            "examples.md example {}: {}",
            n + 1,
            text(&out.stderr)
        );
    }
}

#[test]
fn unseeded_run_replays_from_reported_seed() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("replay.spl");
    fs::write(
        &path,
        "fn main() = { let a = open; let b = fork { open }; print(a, b); }",
    )
    .expect("write");
    let path = path.to_str().expect("utf-8 path");

    let first = text(&spl(&[path]).stdout);
    let seed = first
        .split("--seed ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("no seed reported in:\n{first}"));
    let replay = text(&spl(&["--seed", seed, path]).stdout);

    // The first run has one extra line, reporting the seed
    let first: Vec<&str> = first.lines().collect();
    let replay: Vec<&str> = replay.lines().collect();
    assert_eq!(first[0], replay[0]);
    assert_eq!(first[2..], replay[1..], "replay with --seed {seed} differs");
}

#[test]
fn errors_exit_nonzero() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("error.spl");
    fs::write(&path, "print(1);\nprint(1 / 0);").expect("write");
    let out = spl(&["--seed", SEED, path.to_str().expect("utf-8 path")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stdout).ends_with("1\n"),
        "output before the error is kept"
    );
    assert_eq!(text(&out.stderr), "Error: Division by zero\n");

    let out = spl(&["no/such/file.spl"]);
    assert_eq!(out.status.code(), Some(1));
}
