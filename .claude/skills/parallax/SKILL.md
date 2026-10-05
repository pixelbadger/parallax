---
name: parallax
description: Write, check, run and interpret parallax programs (.px), the decision language in this repo for comparing interventions (policies) across the same uncertain worlds. Use when asked to model a decision under uncertainty, compare options or policies, estimate risk, build a simulation study, write or fix a .px file, or explain a parallax report.
---

# Programming in parallax

parallax turns a fuzzy "what if we did X instead of Y?" into a reproducible
study. Every candidate policy is run in the same possible worlds, and the
runtime returns typed JSON. Full reference: `README.md`. Tested examples:
`examples.md`. Larger programs: `simulations/*.px`.

## Workflow

1. **Frame the decision.** Before writing code, write down:
   - **inputs**: what the host knows now, with units and bounds
   - **uncertainty**: what the world decides, that nobody controls
   - **action**: what we decide
   - **outcome**: what we measure
   - **criteria**: what we require, and what we optimise
   - Is it one decision (a decision model) or a decision every step (a
     sequential model)?
2. **Write the program** in this order: inputs → constants → types → action
   → world → helper `fn`s → model → policies → study. Start from a template
   below.
3. **Check it:** `cargo run --release -- check model.px`. Fix errors (the
   messages say what to do), read the input schema, and look at
   `estimated_operations`: aim for under ~1e9 while iterating.
4. **Run it:** `cargo run --release -- run model.px [--inputs in.json]
   [--input name=value] [--worlds N]`. Use a few hundred worlds while
   iterating, and more for the final answer.
5. **Interpret it** (see below). Report the recommendation with its
   uncertainty and provenance; don't claim more precision than the
   intervals give.

## Rules the checker enforces

Design with these from the start:

- **Uncertainty only in a `world`**, as named facts:
  `uncertain demand(day: Int) ~ uniform(2, 8)`. There is no `random()`.
  Give every stochastic event its own key, parameterised by what makes it
  distinct (`failure(server, hour)`, `roll(year, month, y, x, k)`).
- **Model code reads facts** as `World.fact(args)`. Helper `fn`s are pure:
  read facts in the model, then pass the values (or arrays of them) to
  helpers.
- **Policies never read the world.** A policy sees its observation, `t`,
  inputs and constants. To look ahead, it uses
  `forecast(action, horizon: h, worlds: k)`, which returns an array of
  outcomes from imagined worlds.
- **`oracle policy`** sees the true state and may use
  `rollout(action, horizon: h)` in the real world. Use one as an upper bound,
  never as a recommendation.
- **`observe` reveals facts whole**: a fact may appear only as an
  observation record's field value (`Seen { price: Market.price(t) }`), and
  never a `latent` fact. If the observation type isn't the state type, write
  `belief(o: Obs, t: Int) -> State`, the state a forecast starts from.
- **A model's action parameter needs an `action` type**:
  `action Mode = full | throttled` or `action Lead = min`.
- **No recursion and no `while`.** Loop over ranges or arrays:
  `for i in 0..N { }`. For "until done", loop to a fixed maximum and stop
  early: `for minute in 0..300 while total < need { }`. Loop bounds must
  come from constants, inputs, a policy's family parameter, array lengths,
  or integer `uniform` facts, not from floats or unbounded values.
- **Units must agree.** `5 min + 3 kWh` is an error. Multiply and divide
  freely (`power * 1 min` is energy), and use `x in unit` for a plain
  number. `30%` is 0.3.
- **`/` is always a float**; `//` floors integers; `mod(a, b)` for the
  remainder (`%` is a unit, not modulo).
- `let` is immutable; `var` locals can be reassigned, including in place
  (`s.vx[2] = ...`). Parameters are immutable: write `var s = s`.

## Templates

### One decision

```parallax
input budget: £ between 0 £ and 10000 £ = 2000 £

type Outcome = { cost: £, late: Bool }
action Choice = cheap | fast | hybrid(share: Float)

world Ops {
    latent difficulty ~ lognormal(1.0, 0.3)           # shared driver: correlates the rest
    uncertain duration(task: Int) ~ triangular(2.0, 3.0, 8.0)
}

fn total(xs: [Float]) -> Float = sum(xs)

model project(c: Choice) -> Outcome = {
    let days = total([Ops.duration(i) * Ops.difficulty for i in 0..5])
    ...
    Outcome { cost: ..., late: days > 20.0 }
}

policy cheap_one = cheap
policy fast_one = fast
policy mix[s in [0.25, 0.5, 0.75]] = hybrid(s)

study choose {
    worlds 2000
    seed 1
    require probability(late) <= 10%
    minimize mean(cost)
    report p95(cost), cvar95(cost), probability(late)
}
```

