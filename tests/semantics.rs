//! Behaviour that holds for every seed, and programs the language rejects.

use parallax::study::StudyReport;
use parallax::{Error, ErrorKind, Limits, Options, Report};
use serde_json::{Value as Json, json};

fn run_with(src: &str, opts: &Options) -> Report {
    parallax::run(src, opts).unwrap_or_else(|e| panic!("{e}\n{src}"))
}

fn run(src: &str) -> Report {
    run_with(src, &Options::default())
}

fn run_seed(src: &str, seed: i64) -> Report {
    run_with(
        src,
        &Options {
            seed: Some(seed),
            ..Options::default()
        },
    )
}

fn fails(src: &str) -> Error {
    match parallax::run(src, &Options::default()) {
        Ok(_) => panic!("expected an error:\n{src}"),
        Err(e) => e,
    }
}

fn rejects(src: &str, kind: ErrorKind, part: &str) {
    let e = fails(src);
    assert_eq!(e.kind, kind, "{e}");
    assert!(e.message.contains(part), "`{e}` doesn't mention `{part}`");
}

/// A policy's metric by position, as JSON.
fn metric(s: &StudyReport, policy: &str, i: usize) -> Json {
    let p = s.policies.iter().find(|p| p.name == policy).unwrap();
    p.metrics[i].value.clone()
}

fn num(j: &Json) -> f64 {
    j.as_f64().unwrap()
}

// ---------------------------------------------------------------------
// Common worlds
// ---------------------------------------------------------------------

/// The policies differ in how much they compute and in what order they
/// read facts, yet every world gives each the same facts.
const ORDER: &str = r#"
action Order = forward | backward | late
world W {
    uncertain x(i: Int) ~ uniform(0, 1000000)
    latent shared ~ normal(0.0, 1.0)
}
model read(o: Order) -> Int = match o {
    forward => sum([W.x(i) for i in 0..10]),
    backward => sum([W.x(9 - i) for i in 0..10]),
    late => {
        let warmup = sum([i * i for i in 0..1000])
        if warmup > 0 { sum([W.x(i) for i in 0..10]) } else { 0 }
    }
}
policy a = forward
policy b = backward
policy c = late
study s { worlds 50 report mean(outcome), max(outcome) }
"#;

#[test]
fn every_policy_sees_the_same_worlds() {
    for seed in [0, 1, 2, 99] {
        let r = run_seed(ORDER, seed);
        let s = &r.studies[0];
        for i in 0..2 {
            assert_eq!(metric(s, "a", i), metric(s, "b", i));
            assert_eq!(metric(s, "a", i), metric(s, "c", i));
        }
        // and the worlds differ from each other
        assert!(s.policies[0].metrics[0].ci95.unwrap()[0] < num(&metric(s, "a", 0)));
    }
}

#[test]
fn paired_differences_between_identical_policies_are_zero() {
    let src = ORDER.replace(
        "report mean(outcome), max(outcome)",
        "maximize mean(outcome)",
    );
    let r = run(&src);
    for p in &r.studies[0].policies[1..] {
        let d = p.vs_recommended.as_ref().unwrap();
        assert_eq!(d.difference, 0.0);
        assert_eq!(d.ci95.unwrap(), [0.0, 0.0]);
    }
}

