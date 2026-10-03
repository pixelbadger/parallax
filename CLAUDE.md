# SPL interpreter: notes for agents

SPL (Superposition Language) is a probabilistic language. The language itself is documented in README.md (concepts, built-ins, EBNF). This file covers the implementation and the decisions behind it.

## Commands

```sh
cargo test                                   # unit + tests/semantics.rs + golden programs (~15s)
UPDATE_EXPECT=1 cargo test --test programs   # regenerate tests/*.out and simulations/*.out
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # CI runs both
cargo run --release -- --seed 0 simulations/reactor.spl
```

`[profile.dev] opt-level = 1` is deliberate: the golden tests run whole simulations.

## Layout

- `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`: hand-written lexer and recursive-descent parser. Names are interned to `Sym`. `ast::well_known` pre-interns the names the runtime needs, and its order is load-bearing.
- `src/value.rs`: `Value`, the lazy cells (`Lazy`: Open, Future or Done, collapsing in place), `Scope` and `Copier` (the timeline copy for fork and multiverse).
- `src/interp.rs`: the tree-walking evaluator. `src/stats.rs`: ensemble aggregation. `src/rng.rs`: the seeded RNG.
- `tests/programs.rs`: golden files, an `examples.md` check and a seed-replay check. `tests/semantics.rs`: behaviour that holds for any seed.

## Semantics that are easy to break

- **Forks** run in a `Copier::timeline` copy of the whole scope chain. Uncollapsed values are copied with aliasing preserved, and collapsed ones are shared. `commit` merges only the variables the fork *wrote*, level by level along the chain (`Var::written`).
- **Functions**: a function stored in its own scope is `Slot::Fn` rather than a closure value. This avoids `Rc` cycles and makes a copied scope's functions follow the copy. A function committed out of a fork keeps closing over the fork's scope (see `merge_chain`).
- **Branch origins are `Weak`**, to avoid cycles. A dead origin level is unobservable, so `commit` skips it.
- **Recursion safety**: collapse, timeline copy and `Lazy` drop are all iterative, so long future chains can't overflow the stack. Recursion depth is limited by *measured* stack use (`STACK_SIZE`, `with_stack`), not a call count.
- **RNG**: xoshiro256++ with our own Lemire range sampling. Fork seeds are derived from the RNG state without advancing it, and universe seeds come from `(seed, i)`. Changing any of this changes every `.out` file.
- **Arrays have value semantics, copy-on-write** (`Rc::make_mut`). `AssignPath` *takes* the variable out of its slot so a unique array is written in place, then puts it back. Indexing observes the index.
- **Blocks** that bind nothing run in their parent's scope (`Block::binds`). This is unobservable, and it's an optimisation.
- **Integers** are i64 with checked arithmetic, and overflow is an error. `/` floors.

## Provenance

This is a port of a Python interpreter, now removed. The port was verified byte-for-byte against the Python version patched to use this RNG, across every test, example and simulation plus ~30 edge-case programs. The only deliberate difference is that Python ints were bigints. When deviating from Python behaviour, the comments in `value.rs` say why.

## Performance: what's been learned

- Reactor sim: ~34s in Python, ~1s now. Circumbinary ~4.5s, island ~3.2s (release).
- Measure with callgrind instruction counts. Wall time on these machines is too noisy for ±10% changes.
- Profile shape: the eval dispatch itself (~28%), scope lookup (~17%, linear scans up the chain), then malloc and drops.
- Kept: the Int×Int fast path for binary operators (-5%) and `Block::binds` pre-sizing and elision (-6%).
- Tried and **reverted**: splitting scope names into their own array (reactor got 6% slower from the extra allocation). Measure every change.
- Value semantics cost: `s = f(s)` copies `s`'s arrays, because the caller still holds `s`. Hot loops in the sims update a local in place instead (see the `fly` comment in `simulations/circumbinary.spl`).

## Open work

1. **Interpreter speed-up (est. 1.5-2x)**: resolve variables to `(depth, slot)` at parse time, with pre-allocated slots per block and a fallback for names not yet bound. Then compile the AST to closures. Must keep fork/commit semantics: commit merges by name, so blocks need a name table. Gate it on all golden outputs staying identical.
2. With that, raise `UNIVERSES` in the circumbinary/island sims (16-24 now, so their percentages are coarse) and regenerate the `.out` files.
3. Not built, but discussed: lazy selection for an Open index (`a[open]` returning a future). The user chose "index observes" for now.