A decision model's policy decides once, from inputs only. It may still plan
under uncertainty:
`argmin([mean([o.cost for o in forecast(a, worlds: 50)]) for a in OPTIONS])`.

### Decisions over time

```parallax
type State = { level: Int, failed: Int }
type Seen = { level: Int, alarm: Bool }
action Act = run | rest

world Plant {
    uncertain load(t: Int) ~ uniform(2, 8)
    uncertain alarm(t: Int) ~ bernoulli(0.05)
}

model plant {
    horizon 48
    init = State { level: 20, failed: 0 }
    step(s: State, a: Act, t: Int) -> State = { ... Plant.load(t) ... }
    stop(s: State) -> Bool = s.failed > 3                 # optional
    observe(s: State, t: Int) -> Seen = Seen { level: s.level, alarm: Plant.alarm(t) }
    belief(o: Seen, t: Int) -> State = State { level: o.level, failed: 0 }
    outcome(s: State) -> Result = ...                     # optional
    invariant(s: State) -> Bool = s.level >= 0            # optional
}

policy rule[x in [10, 20]](o: Seen, t: Int) -> Act = if o.level < x { run } else { rest }

policy lookahead(o: Seen, t: Int) -> Act = {
    let bad = mean([float(r.failed) for r in forecast(rest, horizon: 4, worlds: 20, then: run)])
    if bad > 0.0 { run } else { rest }
}

oracle policy perfect(s: State, t: Int) -> Act =
    if rollout(rest, horizon: 4, then: run).failed > s.failed { run } else { rest }

study ops {
    worlds 500
    maximize probability(failed == 0)
    report mean(level), probability(failed > 0 where level < 5)
}
```

## Choosing the study

- **Objectives** are lexicographic: list the most important first.
  Statistics: `mean`, `median`, `quantile(x, q)`, `p5`..`p99`, `stddev`,
  `variance`, `probability(cond)`, `count(cond)`, `cvar95(x)` (mean of the
  worst 5%, high values = bad), `min` and `max`.
- **Constraints** (`require metric op bound`) mark policies infeasible; put
  safety here rather than folding it into a score.
- `where` gives a subgroup: `mean(output where pump_failed)`. This is not
  conditioning; it summarises the worlds where it holds.
- Statistics of array outcomes are element-wise (`median(track)`).
- Use `with input = value` to run the same model under several scenarios,
  one study each.
- Use the same `seed` everywhere you compare, and a different seed to check
  that a conclusion is stable.

## Interpreting the report

- `recommended` is the best feasible, error-free, non-oracle policy.
  `ranking` lists the rest in order.
- `objectives[].ci95` is a 95% interval for a plain `mean` or `probability`.
- `vs_recommended.difference` and its `ci95` are paired, world-by-world
  differences. Because all policies share worlds, these are much tighter
  than the separate intervals: they are what tells you whether the winner
  really wins. If the paired interval includes 0, say the policies are
  indistinguishable at this world count, and add worlds.
- `of_oracle` (e.g. 0.82) is how much of the clairvoyant's performance a
  policy achieves; the gap is the value of better information.
- `status: "model_errors"` means an assertion, invariant, overflow or index
  error happened; see `errors[]` (policy, world, step, line). Fix the model,
  or encode the failure as an outcome.
- Quote `provenance` (model hash, input hash, seed, worlds) with any
  recommendation.

## Common fixes

| Message | Fix |
| :--- | :--- |
| `a policy can't read the world` | Add what it legitimately knows to `observe`, or use `forecast`. |
| `functions are pure` | Read the fact in the model and pass the value in. |
| `must reveal a world fact whole` | Put the fact itself in an observation field, and compute with it in the policy. |
| `can't bound the work` | Loop to a constant maximum with `while`, or derive the bound from an input or constant. |
| `units don't match` | Convert (`x in min`), or fix the formula's dimensions. |
| `fits more than one model` | `policy name for model_name ...`, or annotate `-> Action`. |
| `several models` | Add `model name` to the study. |
| `already declared` | Policies, functions, values and worlds share one namespace; rename one. |
| A budget error on run | Fewer worlds, fewer forecast worlds or a shorter horizon, or raise `--max-operations` deliberately. |
