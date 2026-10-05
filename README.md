# parallax

A small language for comparing interventions across the same uncertain worlds.

The underlying world is held fixed while you look at it through different
decisions. A parallax program says what is known, what isn't, what the world
does in response to an action, which actions we might take, and how to choose
between them. The runtime evaluates every candidate in the same possible
worlds and returns a typed result tree.

> Inputs describe what is known. Uncertainty describes what isn't. Models
> describe what the world does. Policies describe what we might do. Studies
> compare those policies across the same possible worlds. Objectives decide
> which trade-off we prefer.

It has two modes. Offline, a **study** asks which policy to use, across
thousands of common worlds. Online, **decide** asks what the chosen policy
does now, given what is known: one call of the policy and its forecasts,
typically well under a millisecond (see [Serving decisions](#serving-decisions)).

It is meant as a safe executable substrate for agents. A program can't do
I/O, read the clock or the environment, or call anything outside itself, so
untrusted model-generated code can use CPU but can't cause side effects.
Every program terminates, and its work is bounded before it runs.

```parallax
input cell_temp: °C between -30 °C and 40 °C = 1 °C

type Outcome = { cost: p, short: kWh }
action Lead = min

world Pack {
    uncertain heater ~ uniform(0.18 °C/min, 0.30 °C/min)
    uncertain sensor_error ~ uniform(-1 °C, 1 °C)
}

model overnight(lead: Lead) -> Outcome = { ... }

policy lead[m in 0 min ..= 120 min step 5 min] = m

study choose_prewarm {
    worlds 10_000
    seed 42
    require probability(short > 0.1 kWh) <= 1%
    minimize mean(cost)
    report p95(cost), cvar95(cost)
}
```

See [`examples.md`](examples.md) for complete, tested programs, and
[`simulations/`](simulations) for larger studies.

## Concepts

### Uncertainty is keyed

Uncertainty enters only in a `world`, as named facts:

```parallax
world Weather {
    latent climate ~ normal(0.0, 1.0)                    # never observable
    uncertain temp(day: Int) ~ normal(12.0 + climate, 3.0)
    derived frost(day: Int) = temp(day) < 0.0
}
```

A fact's value in world `i` is a pure function of (study seed, `i`, world
name, fact name, arguments). It is not "the next random number", so it
doesn't matter when, how often or in what order a fact is read.
`Weather.temp(3)` is the same temperature for every policy in world `i`, even
if one policy does 200 extra calculations before asking. Every candidate
therefore experiences worlds `0..n` with identical exogenous facts. This is
the language's foundational guarantee: common worlds, not merely common
random-number streams.

- The same keys and seed give the same facts in any study of any program.
  For a different scenario, use another world name.
- Dependent uncertainty comes from shared latent facts, as with `climate`
  above.
- Fact parameters are keys: `Int`, `Bool`, `String` or a plain enum.
- Distributions: `uniform(lo, hi)` (integer bounds draw an integer,
  inclusive; otherwise a float in `lo..hi`), `bernoulli(p)`,
  `categorical(values, weights)`, `normal(mean, sd)`,
  `lognormal(median, sigma)`, `triangular(lo, mode, hi)` and
  `empirical(samples)`. Arguments may use other facts, constants and inputs,
  with units.

Nothing changes because code reads a value: values are just values.

### Decisions are actions

What the policy decides is an `action` type, and a model's action parameter
must have one, so a decision is never mistaken for noise:

```parallax
action Mode = full | throttled          # variants
action Lead = min                       # or any type: here a duration
```

### Models say what the world does

A **decision model** takes one action and returns an outcome:

```parallax
model play(bet: Bet) -> Int = ...
```

A **sequential model** runs for a fixed horizon of steps:

```parallax
model shift {
    horizon 24
    init = Plant { ... }                         # may read the world
    step(s: Plant, a: Mode, t: Int) -> Plant = ...
    stop(s: Plant) -> Bool = s.melted            # optional: end early
    observe(s: Plant, t: Int) -> Gauge = ...     # all a policy sees
    belief(o: Gauge, t: Int) -> Plant = ...      # where forecasts start
    outcome(s: Plant) -> Shift = ...             # optional: default the state
    invariant(s: Plant) -> Bool = s.heat >= 0    # optional, repeatable
}
```

Model clauses may read world facts. Ordinary `fn`s are pure functions of
their arguments: pass facts in from the model.

### Policies see only what they may know

A policy chooses actions. For a decision model it takes no parameters and
decides once, from inputs and constants. For a sequential model it is
`(o: Observation, t: Int) -> Action`.

- A policy can't read world facts or call models. It sees its observation,
  inputs and constants. This is checked statically, so a policy can't cheat
  by inspecting the future.
- `observe` may only reveal a fact whole, as an observation field's value
  (`drought: Nature.drought(t)`), and never a `latent` fact.
- `belief(o, t)` reconstructs a state from an observation. It is required
  unless the observation is the state.
- Families generate candidates: `policy gauge[limit in [40, 50, 60]](o: Plant, t: Int) -> Mode = ...`
  is three policies, `gauge[40]`, `gauge[50]` and `gauge[60]`.
- If several models fit a policy, write `policy name for model ...`.
- `note name = expr` in a policy's body records a reason (a predicted
  error, the worlds it simulated) reported with a served decision. Studies
  ignore notes.

