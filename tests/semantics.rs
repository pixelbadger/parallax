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
fn floats() {
    // A float operand makes the result a float; two integers stay integers
    assert_eq!(
        run("print(1.5 + 1, 7 / 2, 7.0 / 2, 0.1 + 0.2, 3.0, 1 == 1.0, 0.5 < 1);"),
        "2.5 3 3.5 0.30000000000000004 3.0 1 1\n"
    );
    assert_eq!(
        run("print(sqrt(2), float(3), int(2.7), int(0.0 - 2.5), abs(0.0 - 1.5), min(1, 2.5));"),
        "1.4142135623730951 3.0 2 -3 1.5 1.0\n"
    );
    // `a[1].b` is still a member access, not the float `1.b`
    assert_eq!(
        run("type P = { b }; let a = [P { b: 4 }, P { b: 5 }]; print(a[1].b, 0.0 && 1);"),
        "5 0\n"
    );
    // Built-ins on unobserved values stay futures, like `abs`
    assert_eq!(
        run("let s = sqrt(open(16, 16)); let f = fork { s }; print(f, s);"),
        "4.0 4.0\n"
    );
}

#[test]
fn ranged_open_draws_floats_when_a_bound_is_one() {
    let src = "let ok = 1; repeat 500 { let d = observe open(0.5, 1); ok = ok && d > 0.49 && d < 1.01 && d * 4 == int(d * 4) == 0; } print(ok, open(2.5, 2.5));";
    assert_eq!(run(src), "1 2.5\n");
}

