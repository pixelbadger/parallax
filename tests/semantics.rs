//! Language semantics that hold for every seed.

use spl::{Error, run_source};

fn run(src: &str) -> String {
    run_source(src, 0).unwrap_or_else(|e| panic!("{e}\nin:\n{src}"))
}

fn run_seeded(src: &str, seed: u64) -> String {
    run_source(src, seed).unwrap_or_else(|e| panic!("{e}\nin:\n{src}"))
}

fn error(src: &str) -> String {
    match run_source(src, 0) {
        Ok(out) => panic!("expected an error, got output:\n{out}"),
        Err(Error::Syntax(e) | Error::Runtime(e)) => e,
    }
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(
        run("print(2 + 3 * 4, 20 / 3 - 1, 1 + 2 > 2, 1 && 0 || 1);"),
        "14 5 1 1\n"
    );
    // Division floors, like integer division should
    assert_eq!(
        run("print(0 - 7 / 2, (0 - 7) / 2, 7 / (0 - 2), (0 - 7) / (0 - 2));"),
        "-3 -4 -4 3\n"
    );
    assert_eq!(
        run("print(\"a\" == \"a\", \"a\" == \"b\", min(3, 9), max(3, 9), abs(0 - 4));"),
        "1 0 3 9 4\n"
    );
}

#[test]
fn observing_a_future_collapses_its_sources() {
    let out =
        run("let c = open; let d = c * 2; let seen = observe d; print(seen == c * 2, c < 100);");
    assert_eq!(out, "1 1\n");
}

#[test]
fn shared_sources_collapse_once() {
    for seed in 0..20 {
        assert_eq!(
            run_seeded("let a = open; let b = a + a; print(b == 2 * a);", seed),
            "1\n"
        );
    }
}

#[test]
fn ranged_open_respects_bounds() {
    let src = "let ok = 1; repeat 500 { let d = observe open(3, 5); ok = ok && d > 2 && d < 6; } print(ok);";
    assert_eq!(run(src), "1\n");
}

#[test]
fn closures_and_scoping() {
    let src = r#"
        let g = 1;
        fn get() = { g }
        fn mk() = { let local = 5; fn inner() = { local = local + 1; local } inner }
        let f = mk();
        print(f(), f(), get());
        if 1 { let g = 99; print(g); }
        print(g);
    "#;
    assert_eq!(run(src), "6 7 1\n99\n1\n");
}

#[test]
fn fork_is_isolated_until_committed() {
    let src = r#"
        let g = 1;
        fn get() = { g }
        let b = fork { g = 50; get() };
        print(b, g, get());
        commit b;
        print(g, get());
    "#;
    assert_eq!(run(src), "50 1 1\n50 50\n");
}

#[test]
fn commit_applies_only_what_the_fork_wrote() {
    let src = "let a = 1; let b = 1; let f = fork { a = 10; 0 }; b = 20; commit f; print(a, b);";
    assert_eq!(run(src), "10 20\n");
}

#[test]
fn commit_from_nested_block_reaches_forking_scope() {
    let src = r#"
        fn main() = {
            let level = 1;
            let attempt = fork { let level = level + 41; level };
            if (attempt == 42) { commit attempt; }
            print(level);
        }
    "#;
    assert_eq!(run(src), "42\n");
}

#[test]
fn committed_function_keeps_the_forks_scope() {
    let src = "let g = 1; let c = fork { fn later() = { g } g = 77; 0 }; commit c; g = 3; print(later(), g);";
    assert_eq!(run(src), "77 3\n");
}

#[test]
fn fork_does_not_collapse_parent_values() {
    let src = r#"
        let x = open;
        let y = fork { observe x };
        let z = fork { observe x };
        print(y == z);
    "#;
    // Both forks copy the same unobserved x and draw from the same stream
    assert_eq!(run(src), "1\n");
}

#[test]
fn forking_does_not_change_later_draws() {
    for seed in 0..10 {
        let src = "seed(5); let p = fork { observe open }; discard p; let r = observe open; seed(5); print(r == observe open);";
        assert!(run_seeded(src, seed).ends_with("1\n"));
    }
}

#[test]
fn forks_settle_once() {
    assert_eq!(
        error("let a = fork { 1 }; let b = a; commit a; commit b;"),
        "Cannot commit 'b': fork already settled"
    );
    assert_eq!(
        error("let p = 1; discard p;"),
        "Cannot discard 'p': not a fork"
    );
}