### Forecasts and oracles

`forecast(action, horizon: h, worlds: k, then: action2)` lets a policy ask
"given what I know now, what happens if I choose this?" It starts from
`belief(observation)` and runs `k` imagined worlds of its own. Facts that
`observe` has revealed keep their real values there; everything else,
including the future, is redrawn. It returns an array of `k` outcomes, and
every action and policy at the same step imagines the same `k` worlds. A
decision model's policy uses `forecast(action, worlds: k)`.

`skip: n` imagines worlds `n..n + k` instead, continuing an earlier
forecast. So a policy can spend simulation in proportion to ambiguity:
forecast 16 worlds, and only if the actions are close, 48 more, and so on,
in a loop with a fixed maximum (`for r in 0..4 while not clear { ... }`).

An `oracle policy` is clairvoyant, and says so. It receives the true state,
may read the world, and uses `rollout(action, horizon: h, then: a2)` to live
the real future. Oracles are reported but never recommended; each policy's
`of_oracle` gives its primary objective as a fraction of the best feasible
oracle's ("forecast achieves 82% of oracle").

### Studies are the top level

A program culminates in one or more studies. There is no `main` and no
top-level effect.

```parallax
study night_shift {
    model shift                     # needed if there are several models
    worlds 400                      # default 1000
    seed 7                          # default 0
    with soc = 30%, cell_temp = -5 °C   # override inputs for this study
    compare gauge, cautious         # default: every policy for the model
    require probability(melted) <= 20%
    maximize mean(output)           # several objectives: lexicographic
    report median(output), probability(melted where pump_failed)
}
```

Metrics are statistics over the worlds' outcomes. Inside a statistic, the
outcome's fields are in scope (or the whole outcome as `outcome`):

| Statistic | Meaning |
| :--- | :--- |
| `mean(x)`, `median(x)`, `stddev(x)`, `variance(x)` | over worlds |
| `quantile(x, q)`, `p5(x)` ... `p99(x)` | linear interpolation |
| `probability(cond)`, `count(cond)` | fraction / number of worlds |
| `cvar(x, q)`, `cvar95(x)` | mean of the highest `1 - q` of values (losses) |
| `min(x)`, `max(x)` | over worlds |

`stat(x where cond)` summarises only the worlds where `cond` holds. This is
a subgroup of the results, not conditioning in the model. Statistics of
arrays are taken element by element, so `median(track)` is an array.
Statistics combine arithmetically: `mean(a) / mean(b)`, `p95(t) in min`.

A policy is **feasible** if it meets every `require` and had no model
errors. Feasible non-oracle policies are ranked by the objectives in order;
the first is `recommended`.

## Types and units

`Bool`, `Int` (64-bit, overflow is an error), `Float` (always finite: overflow
and division by zero are errors), `String`, records, enums with optional
fields, arrays and quantities.