#[test]
fn seeds_replay_exactly_and_differ_from_each_other() {
    let a = serde_json::to_string(&run_seed(ORDER, 7)).unwrap();
    let b = serde_json::to_string(&run_seed(ORDER, 7)).unwrap();
    let c = serde_json::to_string(&run_seed(ORDER, 8)).unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn keys_are_names_not_declaration_order() {
    let moved = ORDER.replace(
        "    uncertain x(i: Int) ~ uniform(0, 1000000)\n    latent shared ~ normal(0.0, 1.0)",
        "    latent shared ~ normal(0.0, 1.0)\n    uncertain x(i: Int) ~ uniform(0, 1000000)",
    );
    assert_ne!(moved, ORDER);
    let a = &run(ORDER).studies[0];
    let b = &run(&moved).studies[0];
    assert_eq!(metric(a, "a", 0), metric(b, "a", 0));
    // A different world name is a different scenario namespace.
    let renamed = ORDER.replace("W.", "V.").replace("world W", "world V");
    let c = &run(&renamed).studies[0];
    assert_ne!(metric(a, "a", 0), metric(c, "a", 0));
}

#[test]
fn provenance_names_the_worlds() {
    let r = run(ORDER);
    let p = &r.studies[0].provenance;
    assert_eq!(p.worlds, 50);
    assert_eq!(p.world_ids, [0, 49]);
    assert_eq!(p.model_sha256, r.model_sha256);
    assert_eq!(p.model_sha256.len(), 64);
    assert!(p.runtime.starts_with("parallax "));
}

// ---------------------------------------------------------------------
// Information boundaries, forecasts and oracles
// ---------------------------------------------------------------------

/// Guess a coin each step. A forecaster can't know the coming flip; an
/// oracle can; an observed flip is held fixed in a forecast.
const COIN: &str = r#"
action Guess = heads | tails
type Score = { right: Int }
type Seen = { last: Bool }
world C { uncertain flip(t: Int) ~ bernoulli(0.5) }
model game {
    horizon 20
    init = Score { right: 0 }
    step(s: Score, g: Guess, t: Int) -> Score =
        Score { right: s.right + if (g == heads) == C.flip(t) { 1 } else { 0 } }
    observe(s: Score, t: Int) -> Seen = Seen { last: C.flip(t - 1) }
    belief(o: Seen, t: Int) -> Score = Score { right: 0 }
}
policy forecaster(o: Seen, t: Int) -> Guess =
    if forecast(heads, horizon: 1, worlds: 1)[0].right == 1 { heads } else { tails }
oracle policy psychic(s: Score, t: Int) -> Guess =
    if rollout(heads, horizon: 1).right > s.right { heads } else { tails }
study s { worlds 200 maximize mean(right) }
"#;

#[test]
fn a_forecast_imagines_its_own_future_and_an_oracle_sees_the_real_one() {
    for seed in [0, 1, 2] {
        let s = &run_seed(COIN, seed).studies[0];
        let get =
            |n: &str| num(&s.policies.iter().find(|p| p.name == n).unwrap().objectives[0].value);
        assert_eq!(get("psychic"), 20.0);
        let f = get("forecaster");
        assert!(f > 7.0 && f < 13.0, "forecaster scored {f}");
        assert_eq!(s.best_oracle.as_deref(), Some("psychic"));
        assert_eq!(s.recommended.as_deref(), Some("forecaster"));
    }
}

#[test]
fn a_forecast_keeps_what_was_observed() {
    // The policy guesses last step's flip, which it observed: a forecast
    // that re-reads it must agree with the real one every time.
    let src = r#"
action Guess = heads | tails
type Seen = { last: Bool }
type Score = { right: Int, agreed: Int }
world C { uncertain flip(t: Int) ~ bernoulli(0.5) }
model game {
    horizon 20
    init = Score { right: 0, agreed: 0 }
    step(s: Score, g: Guess, t: Int) -> Score = Score {
        right: s.right + if (g == heads) == C.flip(t - 1) { 1 } else { 0 },
        agreed: s.agreed,
    }
    observe(s: Score, t: Int) -> Seen = Seen { last: C.flip(t - 1) }
    belief(o: Seen, t: Int) -> Score = Score { right: 0, agreed: 0 }
}
policy recall(o: Seen, t: Int) -> Guess =
    if forecast(heads, horizon: 1, worlds: 3)[2].right == 1 { heads } else { tails }
study s { worlds 100 maximize mean(right) }
"#;
    let s = &run(src).studies[0];
    assert_eq!(num(&s.policies[0].objectives[0].value), 20.0);
}

#[test]
fn policies_cannot_read_the_world() {
    rejects(
        &COIN.replace(
            "if forecast(heads, horizon: 1, worlds: 1)[0].right == 1",
            "if C.flip(t)",
        ),
        ErrorKind::Check,
        "a policy can't read the world",
    );
    rejects(
        &COIN.replace(
            "policy forecaster(o: Seen, t: Int) -> Guess =",
            "fn peek(t: Int) -> Bool = C.flip(t)\npolicy forecaster(o: Seen, t: Int) -> Guess =",
        ),
        ErrorKind::Check,
        "functions are pure",
    );
    rejects(
        &COIN.replace(
            "if forecast(heads, horizon: 1, worlds: 1)[0].right == 1",
            "if rollout(heads, horizon: 1).right == 1",
        ),
        ErrorKind::Check,
        "only an `oracle policy`",
    );
}

#[test]
fn observe_reveals_facts_whole_and_never_latent_ones() {
    rejects(
        &COIN.replace(
            "Seen { last: C.flip(t - 1) }",
            "Seen { last: C.flip(t - 1) and C.flip(t) }",
        ),
        ErrorKind::Check,
        "must reveal a world fact whole",
    );
    rejects(
        &COIN
            .replace(
                "world C { uncertain",
                "world C { latent bias ~ bernoulli(0.5)\n uncertain",
            )
            .replace("Seen { last: C.flip(t - 1) }", "Seen { last: C.bias }"),
        ErrorKind::Check,
        "latent",
    );
}

#[test]
fn a_belief_is_required_when_the_observation_isnt_the_state() {
    rejects(
        &COIN.replace(
            "    belief(o: Seen, t: Int) -> Score = Score { right: 0 }\n",
            "",
        ),
        ErrorKind::Check,
        "add `belief",
    );
}

// ---------------------------------------------------------------------
// Termination and budgets
// ---------------------------------------------------------------------

const BUDGET: &str = r#"
action A = go
world W { uncertain n ~ uniform(1, 100) }
fn work(n: Int) -> Int = sum([i for i in 0..n])
model m(a: A) -> Int = work(50)
policy p = go
study s { worlds 1000 report mean(outcome) }
"#;

#[test]
fn recursion_is_rejected() {
    rejects(
        "fn f(n: Int) -> Int = if n == 0 { 0 } else { f(n - 1) }\nfn g() -> Int = f(3)",
        ErrorKind::Check,
        "recursion is not allowed",
    );
    rejects(
        "fn f(n: Int) -> Int = g(n)\nfn g(n: Int) -> Int = f(n)",
        ErrorKind::Check,
        "recursion is not allowed",
    );
}

#[test]
fn there_is_no_unbounded_loop() {
    rejects(
        "fn f() -> Int = { var i = 0\n while i < 10 { i = i + 1 }\n i }",
        ErrorKind::Syntax,
        "no `while` loop",
    );
    // An integer drawn from a range bounds a loop...
    run(&BUDGET.replace(
        "model m(a: A) -> Int = work(50)",
        "model m(a: A) -> Int = work(W.n)",
    ));
    // ...but one computed from an unbounded draw can't be budgeted.
    rejects(
        &BUDGET
            .replace(
                "uncertain n ~ uniform(1, 100)",
                "uncertain n ~ normal(50.0, 10.0)",
            )
            .replace(
                "model m(a: A) -> Int = work(50)",
                "model m(a: A) -> Int = work(round(W.n))",
            ),
        ErrorKind::Budget,
        "can't bound the work",
    );
}

#[test]
fn work_is_bounded_before_running() {
    let c = parallax::check(BUDGET, &Options::default()).unwrap();
    let w = c.studies[0].work.as_ref().unwrap();
    let r = run(BUDGET);
    let ops = r.studies[0].work.operations.unwrap();
    assert!(
        ops <= w.estimated_operations,
        "{ops} > {}",
        w.estimated_operations
    );
    assert!(
        ops * 2 > w.estimated_operations,
        "a loose bound: {ops} of {}",
        w.estimated_operations
    );
    assert_eq!(w.estimated_transitions, 1000);
    // The host's limits refuse a study before it runs.
    let tight = Options {
        limits: Limits {
            max_operations: 1000.0,
            ..Limits::default()
        },
        ..Options::default()
    };
    match parallax::run(BUDGET, &tight) {
        Err(e) => assert_eq!(e.kind, ErrorKind::Budget),
        Ok(_) => panic!("ran over budget"),
    }
    let few = Options {
        limits: Limits {
            max_worlds: 10,
            ..Limits::default()
        },
        ..Options::default()
    };
    assert_eq!(
        parallax::run(BUDGET, &few).unwrap_err().kind,
        ErrorKind::Budget
    );
}

// ---------------------------------------------------------------------
// Types and units
// ---------------------------------------------------------------------

#[test]
fn units_are_checked() {
    rejects(
        "const X = 5 min + 3 kWh",
        ErrorKind::Check,
        "units don't match",
    );
    rejects(
        "fn f(x: W) -> kWh = x",
        ErrorKind::Check,
        "expected kWh, found W",
    );
    rejects(
        "const X = (5 min) in kWh",
        ErrorKind::Check,
        "can't express",
    );
    rejects("const X = sqrt(2 s)", ErrorKind::Check, "square root");
    rejects("const X = exp(2 s)", ErrorKind::Check, "without units");
    rejects("const X = 1 furlong", ErrorKind::Syntax, "");
}

#[test]
fn units_convert() {
    let src = r#"
action A = go
model m(a: A) -> Float = 0.0
policy p = go
study s {
    worlds 1
    report 1 kWh in J, 90 min + 1 h in h, 29 p/kWh * 2 kWh in £, 30% in %, (2 m)^2 in cm^2, 1 W * 1 h in J
}
"#;
    let s = &run(src).studies[0];
    let vals: Vec<f64> = (0..6).map(|i| num(&metric(s, "p", i))).collect();
    assert_eq!(vals, vec![3.6e6, 2.5, 0.58, 30.0, 40000.0, 3600.0]);
}

#[test]
fn types_are_checked() {
    rejects(
        "type B = { cap: kWh }\nconst X = B { cap: 5 }",
        ErrorKind::Check,
        "expected kWh",
    );
    rejects(
        "type B = { cap: kWh }\nconst X = B { }",
        ErrorKind::Check,
        "missing field cap",
    );
    rejects(
        "enum F = a | b\nfn f(x: F) -> Int = match x { a => 1 }",
        ErrorKind::Check,
        "doesn't cover b",
    );
    rejects(
        "fn f() -> Int = { let x = 1\n x = 2\n x }",
        ErrorKind::Check,
        "immutable",
    );
    rejects(
        "fn f() -> Int = 1 + true",
        ErrorKind::Check,
        "expected numbers",
    );
    rejects(
        "fn f() -> Bool = 1 < 2 < 3",
        ErrorKind::Syntax,
        "don't chain",
    );
    rejects(
        "fn f() = print(1)",
        ErrorKind::Check,
        "unknown function `print`",
    );
    rejects(
        "fn f() -> Float = 7 // 2.0",
        ErrorKind::Check,
        "`//` divides integers",
    );
}

#[test]
fn uncertainty_enters_only_in_worlds() {
    rejects(
        "fn f() -> Float = uniform(0.0, 1.0)",
        ErrorKind::Check,
        "uncertainty enters only in a `world`",
    );
    rejects(
        "world W { uncertain x ~ 5 }",
        ErrorKind::Check,
        "expected a distribution",
    );
}

#[test]
fn actions_are_declared_as_actions() {
    rejects(
        "model m(a: Int) -> Int = a\npolicy p = 1\nstudy s { worlds 1 }",
        ErrorKind::Check,
        "declared with `action`",
    );
}

// ---------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------

const INPUTS: &str = r#"
input temp: °C between -30 °C and 40 °C
input soc: % = 50%
input names: [Int; 2] = [1, 2]
action A = go
model m(a: A) -> °C = temp * soc
policy p = go
study s { worlds 1 report mean(outcome) }
"#;

fn with_inputs(pairs: &[(&str, Json)]) -> Result<Report, Error> {
    let mut opts = Options::default();
    for (k, v) in pairs {
        opts.inputs.insert(k.to_string(), v.clone());
    }
    parallax::run(INPUTS, &opts)
}

#[test]
fn inputs_follow_their_schema() {
    let e = with_inputs(&[]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Input);
    assert!(e.message.contains("`temp` (°C) is required"), "{e}");
    let r = with_inputs(&[("temp", json!(10))]).unwrap();
    assert_eq!(num(&metric(&r.studies[0], "p", 0)), 5.0);
    assert_eq!(
        r.studies[0].inputs["temp"],
        json!({"value": 10.0, "unit": "°C"})
    );
    // a quantity can be written with its unit, in any compatible unit
    let r = with_inputs(&[("temp", json!("10 °C")), ("soc", json!("0.25"))]).unwrap();
    assert_eq!(num(&metric(&r.studies[0], "p", 0)), 2.5);
    let r = with_inputs(&[("temp", json!(20)), ("soc", json!(10))]).unwrap();
    assert_eq!(num(&metric(&r.studies[0], "p", 0)), 2.0);
    for (pairs, part) in [
        (vec![("temp", json!(50))], "outside"),
        (vec![("temp", json!("10 kWh"))], "isn't in units of"),
        (vec![("temp", json!(true))], "should be °C"),
        (
            vec![("temp", json!(1)), ("names", json!([1, 2, 3]))],
            "needs 2 elements",
        ),
        (
            vec![("temp", json!(1)), ("pressure", json!(1))],
            "unknown input `pressure`",
        ),
    ] {
        let e = with_inputs(&pairs).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Input, "{e}");
        assert!(e.message.contains(part), "`{e}` doesn't mention `{part}`");
    }
}

