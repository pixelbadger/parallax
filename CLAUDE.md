# parallax: notes for agents

parallax is a small decision language: it compares policies across the same
uncertain worlds (offline, `run`), and serves a chosen policy's decision
now (online, `decide` and `serve`). The language is documented in README.md (concepts, units,
grammar) and examples.md (tested examples). This file covers the
implementation and the decisions behind it.

To *write* parallax programs, use the skill in `.claude/skills/parallax/`.

## Commands

```sh
cargo test                                   # unit + semantics + golden programs + examples (~10s)
UPDATE_EXPECT=1 cargo test --test programs   # regenerate tests/*.out and simulations/*.out
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # CI runs both
cargo run --release -- run simulations/reactor.px
cargo run --release -- check simulations/reactor.px
cargo run --release -- decide simulations/tool_choice.px --policy adaptive --input confidence=60%
```

Releases: when CI passes on main, `.github/workflows/release.yml` publishes
binaries for Cargo.toml's `version` as GitHub release `v<version>`, unless
it already exists. To release, bump the version (and refresh `Cargo.lock`,
since CI builds `--locked`). The runtime version appears in every report's
provenance, so regenerate the goldens with `UPDATE_EXPECT=1`. `scripts/install.sh`
fetches a release.

`[profile.dev] opt-level = 1` is deliberate: the golden tests run whole
studies. Each golden program is its own test, so they run in parallel.

## Layout

The pipeline is parse → check → (per study) set up, bound, run; or, to
serve, parse → check once → (per request) inputs, bound, one policy call.

- `src/lexer.rs`: tokens, with Go-style newline terminators: a newline is a
  `;` when the line can end there and the next line can't continue it, and
  it is ignored inside `(` and `[`.
- `src/parser.rs`, `src/ast.rs`: recursive descent. Numbers followed by a
  unit name become quantities, so the parser pre-scans `unit` declarations.
  Many words (`step`, `horizon`, `worlds`, `report`...) are contextual,
  not keywords.
- `src/check.rs`: names, types, units and information boundaries, producing
  `src/ir.rs`. Functions, facts and globals are checked lazily on first use;
  a function still being checked when called again is recursion, which is
  how recursion is rejected. `Ctx` records what the code being checked may
  do (read facts, forecast, roll out). Globals get a dependency order
  (`global_order`).
- `src/units.rs`: dimensions as exponent vectors over 16 base units (8
  built in, 8 for `unit name`). A type `Ty::Num(Dim, Hint)` carries a
  display hint (the unit written), which equality ignores.
- `src/world.rs`: keyed draws (SplitMix64 hashing, `Stream`).
- `src/eval.rs`: `Machine`, the evaluator, and the runners for decision and
  sequential models, forecasts and rollouts.
- `src/cost.rs`: the static work bound, an abstract interpreter.
- `src/stats.rs`: statistics shared by code (over arrays) and studies (over
  worlds).
- `src/study.rs`: inputs, settings, policy instances, budgets, running, and
  the typed result structs (`Report`, `CheckReport`), serialised to JSON.
- `src/decide.rs`: serving. `Engine` holds the checked program; `decide`
  takes a `Request` (policy, inputs, observation, step, history, seed),
  bounds that one call (`Analyzer::decision`), pins revealed facts and
  runs `Machine::serve`. `main.rs` has the `decide` and `serve` commands.
- `tests/programs.rs`: golden files, plus the check that actual operations
  never exceed the bound, and the examples.md runner. `tests/semantics.rs`:
  properties that hold for every seed, and the rejections.
  `tests/decide.rs`: serving, including the `serve` binary.

## Semantics that are easy to break

- **Keys are the foundation.** A fact's value is
  `Stream::new(seed, world_id, key)`, with key =
  hash(world name, fact name, arguments). Variants hash by *name* (via
  `variant_keys`), so reordering declarations changes nothing, but renaming
  a world, fact or variant changes the draws. Changing `mix`, `combine`,
  `fnv`, `Stream` or any sampler changes every `.out` file.
- **Facts are recomputed, not memoised.** Being pure functions of their key,
  they need no cache, and there's nothing to invalidate between policies.
- **Forecast worlds.** `forecast_world(Some(eval), t, j)` for sequential
  models, so every action and policy at the same (world, step) imagines the
  same `k` worlds. A decision model's non-oracle policy uses
  `forecast_world(None, 0, j)`: it decides once for all worlds
  (`decide_once`), from inputs only.
- **Served decisions** have no evaluation world (`Decision.eval` is
  `None`), so forecasts use `forecast_world(None, t, j)`: for a decision
  model that is exactly a study's, so `decide` with the study's seed
  returns the study's action (tested). Revealed facts can't come from a
  real world either: `decide::pin` finds the facts `observe` reads as whole
  record fields (the `direct` rule makes the field's value the fact's
  value), evaluates their key arguments with only `t` known, and puts
  key → observed value in `Machine::pinned`, which `fact` returns while
  forecasting. Keys depending on the state go to `unpinned`.
