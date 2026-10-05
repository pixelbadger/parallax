//! Serving decisions: `decide` follows a chosen policy once, now, and
//! agrees with what studies do.

use std::io::Write;
use std::process::{Command, Stdio};

use parallax::{Engine, ErrorKind, Options, Request};
use serde_json::{Map, Value as Json, json};

fn req(policy: &str) -> Request {
    Request {
        policy: policy.into(),
        ..Request::default()
    }
}

fn inputs(pairs: &[(&str, Json)]) -> Map<String, Json> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn tool_choice() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("simulations/tool_choice.px"),
    )
    .unwrap()
}

/// A decision model's policy decides once, from inputs: served with the
/// study's seed, it chooses exactly what the study reported.
#[test]
fn a_served_decision_matches_the_study() {
    let src = tool_choice();
    let engine = Engine::new(&src).unwrap();
    for (with, situation) in [
        (vec![], vec![]),
        (
            vec![("fresh", json!(true)), ("confidence", json!(90))],
            vec![("fresh", json!(true)), ("confidence", json!(90))],
        ),
        (
            vec![("confidence", json!(96))],
            vec![("confidence", json!(96))],
        ),
    ] {
        let opts = Options {
            inputs: inputs(&with),
            study: Some("unsure_question".into()),
            ..Options::default()
        };
        let report = parallax::run(&src, &opts).unwrap();
        let study = &report.studies[0];
        for p in &study.policies {
            let d = engine
                .decide(&Request {
                    inputs: inputs(&situation),
                    seed: Some(study.provenance.seed),
                    ..req(&p.name)
                })
                .unwrap();
            assert_eq!(Some(&d.action), p.action.as_ref(), "{}", p.name);
            assert!(d.work.operations <= d.work.estimated_operations);
        }
    }
}

