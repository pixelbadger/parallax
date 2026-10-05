//! Serving a chosen policy: one decision, now.
//!
//! A study answers "which policy should we use?" by running every candidate
//! through thousands of common worlds. Once a policy is chosen, an agent
//! asks something else, many times: "given that policy and what I know now,
//! what should I do?" That is one call of the policy, with its forecasts,
//! and no evaluation worlds at all.
//!
//! An [`Engine`] holds a checked program, so a host that serves many
//! decisions parses and checks it once. Each [`Request`] gives the inputs
//! (the current situation) and, for a sequential model, what the policy
//! observes and the step. The decision's work is bounded before it runs.
//!
//! A sequential model's `observe` reveals facts, and in a study a forecast
//! holds those at their real values. Served, there is no simulated real
//! world: the observation *is* the real value, so a fact an observation
//! field reveals is pinned to that field's value in the forecasts. Earlier
//! observations can be given as `history` to pin what they revealed too.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::cost::Analyzer;
use crate::error::{Error, ErrorKind, Result};
use crate::eval::Machine;
use crate::ir::*;
use crate::study::{
    Limits, RUNTIME, fault_err, globals, horizons, input_ids, json_value, sha256, short, typed_json,
};
use crate::value::Value;

/// One decision to serve.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Echoed in the response, for hosts that pipeline requests.
    #[serde(default)]
    pub id: Option<Json>,
    /// The policy, as a study names it: `name`, or `name[param]` in a family.
    pub policy: String,
    /// Input values by name, as for a study.
    #[serde(default)]
    pub inputs: Map<String, Json>,
    /// A sequential policy's observation now.
    #[serde(default)]
    pub observation: Option<Json>,
    /// A sequential policy's step number now.
    #[serde(default)]
    pub step: Option<i64>,
    /// Earlier observations, whose revealed facts forecasts also hold.
    #[serde(default)]
    pub history: Vec<Observed>,
    /// Which worlds forecasts imagine (default 0).
    #[serde(default)]
    pub seed: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observed {
    pub step: i64,
    pub observation: Json,
}

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Json>,
    pub runtime: String,
    pub model_sha256: String,
    pub inputs_sha256: String,
    pub policy: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<i64>,
    pub seed: i64,
    /// What to do, with its unit if it has one.
    pub action: Json,
    /// The policy's `note`s: its reasons, by name.
    pub notes: Map<String, Json>,
    /// Facts an observation revealed that forecasts couldn't hold fixed
    /// (their key depends on the state), so they were redrawn.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unpinned: Vec<String>,
    pub work: DecisionWork,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionWork {
    pub estimated_operations: u64,
    pub operations: u64,
    pub max_forecast_worlds: u64,
}

/// A checked program, ready to serve decisions.
pub struct Engine {
    program: Program,
    model_sha256: String,
    /// The host's budget for one decision.
    pub limits: Limits,
}

