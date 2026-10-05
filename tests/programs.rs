//! Golden programs: every `tests/*.px` and `simulations/*.px` must produce
//! exactly its `.out` report. `UPDATE_EXPECT=1` rewrites the `.out` files.
//!
//! Every study's work must also stay within the bound `check` promised.

use std::path::{Path, PathBuf};

use parallax::{Options, Report};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn check_work(path: &Path, report: &Report) {
    for s in &report.studies {
        let ops = s.work.operations.expect("a run reports its operations");
        assert!(
            ops <= s.work.estimated_operations,
            "{}: study `{}` did {ops} operations, over its bound of {}",
            path.display(),
            s.study,
            s.work.estimated_operations
        );
        for p in &s.policies {
            assert!(
                p.work.operations <= p.work.estimated_operations,
                "{}: policy `{}` did {} operations, over its bound of {}",
                path.display(),
                p.name,
                p.work.operations,
                p.work.estimated_operations
            );
        }
    }
}

fn golden(rel: &str) {
    let path = root().join(rel);
    let src = std::fs::read_to_string(&path).unwrap();
    let report = parallax::run(&src, &Options::default())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    check_work(&path, &report);
    let got = serde_json::to_string_pretty(&report).unwrap() + "\n";
    let out = path.with_extension("out");
    if std::env::var_os("UPDATE_EXPECT").is_some() {
        std::fs::write(&out, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&out)
        .unwrap_or_else(|_| panic!("{} is missing: run with UPDATE_EXPECT=1", out.display()));
    if got != want {
        let line = got
            .lines()
            .zip(want.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "{} differs from {} at line {}:\n  got:  {}\n  want: {}",
            path.display(),
            out.display(),
            line + 1,
            got.lines().nth(line).unwrap_or(""),
            want.lines().nth(line).unwrap_or("")
        );
    }
}

#[test]
fn language() {
    golden("tests/language.px");
}

#[test]
fn worlds() {
    golden("tests/worlds.px");
}

#[test]
fn prewarm() {
    golden("simulations/prewarm.px");
}

#[test]
fn reactor() {
    golden("simulations/reactor.px");
}

#[test]
fn circumbinary() {
    golden("simulations/circumbinary.px");
}

#[test]
fn island() {
    golden("simulations/island.px");
}

#[test]
fn tool_choice() {
    golden("simulations/tool_choice.px");
}

#[test]
fn tooluse() {
    golden("simulations/tooluse.px");
}

/// Every golden program above is listed: a new `.px` needs a test.
#[test]
fn every_program_is_tested() {
    let listed = [
        "language",
        "worlds",
        "prewarm",
        "reactor",
        "circumbinary",
        "island",
        "tool_choice",
        "tooluse",
    ];
    for dir in ["tests", "simulations"] {
        for entry in std::fs::read_dir(root().join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "px") {
                let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
                assert!(
                    listed.contains(&stem.as_str()),
                    "{} has no test",
                    path.display()
                );
            }
        }
    }
}

/// Every example in examples.md runs, or fails exactly as it says.
#[test]
fn examples() {
    let text = std::fs::read_to_string(root().join("examples.md")).unwrap();
    let mut blocks = 0;
    let mut rest = text.as_str();
    while let Some(start) = rest.find("```parallax") {
        let body = &rest[start + "```parallax".len()..];
        let end = body.find("```").expect("unterminated example");
        let code = &body[..end];
        rest = &body[end + 3..];
        blocks += 1;
        // An example that should be rejected says so on its first line:
        // `# error: <part of the message>`.
        let expect_err = code
            .lines()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| l.trim().strip_prefix("# error:"))
            .map(|s| s.trim().to_string());
        let r = parallax::run(code, &Options::default());
        match (r, expect_err) {
            (Ok(report), None) => check_work(Path::new("examples.md"), &report),
            (Err(e), Some(want)) => assert!(
                e.message.contains(&want),
                "example {blocks} failed with `{e}`, expected `{want}`:\n{code}"
            ),
            (Ok(_), Some(want)) => panic!("example {blocks} should fail with `{want}`:\n{code}"),
            (Err(e), None) => panic!("example {blocks} failed: {e}\n{code}"),
        }
    }
    assert!(blocks > 5, "examples.md has only {blocks} examples");
}

/// `check` describes every simulation and bounds its work without running.
#[test]
fn check_describes_programs() {
    for f in [
        "prewarm",
        "reactor",
        "circumbinary",
        "island",
        "tool_choice",
        "tooluse",
    ] {
        let src = std::fs::read_to_string(root().join(format!("simulations/{f}.px"))).unwrap();
        let c = parallax::check(&src, &Options::default()).unwrap();
        assert!(!c.studies.is_empty());
        for s in &c.studies {
            assert!(s.error.is_none(), "{f}: {:?}", s.error);
            let w = s.work.as_ref().unwrap();
            assert!(w.estimated_operations > 0 && w.estimated_transitions > 0);
            assert!(w.operations.is_none());
        }
    }
}