#[test]
fn check_describes_the_input_schema() {
    let c = parallax::check(INPUTS, &Options::default()).unwrap();
    let temp = &c.inputs[0];
    assert_eq!(temp.name, "temp");
    assert_eq!(temp.unit.as_deref(), Some("°C"));
    assert_eq!(temp.between, Some([json!(-30.0), json!(40.0)]));
    assert!(temp.required);
    assert!(!c.inputs[1].required);
    // the study can't be sized without its input
    assert_eq!(c.studies[0].error.as_ref().unwrap().kind, ErrorKind::Input);
}

// ---------------------------------------------------------------------
// Studies
// ---------------------------------------------------------------------

#[test]
fn model_errors_are_reported_against_worlds_and_steps() {
    let src = r#"
action A = careful | reckless
type S = { level: Int }
world W { uncertain spike(t: Int) ~ bernoulli(0.3) }
model tank {
    horizon 10
    init = S { level: 0 }
    step(s: S, a: A, t: Int) -> S = {
        let add = if a == reckless and W.spike(t) { 5 } else { 1 }
        assert s.level + add < 20, "the tank overflowed"
        S { level: s.level + add }
    }
    observe(s: S, t: Int) -> S = s
}
policy safe(o: S, t: Int) -> A = careful
policy risky(o: S, t: Int) -> A = reckless
study s { worlds 100 maximize mean(level) }
"#;
    let s = &run(src).studies[0];
    let risky = s.policies.iter().find(|p| p.name == "risky").unwrap();
    assert_eq!(risky.status, "model_errors");
    assert!(risky.model_errors > 0);
    assert_eq!(risky.rank, None);
    assert_eq!(risky.worlds_completed + risky.model_errors, 100);
    let e = &s.errors[0];
    assert_eq!(e.policy, "risky");
    assert_eq!(e.message, "the tank overflowed");
    assert!(e.world.is_some() && e.step.is_some() && e.line.is_some());
    assert_eq!(s.recommended.as_deref(), Some("safe"));
    assert!(s.errors.len() <= 5);
}