#[test]
fn multiverse_aggregates_floats() {
    let src = "let r = multiverse 4 { if (open < 50) { 1 } else { 0.5 } }; print(r.mean > 0.49 && r.mean < 1.01, r.min, r.max, r.hits);";
    assert_eq!(run(src), "1 0.5 1.0 4\n");
    // Integer universes still aggregate to integers
    assert_eq!(run("print(multiverse 3 { 2 }.mean);"), "2\n");
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
fn a_name_is_found_where_it_is_bound_when_read() {
    // Before a block's `let`, a name still means the outer binding
    let src = r#"
        let x = 1;
        fn f() = { print(x); let x = 2; x }
        print(f());
        repeat 2 { print(x); let x = 5; print(x); }
    "#;
    assert_eq!(run(src), "1\n2\n1\n5\n1\n5\n");
    // A function can use names bound after it
    assert_eq!(
        run("fn f() = { g() + y } fn g() = { 10 } let y = 5; print(f());"),
        "15\n"
    );
}

#[test]
fn globals_carry_over_between_programs() {
    let out = spl::with_stack(|| {
        let mut interp = spl::Interpreter::new(0, Vec::new());
        let first = interp.parse("fn f() = { later + 1 } let x = 1;").unwrap();
        interp.run(&first).unwrap();
        let second = interp.parse("let later = 41; print(f(), x);").unwrap();
        interp.run(&second).unwrap();
        String::from_utf8(interp.into_output()).unwrap()
    });
    assert_eq!(out, "42 1\n");
}

#[test]
fn a_let_in_a_fork_binds_in_the_forking_scope() {
    let src = r#"
        let b = fork { let y = 7; y };
        commit b;
        fn h() = { let c = fork { let z = 3; z }; commit c; z }
        print(y, h());
    "#;
    assert_eq!(run(src), "7 3\n");
    // A block that binds nothing has no scope of its own, so the fork
    // inside it forks the enclosing one
    let src = "let b = 0; if 1 { b = fork { let y = 1; y }; } commit b; print(y);";
    assert_eq!(run(src), "1\n");
    let src = "let b = 0; if 1 { let q = 0; b = fork { let y = 1; y }; } commit b; print(y);";
    assert_eq!(error(src), "Undefined variable 'y'");
}

#[test]
fn each_loop_iteration_has_a_fresh_scope() {
    let src = r#"
        let fs = [];
        let i = 0;
        while (i < 3) { let k = i * 10; fn get() = { k } fs = push(fs, get); i = i + 1; }
        fn call(f) = { f() }
        print(call(fs[0]), call(fs[1]), call(fs[2]));
        let saved = 0;
        repeat 3 { let v = i; saved = fork { v = v + 100; v }; i = i + 1; }
        commit saved;
        print(saved, i);
    "#;
    assert_eq!(run(src), "0 10 20\n105 6\n");
}

#[test]
fn an_index_is_evaluated_after_its_base() {
    let src = r#"
        let a = [1, 2, 3];
        fn bump() = { a = [7, 8, 9]; 1 }
        print(a[bump()], a[0]);
        let s = [[1, 2], [3, 4]];
        fn g() = { s[0][0] = 50; 0 }
        print(s[g()][0], s[0][0]);
    "#;
    assert_eq!(run(src), "2 7\n1 50\n");
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
        ("print(\"a\" + 1);", "'+' needs numbers, got 'a', 1"),
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
            "A universe must produce a number, a struct or an array, got s",
        ),
        ("fn f(n) = { f(n + 1) } f(0);", "Recursion too deep"),
        ("print(9223372036854775807 + 1);", "Integer overflow in '+'"),
        ("print(1.5 / 0);", "Division by zero"),
        (
            "let b = 1.0; repeat 400 { b = b * 10; }",
            "Float overflow in '*'",
        ),
        ("print(sqrt(0 - 1));", "sqrt() of a negative number: -1"),
        (
            "print(int(1.0 * 9223372036854775807 * 2));",
            "int() out of range: 1.8446744073709552e19",
        ),
        ("print([1][0.0]);", "array index needs an integer, got 0.0"),
        ("print(open(2.0, 1.0));", "open(2.0, 1.0): empty range"),
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

#[test]
fn arrays_have_value_semantics() {
    let src = r#"
        let a = [1, 2, 3];
        let b = a;
        b[0] = 10;
        fn bump(arr) = { arr[0] = arr[0] + 1; arr }
        let c = bump(a);
        print(a, b, c, len(a), a[2]);
    "#;
    assert_eq!(run(src), "[1, 2, 3] [10, 2, 3] [2, 2, 3] 3 3\n");
}

#[test]
fn nested_element_and_field_writes() {
    let src = r#"
        type P = { x, ys };
        let grid = array(2, array(3, 0));
        grid[1][2] = 5;
        let p = P { x: 1, ys: [7, 8] };
        p.ys[1] = 9;
        p.x = p.x + 1;
        print(grid, p);
    "#;
    assert_eq!(run(src), "[[0, 0, 0], [0, 0, 5]] P { x: 2, ys: [7, 9] }\n");
}

#[test]
fn push_returns_a_new_array() {
    assert_eq!(
        run("let a = []; let b = push(a, 1); print(a, b, len(push(b, \"s\")));"),
        "[] [1] 2\n"
    );
}

#[test]
fn array_elements_stay_unobserved_until_used() {
    let src = "let a = [open, 5]; let x : ?Int = a[0]; print(a[0] == a[0], a[1]);";
    assert_eq!(run(src), "1 5\n");
    // array(n, v) repeats one value: an Open value collapses once for all
    assert_eq!(
        run("let a = array(3, open); print(a[0] == a[1] && a[1] == a[2]);"),
        "1\n"
    );
}

#[test]
fn indexing_observes_the_index() {
    for seed in 0..10 {
        let out = run_seeded(
            "let a = [10, 20, 30]; let i = open(0, 2); print(a[i] == 10 + i * 10);",
            seed,
        );
        assert_eq!(out, "1\n");
    }
}

#[test]
fn forks_copy_arrays() {
    let src = r#"
        let a = [1, 2, 3];
        let f = fork { a[1] = 99; a[1] };
        print(f, a);
        commit f;
        print(a);
    "#;
    assert_eq!(run(src), "99 [1, 2, 3]\n[1, 99, 3]\n");
}

#[test]
fn multiverse_aggregates_arrays_elementwise() {
    let out =
        run("let m = multiverse 50 { [3, open(1, 6) > 0] }; print(m[0].mean, m[1].rate, len(m));");
    assert_eq!(out, "3 100 2\n");
    assert_eq!(
        error("let m = multiverse 4 { array(observe open(1, 2), 0) };"),
        "Every universe must produce the same kind of result"
    );
}

#[test]
fn array_errors() {
    let cases = [
        (
            "let a = [1]; print(a[1]);",
            "Index 1 out of range for an array of length 1",
        ),
        (
            "let a = [1]; a[0 - 1] = 2;",
            "Index -1 out of range for an array of length 1",
        ),
        ("let a = 1; a[0] = 2;", "Cannot index <1>"),
        ("let a = [1]; a.x = 1;", "Cannot access 'x' on Array[1]"),
        (
            "type P = { x }; let p = P { x: 1 }; p.y = 2;",
            "'P' has no field 'y'",
        ),
        ("print(len(3));", "len() needs an array, got <3>"),
        ("print([1] + 1);", "'+' needs numbers, got [1], 1"),
        ("q[0] = 1;", "Cannot assign to undefined variable 'q'"),
        (
            "let a = array(0 - 1, 0);",
            "array() length -1 is out of range",
        ),
        (
            "(1)[0] = 2;",
            "Line 1: Can only assign to a variable, an element or a field, got '='",
        ),
    ];
    for (src, expected) in cases {
        assert_eq!(error(src), expected, "for {src}");
    }
}