#[test]
fn multiverse_ensembles() {
    let out = run("let r = multiverse 4 { 7 }; print(r);");
    assert_eq!(
        out,
        "Ensemble { n: 4, rejected: 0, total: 28, mean: 7, min: 7, max: 7, median: 7, hits: 4, rate: 100 }\n"
    );
    let out = run("let r = multiverse 10 { given 0; 1 }; print(r.n, r.rejected, r.mean);");
    assert_eq!(out, "0 10 none\n");
}

#[test]
fn multiverse_struct_results_and_conditioning() {
    let src = r#"
        type Roll = { a, b };
        let r = multiverse 200 { let a = open(1, 6); given a > 3; Roll { a: a, b: open(1, 6) } };
        print(r.a.n + r.a.rejected, r.a.min > 3, r.b.min > 0, r.b.max < 7);
    "#;
    assert_eq!(run(src), "200 1 1 1\n");
}

#[test]
fn multiverses_share_draws_and_leave_the_outer_stream_alone() {
    let src = r#"
        seed(11);
        let first = multiverse 50 { open };
        let after = observe open;
        let again = multiverse 50 { open };
        seed(11);
        print(first.total == again.total, after == observe open);
    "#;
    assert!(run(src).ends_with("1 1\n"));
}

#[test]
fn pins_survive_reseeding_until_reset() {
    let src = "seed(10); pin k = open; seed(9999); pin k = 1000; print(k < 100); reset k; pin k = 1000; print(k);";
    assert_eq!(
        run(src),
        "[SYS] Seed: 10\n[SYS] Seed: 9999\n[SYS] Pinned 'k' retrieved.\n1\n1000\n"
    );
}

#[test]
fn annotations_warn_on_state_mismatch() {
    let out = run(
        "let w : ?Int = 5; let v : ~Int = open; let x : Int = open; let y : Any = open; let z : ~Int = open + 1;",
    );
    assert_eq!(
        out,
        "[WARN] w: Expected Open (?Int), got COLLAPSED\n\
         [WARN] v: Expected Future (~Int), got OPEN\n\
         [WARN] x: Expected Collapsed (Int), got OPEN\n"
    );
}

#[test]
fn loops_and_deep_recursion() {
    let src = r#"
        let total = 0; repeat 10 { total = total + 1; }
        let i = 0; while (i < 5) { i = i + 1; }
        fn count(n) = { if (n == 0) { 0 } else { 1 + count(n - 1) } }
        print(total, i, count(20000));
    "#;
    assert_eq!(run(src), "10 5 20000\n");
}

#[test]
fn long_future_chains_do_not_overflow() {
    let src = "let x = 0; repeat 200000 { x = x + open(1, 1); } let f = fork { x }; print(f);";
    assert_eq!(run(src), "200000\n");
}

#[test]
fn runtime_errors() {
    let cases = [
        ("print(1 / 0);", "Division by zero"),
        ("print(\"a\" + 1);", "'+' needs integers, got 'a', 1"),
        ("print(open(5, 1));", "open(5, 1): empty range"),
        ("type T = { a, a };", "Duplicate field in type 'T'"),
        (
            "type T = { a }; let t = T { a: 1, b: 2 };",
            "Bad fields for 'T': missing [], unknown ['b']",
        ),
        ("let t = Q { };", "Unknown type 'Q'"),
        ("print(nope());", "Undefined variable 'nope'"),
        ("x = 1;", "Cannot assign to undefined variable 'x'"),
        ("let x = 1; x(2);", "'x' is not a function"),
        (
            "fn f(a) = { a } f(1, 2);",
            "'f' expects 1 argument(s), got 2",
        ),
        ("print(min(1));", "min() takes 2 argument(s), got 1"),
        ("given 0;", "'given' condition failed outside a multiverse"),
        ("let p = 1; print(p.x);", "Cannot access 'x' on <1>"),
        (
            "let q = multiverse 3 { \"s\" };",
            "A universe must produce an integer or a struct, got s",
        ),
        ("fn f(n) = { f(n + 1) } f(0);", "Recursion too deep"),
        ("print(9223372036854775807 + 1);", "Integer overflow in '+'"),
    ];
    for (src, expected) in cases {
        assert_eq!(error(src), expected, "for {src}");
    }
}

#[test]
fn syntax_errors() {
    assert_eq!(error("let x = 1 @ 2;"), "Line 1: Illegal char '@ 2;...'");
    assert_eq!(error("let x = 1"), "Unexpected end of file. Expected SEMI");
    assert_eq!(
        error("if 1 { 2 } else if 0 { 3 }"),
        "Line 1: Expected LBRACE, got 'if'"
    );
}