#[test]
fn constraints_decide_feasibility_and_objectives_rank() {
    let src = r#"
action Bet = small | medium | big
world C { uncertain win ~ bernoulli(0.6) }
type R = { gain: Int, ruined: Bool }
model play(b: Bet) -> R = {
    let stake = match b { small => 1, medium => 5, big => 20 }
    R { gain: if C.win { stake } else { -stake }, ruined: not C.win and stake > 10 }
}
policy s = small
policy m = medium
policy b = big
study st {
    worlds 1000
    require probability(ruined) <= 1%
    maximize mean(gain)
}
"#;
    let s = &run(src).studies[0];
    let status: Vec<&str> = s.policies.iter().map(|p| p.status).collect();
    assert_eq!(status, ["ok", "ok", "infeasible"]);
    assert_eq!(s.ranking, ["m", "s"]);
    assert_eq!(s.recommended.as_deref(), Some("m"));
    let b = &s.policies[2];
    assert_eq!(b.constraints[0].satisfied, Some(false));
}

#[test]
fn integer_overflow_is_a_model_error() {
    let src = r#"
action A = go
world W { uncertain big ~ uniform(4000000000000000000, 5000000000000000000) }
model m(a: A) -> Int = W.big * 3
policy p = go
study s { worlds 3 report mean(outcome) }
"#;
    let s = &run(src).studies[0];
    assert_eq!(s.policies[0].model_errors, 3);
    assert_eq!(s.errors[0].message, "integer overflow");
}