```parallax
type Leg = { distance: km, speed: km/h }
enum Fate = safe | ejected | crashed(star: Int)
```

A quantity is a number with a dimension: `120 W`, `10 kWh`, `29 p/kWh`,
`75 min`, `30%`. Adding minutes to kilowatt-hours, or passing `W` where `kWh`
is expected, is a type error. Multiplying and dividing combine dimensions:
`29 p/kWh * 3 kWh` is in pence. `x in unit` gives a plain number:
`(90 min + 1 h) in min` is `150.0`.

- Built in: `s ms min h day week`, `m km cm mm`, `kg g`, `°C` (or `degC`),
  `A Ah V`, `J kJ MJ Wh kWh MWh`, `W kW MW`, `N`, `Hz`, `£ p`, `$ ¢`, `€`,
  and `%` (0.01). °C is linear; there is no kelvin, so no affine
  conversions.
- `unit pallet` declares a base unit; `unit truckload = 24 pallet` a derived
  one.
- A unit follows a number literal, and in a type stands for its dimension
  (`x: kWh`). Results are shown in the unit the program wrote.
- `/` always gives a float; `//` floors integers; `mod(a, b)` is the
  non-negative remainder. Integers promote to floats where needed, and a
  literal `0` takes any unit.
- Floating-point results are identical on every platform: `sqrt`, `exp`,
  `ln`, `sin`, `cos` and `pow` are implemented portably.

## Functions and termination

Functions are pure: no ambient mutable state, no closures, no recursion
(checked). `let` binds; `var` declares a local that can be reassigned,
including in place (`grid[y][x] = v`, `s.heat = h`). Arrays and records are
values, copied on write.

Loops are always bounded: `for i in 0..n { }`, `for x in xs { }`,
`iterate n { }`, comprehensions `[f(x) for x in xs if cond]`, and an early
stop with `for i in 0..300 while cond { }`. There is no `while` loop.

Before running anything, `parallax check` bounds each study's work. It
tracks integers as intervals and arrays by length, through every function,
model step, forecast and rollout, and a loop whose bound depends on
something unknown before the run is rejected. A run counts the operations it
actually performs, and the test suite checks the count never exceeds the
bound.

## Errors in worlds

An `assert cond, "message"`, a failed `invariant`, an integer overflow or an
index out of range in one world is a model error, not a crash. It is reported
with its policy, world, step and line, and a policy with model errors is
never recommended.

## Running