- **`note`** (contextual, policies only) records into `Machine::notes`
  only when it is `Some` (serving); in studies it costs its expression,
  like `St::Expr`.
- **`skip`** offsets a forecast's world indices `j`; the bound is
  unchanged (it depends on `worlds` only).
- **Revealed facts.** While `observe` runs, `observing` records each fact
  key read into `revealed` (cleared per world). In a forecast, a revealed key
  is computed wholly in the evaluation world. The checker allows a fact read
  in `observe` only as a record field's whole value (`direct`), never a
  latent one, so revealing never leaks a fact a policy only partly saw.
- **Information boundary.** `Ctx::Policy` can't read facts or call models;
  `fn`s are pure (`Ctx::Fn` can't read facts). Only oracles may `rollout`,
  and they may not `forecast`.
- **Policies infer their model** from their parameter shape, then their
  return type, then by trying their body against each candidate.
- **Work accounting must mirror evaluation.** `eval.rs` adds 1 to `ops` per
  node, statement, loop iteration, model step and forecast sample, plus
  array work in builtins and statistics (`stats::cost`). `cost.rs` charges
  exactly the same, with branches taking the max and loops multiplied by
  their bound. If you add a node or change what evaluation counts, change
  both: the golden tests assert actual ≤ estimated for every policy. The
  fast paths in `eval` (`v.f`, `v[i]`, `v.f[i]` read in place) add the
  `ops` the nodes they skip would have.
- **Abstract domain.** Integers are intervals (`i64::MIN`/`MAX` for
  unbounded), arrays are length ranges with a joined element, records are
  field-wise, and everything else is `Top`. Loop-carried variables reach a
  fixpoint by joining, then widening after 3 rounds. A sequential model's
  state is a fixpoint of `step` under any action (`Analyzer::state`). Integer
  facts from `uniform(lo, hi)` carry their range, so they can bound loops.
- **Missing metric values.** A statistic over no worlds (`where` filtered
  everything) is `Value::Unit`, which propagates through arithmetic and
  builtins and serialises as `null`.
- **`/` always gives a float**, `//` floors integers, and `%` is the unit
  0.01, not modulo (`mod(a, b)`).
- **Determinism across platforms.** Only correctly rounded float operations
  and `libm` (pure Rust) are used, and `powi` is our own repeated squaring.
  Don't use `f64::powi`, `exp` or `sin` from std.
- **`Value` is 16 bytes** (a `const` assert): strings are `Rc<String>`,
  enums with fields are `Data(Rc<(tag, fields)>)`.

## Provenance

This replaces SPL (Superposition Language), whose interpreter lives in git
history before the parallax rewrite. Open/Resolved/Collapsed values,
sequential RNG, `fork`/`commit`, `pin`, `given`, `multiverse`, `while`,
recursion and `print` are gone by design (see README.md). The four
simulations were ported, so their numbers differ from SPL's, but the
findings carried over (e.g. pre-warm: calibrated plan 15 min, best 20 min).

## Performance

- About 80M operations a second. Pre-warm ~10s, tooluse ~12s (100 worlds; its
  forecasts are the heavy part), island and circumbinary
  ~5s, reactor 0.5s (release and opt-level 1 are similar).
- Measure with callgrind instruction counts (`circumbinary.px` with
  `worlds 4` is a good probe); wall time is too noisy.
- Measured wins: in-place reads of `v.f`, `v[i]`, `v.f[i]` (-15%),
  no allocation in element assignment (-8%), arguments pushed straight into
  the callee's frame, Int/Float fast paths. A general path walker was
  *slower* than cloning; outlining rare `eval` arms gained ~1%.
- Compiling the IR to closures was tried (October 2026): a full
  closure compiler with operand fast paths and identical ops accounting
  cut callgrind instructions by 20% on the circumbinary probe, but wall
  time didn't improve (circumbinary was slower, 4.1 s vs 3.6 s best of 5;
  indirect calls cost what the dispatch saved). It was dropped. The remaining
  costs are `Value` clone/drop and `Result` plumbing, so a real gain needs
  typed (unboxed `f64`) code, which the IR doesn't carry yet.
- Serving: `serve` answers a constant policy in ~26 µs and the adaptive
  tool-choice policy in ~0.1-1 ms; ~40 µs of that is bounding the call.
  Caching the bound per (policy, inputs) is the obvious next step if it
  matters.
- Studies are embarrassingly parallel by world, but `Value` uses `Rc`, so
  threads would each need their own machine.

## Open work

1. Parallel worlds; typed numeric code (see Performance).
2. Weighted evidence (importance sampling) if conditioning is ever needed;
   `given` was removed deliberately.
3. Paired differences are only for a plain `mean`/`probability` primary
   objective; other objectives get none.