#[test]
fn notes_give_the_reasons_with_units() {
    let d = parallax::decide(&tool_choice(), &req("adaptive")).unwrap();
    assert_eq!(d.action, json!("search"));
    let n = &d.notes;
    assert_eq!(n["search_saves"]["unit"], json!("s"));
    assert!(n["search_saves"]["value"].as_f64().unwrap() > 0.0);
    assert_eq!(n["predicted_error_direct"]["unit"], json!("%"));
    let worlds = n["worlds"].as_i64().unwrap();
    assert!([16, 64, 256, 1024].contains(&worlds), "{worlds}");
    // A fast path simulates nothing and says why.
    let fast = parallax::decide(
        &tool_choice(),
        &Request {
            inputs: inputs(&[("fresh", json!(true))]),
            ..req("adaptive")
        },
    )
    .unwrap();
    assert_eq!(fast.action, json!("search_and_check"));
    assert!(fast.notes["why"].as_str().unwrap().contains("fresh"));
    assert!(fast.work.operations < 20);
    // Policies without notes have none, and the same request replays exactly.
    let a = parallax::decide(&tool_choice(), &req("always_direct")).unwrap();
    assert!(a.notes.is_empty());
    let again = |seed| {
        serde_json::to_string(
            &parallax::decide(
                &tool_choice(),
                &Request {
                    seed,
                    ..req("adaptive")
                },
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(again(Some(4)), again(Some(4)));
}

/// `skip` extends a forecast with the next imagined worlds.
#[test]
fn skip_continues_the_same_imagined_worlds() {
    let src = r#"
action Go = go
world W { uncertain x ~ normal(0.0, 1.0) }
model m(a: Go) -> Float = W.x
policy check = {
    let every = forecast(go, worlds: 8)
    let first = forecast(go, worlds: 3)
    let rest = forecast(go, worlds: 5, skip: 3)
    assert all([every[i] == first[i] for i in 0..3]), "first"
    assert all([every[i + 3] == rest[i] for i in 0..5]), "rest"
    assert first[0] != rest[0], "distinct"
    note n = len(rest)
    go
}
study s { worlds 5 report mean(outcome) }
"#;
    let r = parallax::run(src, &Options::default()).unwrap();
    assert_eq!(r.studies[0].policies[0].status, "ok");
    let d = parallax::decide(src, &req("check")).unwrap();
    assert_eq!(d.notes["n"], json!(5));
}

/// What `observe` revealed keeps its observed value in a served
/// decision's forecasts; everything else is imagined.
const MARKET: &str = r#"
type Stall = { held: Bool, proceeds: Float }
type Seen = { price: Float, held: Bool }
action Choice = sell | hold
world Market {
    latent trend ~ uniform(-0.5, 1.0)
    uncertain noise(day: Int) ~ normal(0.0, 1.0)
    derived price(day: Int) = 10.0 + trend * float(day) + noise(day)
}
model market {
    horizon 5
    init = Stall { held: true, proceeds: 0.0 }
    step(s: Stall, a: Choice, t: Int) -> Stall =
        if s.held and a == sell { Stall { held: false, proceeds: Market.price(t) } } else { s }
    observe(s: Stall, t: Int) -> Seen = Seen { price: Market.price(t), held: s.held }
    belief(o: Seen, t: Int) -> Stall = Stall { held: o.held, proceeds: 0.0 }
}
policy look(o: Seen, t: Int) -> Choice = {
    let now = forecast(sell, horizon: 1, worlds: 4)
    let later = forecast(hold, horizon: 2, worlds: 4, then: sell)
    note now = now[0].proceeds
    note spread_now = max([r.proceeds for r in now]) - min([r.proceeds for r in now])
    note later_spread = max([r.proceeds for r in later]) - min([r.proceeds for r in later])
    if now[0].proceeds >= mean([r.proceeds for r in later]) { sell } else { hold }
}
study s { worlds 50 maximize mean(proceeds) }
"#;

#[test]
fn observed_facts_are_pinned_in_forecasts() {
    let d = parallax::decide(
        MARKET,
        &Request {
            observation: Some(json!({"price": 12.5, "held": true})),
            step: Some(2),
            ..req("look")
        },
    )
    .unwrap();
    assert_eq!(d.notes["now"], json!(12.5));
    assert_eq!(d.notes["spread_now"], json!(0.0));
    assert!(d.notes["later_spread"].as_f64().unwrap() > 0.0);
    assert!(d.unpinned.is_empty());
    assert_eq!(d.step, Some(2));
}

#[test]
fn history_pins_earlier_observations() {
    // A model whose forecast reads yesterday's price.
    let src = MARKET.replace(
        "Stall { held: false, proceeds: Market.price(t) }",
        "Stall { held: false, proceeds: Market.price(t - 1) }",
    );
    let at = |history: Vec<parallax::decide::Observed>| {
        parallax::decide(
            &src,
            &Request {
                observation: Some(json!({"price": 12.5, "held": true})),
                step: Some(2),
                history,
                ..req("look")
            },
        )
        .unwrap()
    };
    assert_ne!(at(vec![]).notes["now"], json!(9.0));
    let d = at(vec![parallax::decide::Observed {
        step: 1,
        observation: json!({"price": 9.0, "held": true}),
    }]);
    assert_eq!(d.notes["now"], json!(9.0));
}

#[test]
fn facts_keyed_by_the_state_are_reported_unpinned() {
    let src = MARKET
        .replace(
            "type Stall = { held: Bool,",
            "type Stall = { day: Int, held: Bool,",
        )
        .replace("Stall { held: true,", "Stall { day: 0, held: true,")
        .replace("Stall { held: false,", "Stall { day: t, held: false,")
        .replace("Stall { held: o.held,", "Stall { day: t, held: o.held,")
        .replace("price: Market.price(t)", "price: Market.price(s.day)");
    let d = parallax::decide(
        &src,
        &Request {
            observation: Some(json!({"price": 12.5, "held": true})),
            step: Some(2),
            ..req("look")
        },
    )
    .unwrap();
    assert_eq!(d.unpinned, vec!["Market.price".to_string()]);
}

fn refuses(src: &str, r: Request, kind: ErrorKind, part: &str) {
    match parallax::decide(src, &r) {
        Ok(d) => panic!("expected `{part}`, got {d:?}"),
        Err(e) => {
            assert_eq!(e.kind, kind, "{e}");
            assert!(e.message.contains(part), "`{e}` doesn't mention `{part}`");
        }
    }
}

#[test]
fn bad_requests_are_refused() {
    let seen = || Request {
        observation: Some(json!({"price": 12.5, "held": true})),
        step: Some(1),
        ..req("look")
    };
    refuses(
        MARKET,
        req("nope"),
        ErrorKind::Input,
        "no policy named `nope`",
    );
    refuses(MARKET, req("look"), ErrorKind::Input, "give its `step`");
    refuses(
        MARKET,
        Request {
            observation: None,
            ..seen()
        },
        ErrorKind::Input,
        "needs an `observation`",
    );
    refuses(
        MARKET,
        Request {
            step: Some(5),
            ..seen()
        },
        ErrorKind::Input,
        "outside model `market`'s horizon",
    );
    refuses(
        MARKET,
        Request {
            observation: Some(json!({"price": "high", "held": true})),
            ..seen()
        },
        ErrorKind::Input,
        "the observation.price",
    );
    let src = tool_choice();
    refuses(
        &src,
        Request {
            step: Some(0),
            ..req("adaptive")
        },
        ErrorKind::Input,
        "decides once",
    );
    refuses(
        &src,
        Request {
            inputs: inputs(&[("confidence", json!(150))]),
            ..req("adaptive")
        },
        ErrorKind::Input,
        "outside",
    );
    // Oracles see the real future, and families need a member.
    let worlds =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/worlds.px")).unwrap();
    refuses(&worlds, req("prophet"), ErrorKind::Input, "is an oracle");
    refuses(
        &worlds,
        req("threshold"),
        ErrorKind::Input,
        "threshold[10.5]",
    );
    let ok = parallax::decide(
        &worlds,
        &Request {
            observation: Some(json!({"price": 11.0, "held": true})),
            step: Some(0),
            ..req("threshold[10.5]")
        },
    )
    .unwrap();
    assert_eq!(ok.action, json!("sell"));
}

#[test]
fn a_decision_is_bounded_before_it_runs() {
    let mut engine = Engine::new(&tool_choice()).unwrap();
    engine.limits.max_operations = 1000.0;
    let e = engine.decide(&req("adaptive")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Budget);
    assert!(engine.decide(&req("always_direct")).is_ok());
}

#[test]
fn a_failing_policy_is_a_model_error() {
    let src = r#"
action Go = go
world W { uncertain x ~ normal(0.0, 1.0) }
model m(a: Go) -> Float = W.x
input n: Int = 1
policy picky = {
    assert n > 1, "n must be over 1"
    go
}
study s { worlds 5 report mean(outcome) }
"#;
    let e = parallax::decide(src, &req("picky")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Model);
    assert!(e.message.contains("n must be over 1"));
    assert_eq!(e.line, Some(7));
}

#[test]
fn note_belongs_to_policies() {
    let e = parallax::compile("fn f() -> Int = {\n    note x = 1\n    1\n}").unwrap_err();
    assert!(e.message.contains("use it in a policy"), "{e}");
    // `note` stays an ordinary name elsewhere.
    parallax::compile("fn f(note: Int) -> Int = note + 1").unwrap();
}

/// `parallax serve` answers one JSON line per request, in order.
#[test]
fn serve_answers_each_line() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/simulations/tool_choice.px");
    let mut child = Command::new(env!("CARGO_BIN_EXE_parallax"))
        .args(["serve", path, "--input", "stakes=30 s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"id": 1, "policy": "adaptive"}}"#).unwrap();
    writeln!(stdin).unwrap();
    writeln!(
        stdin,
        r#"{{"id": "b", "policy": "adaptive", "inputs": {{"fresh": true, "stakes": "2 min"}}}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"id": 3, "policy": "oracle?"}}"#).unwrap();
    writeln!(stdin, "not json").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines: Vec<Json> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["id"], json!(1));
    assert_eq!(lines[1]["id"], json!("b"));
    assert_eq!(lines[1]["action"], json!("search_and_check"));
    assert_eq!(lines[2]["id"], json!(3));
    assert_eq!(lines[2]["error"]["kind"], json!("input"));
    assert!(
        lines[3]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bad request")
    );
}