Each version is released with prebuilt binaries for Linux (x86_64), macOS
(Apple silicon and Intel) and Windows (x86_64). The repo is private, so
download them with an authenticated [`gh`](https://cli.github.com):

```sh
scripts/install.sh                # latest release into ~/.local/bin (or $PARALLAX_INSTALL_DIR)
scripts/install.sh v0.1.0         # a specific version
gh release download --repo pixelbadger/parallax --pattern '*windows*'   # Windows: unzip parallax.exe
```

Then:

```sh
parallax run simulations/reactor.px
parallax check simulations/reactor.px
parallax run model.px --inputs inputs.json --input soc=30% --seed 3
```

From source, use `cargo run --release -- <args>` in place of `parallax`, or
`cargo install --path .` to install it.

Options: `--study NAME`, `--seed N`, `--worlds N`, `--max-operations X`,
`--max-worlds N` and `--compact`. Errors are printed as JSON too
(`{"error": {"kind", "message", "line", "col"}}`), with exit status 1.

Inputs are given as JSON. A quantity is either a number in the declared unit
or a string with a unit (`"5 min"`); enums are strings, records are objects,
and arrays are arrays.

### Results

`run` returns, for each study:

- `provenance`: runtime version, model and input hashes, seed, world count
  and world ids.
- The resolved `inputs`, with units.
- Per policy: `status` (`ok`, `infeasible` or `model_errors`), `rank`, any
  family `parameter`, a decision model's chosen `action`, `objectives`,
  `constraints` and `metrics`. Each metric has its value and unit, and a 95%
  interval for a plain `mean` or `probability`.
- Per policy, also `vs_recommended` (the paired, world-by-world difference
  with its interval), `of_oracle`, and estimated and actual operations.
- `ranking`, `recommended`, `best_oracle`, sample `errors`, and the
  study's `work`.

`check` returns the input schema (type, unit, bounds, default, required),
the worlds' facts, models, policies, and each study's estimated operations,
transitions and forecast worlds.

As a library: `parallax::run(src, &Options)` and `parallax::check(...)`
return the same typed `Report` and `CheckReport`; `Options::limits` sets
the host's budget.

### Serving decisions

Once a study has chosen a policy, `decide` follows it for one decision,
without the study's evaluation worlds:

```sh
parallax decide simulations/tool_choice.px --policy adaptive --input confidence=60%
```

```json
{
  "policy": "adaptive",
  "model": "ask",
  "seed": 0,
  "action": "search",
  "notes": {
    "worlds": 64,
    "predicted_error_direct": { "value": 48.4375, "unit": "%" },
    "predicted_error_search": { "value": 10.9375, "unit": "%" },
    "search_saves": { "value": 37.17, "unit": "s" }
  },
  "work": { "estimated_operations": 427295, "operations": 6772, "max_forecast_worlds": 768 }
}
```

(Abridged: the answer also carries `runtime`, `model_sha256` and
`inputs_sha256`.)

- A **decision model's policy** decides from inputs alone. Its forecasts
  imagine the same worlds as in a study, so with the study's `--seed` it
  chooses exactly the `action` the study reported.
- A **sequential policy** decides one step: give `--step t` and
  `--observation` (JSON, or a file holding it). Forecasts start from
  `belief(observation, t)`. A fact that `observe` reveals is held at the
  observed field's value, as a study holds it at its real value. Facts
  revealed earlier can be given as `--history '[{"step": 3, "observation":
  {...}}]'`. A revealed fact whose key depends on the state can't be keyed
  without the state, so it is listed in `unpinned` and forecasts redraw it.
- Oracles can't be served: there is no real future to look at.
- The decision's work is bounded before it runs (`--max-operations`), and
  `work` reports the bound and the actual count.
- Errors are JSON, as for `run`. A failure in the policy or its forecasts
  is `{"kind": "model"}`.

For an agent that decides often, `serve` checks the program once, then
answers one JSON request per line on stdin with one JSON line on stdout:

```sh
parallax serve simulations/tool_choice.px --input stakes="2 min"
{"id": 1, "policy": "adaptive", "inputs": {"confidence": "60%"}}
{"id": 2, "policy": "adaptive", "inputs": {"fresh": true}}
```

A request has `policy` and optionally `id` (echoed back), `inputs` (laid
over those on the command line), `observation`, `step`, `history` and
`seed`. Answers come in order; a bad request gets `{"id", "error"}` and
the server carries on. Recalibrating a domain from telemetry only changes
its inputs: no restart and no new program.

As a library, `parallax::Engine::new(src)` holds the checked program, and
`engine.decide(&Request)` returns the typed `Decision`.

### Testing

```sh
cargo test                                    # unit, semantics, golden programs, examples
UPDATE_EXPECT=1 cargo test --test programs    # regenerate tests/*.out and simulations/*.out
```

## Grammar

Newlines end declarations and statements wherever an expression could end;
inside `(` and `[` they are ignored. `;` also separates. Comments start
with `#`.

```ebnf
program     = { decl } ;
decl        = "input" IDENT ":" type [ "between" expr "and" expr ] [ "=" expr ]
            | ( "const" | "derived" ) IDENT [ ":" type ] "=" expr
            | "unit" IDENT [ "=" NUMBER unit ]
            | "type" IDENT "=" ( record_type | type )
            | "enum" IDENT "=" variants
            | "action" IDENT "=" ( variants | type | record_type )
            | "world" IDENT "{" { fact } "}"
            | "fn" IDENT params [ "->" type ] body
            | "model" IDENT "(" param ")" [ "->" type ] body
            | "model" IDENT "{" { clause } "}"
            | [ "oracle" ] "policy" IDENT [ "[" IDENT "in" iter "]" ]
                  [ "for" IDENT ] [ params ] [ "->" type ] body
            | "study" IDENT "{" { study_clause } "}" ;
variants    = variant { "|" variant } ;
variant     = IDENT [ "(" IDENT ":" type { "," IDENT ":" type } ")" ] ;
record_type = "{" IDENT ":" type { "," IDENT ":" type } "}" ;
type        = "Int" | "Float" | "Bool" | "String" | IDENT | unit
            | "[" type [ ";" expr ] "]" ;
unit        = atom { ( "*" | "/" ) atom } ;
atom        = ( IDENT | "%" ) [ "^" [ "-" ] INT ] ;
fact        = ( "latent" | "uncertain" ) IDENT [ params ] [ ":" type ] "~" call
            | "derived" IDENT [ params ] [ ":" type ] "=" expr ;
clause      = "horizon" expr
            | "init" [ "->" type ] body
            | ( "step" | "stop" | "observe" | "belief" | "outcome" | "invariant" )
                  params [ "->" type ] body ;
study_clause = "model" IDENT | "worlds" expr | "seed" expr
            | "with" IDENT "=" expr { "," IDENT "=" expr }
            | "compare" ( "all" [ "policies" ] | IDENT { "," IDENT } )
            | "require" expr | ( "minimize" | "maximize" ) expr
            | "report" expr { "," expr } ;
params      = "(" [ param { "," param } ] ")" ;
param       = IDENT ":" type ;
body        = "=" expr | block ;

block       = "{" { stmt } [ expr ] "}" ;
stmt        = ( "let" | "var" ) IDENT [ ":" type ] "=" expr
            | place "=" expr
            | "for" IDENT "in" iter [ "while" expr ] block
            | "iterate" expr block
            | "assert" expr [ "," STRING ]
            | "note" IDENT "=" expr                  (* in a policy *)
            | expr ;
place       = IDENT { "." IDENT | "[" expr "]" } ;
iter        = expr ( ".." | "..=" ) expr [ "step" expr ] | expr ;

expr        = and { "or" and } ;
and         = not { "and" not } ;
not         = "not" not | cmp ;
cmp         = conv [ ( "==" | "!=" | "<" | ">" | "<=" | ">=" ) conv ] ;
conv        = add [ "in" unit ] ;
add         = mul { ( "+" | "-" ) mul } ;
mul         = unary { ( "*" | "/" | "//" ) unary } ;
unary       = "-" unary | postfix [ "^" unary ] ;
postfix     = primary { "." IDENT | "[" expr "]" | "(" [ arg { "," arg } ] ")" } ;
arg         = [ IDENT ":" ] expr [ "where" expr ] ;
primary     = NUMBER [ unit ] | STRING | "true" | "false"
            | IDENT [ "{" [ IDENT [ ":" expr ] { "," IDENT [ ":" expr ] } ] "}" ]
            | "[" [ expr { "," expr } ] "]"
            | "[" expr "for" IDENT "in" iter [ "if" expr ] "]"
            | "(" expr ")" | block
            | "if" expr block [ "else" ( "if" ... | block ) ]
            | "match" expr "{" pattern "=>" expr { "," pattern "=>" expr } "}" ;
pattern     = "_" | [ IDENT "." ] IDENT [ "(" IDENT { "," IDENT } ")" ] ;
```

In the head of an `if`, `match` or `for`, `Name {` starts the block rather
than a record: parenthesise a record there.

Built-in functions: `min`, `max` (two values, or one array), `abs`, `sqrt`,
`exp`, `ln`, `sin`, `cos`, `pow`, `floor`, `ceil`, `round`, `float`, `clamp`,
`mod`, `len`, `fill(n, v)`, `sum`, `any`, `all`, `argmin`, `argmax`, the
statistics above over an array, `forecast(action, horizon:, worlds:, skip:,
then:)` and `rollout(action, horizon:, then:)`; `pi` is a constant.