impl Engine {
    pub fn new(src: &str) -> Result<Engine> {
        Ok(Engine {
            program: crate::compile(src)?,
            model_sha256: sha256(src.as_bytes()),
            limits: Limits::default(),
        })
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn decide(&self, req: &Request) -> Result<Decision> {
        let p = &self.program;
        input_ids(p, &req.inputs)?;
        let (globals, shown) = globals(p, &[], &req.inputs)?;
        let mut m = Machine::new(p, globals.clone(), 0);
        let horizons = horizons(p, &mut m, &self.limits, 0)?;
        let (pid, param) = self.policy(&mut m, &req.policy)?;
        let pol = &p.policies[pid as usize];
        let model = &p.models[pol.model as usize];
        let input_err = |msg: String| Error::new(ErrorKind::Input, msg);

        let seen = match model {
            ModelDef::Decision { .. } => {
                if req.observation.is_some() || req.step.is_some() || !req.history.is_empty() {
                    return Err(input_err(format!(
                        "`{}` decides once for model `{}`, from inputs only: it takes no \
                         observation, step or history",
                        req.policy,
                        model.name()
                    )));
                }
                None
            }
            ModelDef::Sequential(sm) => {
                let horizon = horizons[pol.model as usize];
                let step_ok = |t: i64| -> Result<i64> {
                    if (0..horizon).contains(&t) {
                        Ok(t)
                    } else {
                        Err(input_err(format!(
                            "step {t} is outside model `{}`'s horizon 0..{horizon}",
                            sm.name
                        )))
                    }
                };
                let t = step_ok(req.step.ok_or_else(|| {
                    input_err(format!(
                        "`{}` acts at each step of model `{}`: give its `step` and `observation`",
                        req.policy, sm.name
                    ))
                })?)?;
                let obs_json = req.observation.as_ref().ok_or_else(|| {
                    input_err(format!(
                        "`{}` needs an `observation` ({})",
                        req.policy,
                        crate::study::ty_name(p, &sm.obs)
                    ))
                })?;
                let obs = json_value(p, obs_json, &sm.obs, "the observation").map_err(input_err)?;
                let mut past = Vec::new();
                for h in &req.history {
                    let ht = step_ok(h.step)?;
                    let what = format!("the observation at step {ht}");
                    past.push((
                        json_value(p, &h.observation, &sm.obs, &what).map_err(input_err)?,
                        ht,
                    ));
                }
                Some((sm, obs, t, past))
            }
        };

        // Bound the decision before running it.
        let mut an = Analyzer::new(p, &globals, horizons.clone());
        let cost = an.decision(
            pol,
            param.as_ref(),
            seen.as_ref().map(|(_, o, t, _)| (o, *t)),
        )?;
        let budget = |msg: String| Error::new(ErrorKind::Budget, msg);
        if an.max_forecast_worlds > self.limits.max_forecast_worlds {
            return Err(budget(format!(
                "a forecast asks for {} worlds; the limit is {}",
                an.max_forecast_worlds, self.limits.max_forecast_worlds
            )));
        }
        if !cost.ops.is_finite() || cost.ops > self.limits.max_operations {
            return Err(budget(format!(
                "deciding with `{}` could take up to {:.3e} operations; the limit is {:.3e}",
                req.policy, cost.ops, self.limits.max_operations
            )));
        }

        let seed = req.seed.unwrap_or(0);
        let mut m = Machine::new(p, globals, seed as u64);
        m.horizons = horizons;
        let mut unpinned = Vec::new();
        if let Some((sm, obs, t, past)) = &seen {
            for (o, ht) in past.iter().chain(std::iter::once(&(obs.clone(), *t))) {
                pin(&mut m, sm, o, *ht, &mut unpinned)?;
            }
        }
        unpinned.sort();
        unpinned.dedup();
        let ops_before = m.ops;
        m.notes = Some(Vec::new());
        let action = m
            .serve(pol, param.as_ref(), seen.map(|(_, o, t, _)| (o, t)))
            .map_err(|f| {
                fault_err(
                    ErrorKind::Model,
                    f,
                    &format!("deciding with `{}`", req.policy),
                )
            })?;
        let notes = m
            .notes
            .take()
            .unwrap_or_default()
            .into_iter()
            .map(|(n, ty, v)| (n.to_string(), typed_json(p, &v, &ty)))
            .collect();
        Ok(Decision {
            id: req.id.clone(),
            runtime: RUNTIME.into(),
            model_sha256: self.model_sha256.clone(),
            inputs_sha256: sha256(serde_json::to_string(&shown).unwrap_or_default().as_bytes()),
            policy: req.policy.clone(),
            model: model.name().to_string(),
            step: req.step,
            seed,
            action: typed_json(p, &action, model.action()),
            notes,
            unpinned,
            work: DecisionWork {
                estimated_operations: cost.ops.ceil() as u64,
                operations: m.ops - ops_before,
                max_forecast_worlds: an.max_forecast_worlds,
            },
        })
    }

    /// The policy a study would call `name` or `name[param]`.
    fn policy(&self, m: &mut Machine, name: &str) -> Result<(u32, Option<Value>)> {
        let p = &self.program;
        let (base, label) = match name.split_once('[') {
            Some((b, rest)) => match rest.strip_suffix(']') {
                Some(l) => (b.trim(), Some(l.trim())),
                None => {
                    return Err(Error::new(
                        ErrorKind::Input,
                        format!("can't read the policy name `{name}`"),
                    ));
                }
            },
            None => (name.trim(), None),
        };
        let Some(pid) = p.policies.iter().position(|q| q.name == base) else {
            let names: Vec<&str> = p.policies.iter().map(|q| q.name.as_str()).collect();
            return Err(Error::new(
                ErrorKind::Input,
                format!(
                    "no policy named `{base}` (this program's policies: {})",
                    names.join(", ")
                ),
            ));
        };
        let pol = &p.policies[pid];
        if pol.oracle {
            return Err(Error::new(
                ErrorKind::Input,
                format!(
                    "`{base}` is an oracle: it sees the real future, which a served decision \
                     doesn't have"
                ),
            ));
        }
        match (&pol.family, label) {
            (None, None) => Ok((pid as u32, None)),
            (None, Some(_)) => Err(Error::new(
                ErrorKind::Input,
                format!("`{base}` is a single policy, not a family"),
            )),
            (Some((code, ty)), label) => {
                let vals = m
                    .code(code)
                    .map_err(|f| fault_err(ErrorKind::Runtime, f, &format!("policy `{base}`")))?;
                let labels: Vec<String> = vals.items().iter().map(|v| short(p, v, ty)).collect();
                match label.and_then(|l| labels.iter().position(|x| x == l)) {
                    Some(i) => Ok((pid as u32, Some(vals.items()[i].clone()))),
                    None => Err(Error::new(
                        ErrorKind::Input,
                        format!(
                            "`{base}` is a family: name one of {}",
                            labels
                                .iter()
                                .map(|l| format!("{base}[{l}]"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )),
                }
            }
        }
    }
}

/// Pin the facts observation `o` at step `t` revealed. `observe` may read a
/// fact only as an observation field's whole value, so each such field is
/// that fact's value; its key comes from the field's arguments, which must
/// not depend on the state the host doesn't send.
fn pin(
    m: &mut Machine,
    sm: &SeqModel,
    o: &Value,
    t: i64,
    unpinned: &mut Vec<String>,
) -> Result<()> {
    let p = m.p;
    let def = &p.fns[sm.observe as usize];
    let mut found = Vec::new();
    reveals(&def.body, o, &mut found, unpinned, p);
    for (fact, args, v) in found {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            let v = m
                .eval_in(a, vec![Value::Unit, Value::Int(t)], def.nslots)
                .map_err(|f| fault_err(ErrorKind::Model, f, "pinning an observed fact"))?;
            vals.push(v);
        }
        let key = m.fact_key(fact, &vals);
        m.pinned.insert(key, v);
    }
    Ok(())
}

fn fact_name(p: &Program, f: u32) -> String {
    let d = &p.facts[f as usize];
    format!("{}.{}", d.world, d.name)
}

/// The facts `e` (an observe body, evaluating to `o`) reveals whole, with
/// the arguments that key them.
fn reveals<'e>(
    e: &'e Ex,
    o: &Value,
    found: &mut Vec<(u32, &'e [Ex], Value)>,
    unpinned: &mut Vec<String>,
    p: &Program,
) {
    match e {
        Ex::Fact { fact, args, .. } => {
            if args.iter().all(step_only) {
                found.push((*fact, args, o.clone()));
            } else {
                unpinned.push(fact_name(p, *fact));
            }
        }
        Ex::Block(_, tail) => reveals(tail, o, found, unpinned, p),
        Ex::Record(fields) => {
            for (f, v) in fields.iter().zip(o.items()) {
                reveals(f, v, found, unpinned, p);
            }
        }
        _ => facts_in(e, unpinned, p),
    }
}

/// Facts read where we can't tell which value the observation holds.
fn facts_in(e: &Ex, out: &mut Vec<String>, p: &Program) {
    let mut kids: Vec<&Ex> = Vec::new();
    match e {
        Ex::Fact { fact, args, .. } => {
            out.push(fact_name(p, *fact));
            kids.extend(args);
        }
        Ex::Neg(x, _) | Ex::Not(x) | Ex::ToNum(x) | Ex::Field(x, _) => kids.push(x),
        Ex::Arith(_, a, b, _)
        | Ex::Cmp(_, a, b)
        | Ex::And(a, b)
        | Ex::Or(a, b)
        | Ex::Index(a, b, _) => kids.extend([&**a, &**b]),
        Ex::Call(_, xs)
        | Ex::Builtin(_, xs, _)
        | Ex::Record(xs)
        | Ex::Array(xs)
        | Ex::Variant(_, xs) => kids.extend(xs),
        Ex::If(c, t, f) => kids.extend([&**c, &**t, &**f]),
        Ex::Match(s, arms, _) => {
            kids.push(s);
            kids.extend(arms.iter().map(|a| &a.body));
        }
        Ex::Comp { body, filter, .. } => {
            kids.push(body);
            kids.extend(filter.as_deref());
        }
        Ex::Block(stmts, tail) => {
            for s in stmts {
                if let St::Let(_, x) | St::Set(_, _, x) | St::Expr(x) | St::Assert(x, _, _) = s {
                    kids.push(x);
                }
            }
            kids.push(tail);
        }
        _ => {}
    }
    for k in kids {
        facts_in(k, out, p);
    }
}

/// Whether `e` depends only on `observe`'s step parameter (slot 1),
/// constants and inputs.
fn step_only(e: &Ex) -> bool {
    match e {
        Ex::Const(_) | Ex::Global(_) => true,
        Ex::Local(i) => *i == 1,
        Ex::Neg(x, _) | Ex::Not(x) | Ex::ToNum(x) => step_only(x),
        Ex::Arith(_, a, b, _) | Ex::Cmp(_, a, b) | Ex::And(a, b) | Ex::Or(a, b) => {
            step_only(a) && step_only(b)
        }
        Ex::If(c, t, f) => step_only(c) && step_only(t) && step_only(f),
        Ex::Builtin(_, xs, _) | Ex::Call(_, xs) | Ex::Variant(_, xs) => xs.iter().all(step_only),
        _ => false,
    }
}
