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
- `src/resolve.rs`: after parsing, gives every scope a fixed slot layout and resolves each use of a name to the slots it can be in (`ast::Ref`).
- `src/compile.rs`: compiles the resolved tree to closures (`Code`), deciding what's static (operator, variable location, whether a block needs a scope) once.
- `src/value.rs`: `Value`, the lazy cells (`Lazy`: Open, Future or Done, collapsing in place), `Scope` (slots) and `Copier` (the timeline copy for fork and multiverse).
- `src/interp.rs`: the runtime the compiled code calls into (`Machine`: RNG, pins, types, output, and every operation with real semantics), behind the public `Interpreter`. `src/stats.rs`: ensemble aggregation. `src/rng.rs`: the seeded RNG.
- `tests/programs.rs`: golden files, an `examples.md` check and a seed-replay check. `tests/semantics.rs`: behaviour that holds for any seed.

## Semantics that are easy to break

- **Forks** run in a `Copier::timeline` copy of the whole scope chain. Uncollapsed values are copied with aliasing preserved, and collapsed ones are shared. `commit` merges only the variables the fork *wrote*, level by level along the chain (`Var::written`), by slot: a copy has its original's layout.
- **Scopes are slots, resolved statically, bound dynamically.** The runtime scope chain always mirrors the source's block nesting, so `resolve` gives each scope a layout and each use a list of candidate `(up, slot)`s, innermost first. A slot is allocated when its scope is created but bound only when its `let` runs, so a read falls back to the next candidate while it's unbound (a use before the `let`, or a function called before a name it uses is bound). That reproduces the old by-name search exactly.
- **Fork and multiverse blocks don't own a scope**: they run in a copy of the enclosing one, so their `let`s are slots of the enclosing scope, which `commit` fills. Globals are the exception to static layouts: a later program run by the same `Interpreter` can add some (`Globals`, `Ref::depth`).
- **Functions**: a function stored in its own scope is `Slot::Fn` rather than a closure value. This avoids `Rc` cycles and makes a copied scope's functions follow the copy. A function committed out of a fork keeps closing over the fork's scope (see `merge_chain`).
- **Branch origins are `Weak`**, to avoid cycles. A dead origin level is unobservable, so `commit` skips it.
- **Recursion safety**: collapse, timeline copy and `Lazy` drop are all iterative, so long future chains can't overflow the stack. Recursion depth is limited by *measured* stack use (`STACK_SIZE`, `with_stack`), not a call count.
- **RNG**: xoshiro256++ with our own Lemire range sampling. Fork seeds are derived from the RNG state without advancing it, and universe seeds come from `(seed, i)`. Changing any of this changes every `.out` file.
- **Arrays have value semantics, copy-on-write** (`Rc::make_mut`). `AssignPath` *takes* the variable out of its slot so a unique array is written in place, then puts it back. Indexing observes the index.
- **Blocks** that bind nothing run in their parent's scope (`Block::binds`). This is *nearly* unobservable: a `let` in a fork inside such a block, once committed, lands in the parent (see `a_let_in_a_fork_binds_in_the_forking_scope`). Keep it as it is.
- **Loop bodies reuse their scope** between iterations when nothing kept hold of it (`Body::run_again`: no other strong or weak reference). A closure or an escaped fork's origin keeps it, and the next iteration gets a fresh one.
- **Reading `a.b[i]` in place** (`compile::access`) borrows the variable's slot while the indices are evaluated. That is only done when the indices can't write a variable (`no_writes`: no calls, no blocks), which also keeps "base before index" order observable-equivalent.
- **Integers** are i64 with checked arithmetic, and overflow is an error. `/` floors.

## Provenance

This is a port of a Python interpreter, now removed. The port was verified byte-for-byte against the Python version patched to use this RNG, across every test, example and simulation plus ~30 edge-case programs. The only deliberate difference is that Python ints were bigints. When deviating from Python behaviour, the comments in `value.rs` say why.

## Performance: what's been learned

- Reactor sim: ~34s in Python, ~0.8s now. Circumbinary ~2.2s, island ~1.5s (release).
- Measure with callgrind instruction counts. Wall time on these machines is too noisy for ±10% changes. Also diff the old binary against the new on edge-case programs (stdout, errors, several seeds): the golden files don't cover scoping corners.
- Slot resolution plus closure compilation, in instructions: circumbinary 28.6G → 14.1G, island 21.3G → 11.1G, reactor 7.5G → 6.0G. What each part was worth (island): slots -8%, a 16-byte `R<Value>` (boxed errors, `Rc<String>` strings) -4%, closures -36%, inline operands for constants and variables -8%.
- Slots alone gained little: lookup was smaller than thought, and giving *every* referenced name a global slot bloated the global scope that every fork copies (reactor +4%). Only names actually bound globally get one.
- Keep `R<Value>` at 16 bytes (there's a `const` assert): it's returned in registers from every closure.
- Profile shape now: circumbinary and island spend ~15% reading variables (`Scope::read`, RefCell borrow, clone) and the rest spread thin. Reactor is bound by timeline copies and allocation (malloc/free ~25%, `Copier` ~12%): every fork and universe copies the scope chain, two allocations per scope.
- Kept from before: the Int×Int fast path for binary operators and `Block::binds` elision.
- Tried and **reverted**: splitting scope names into their own array (reactor got 6% slower from the extra allocation). Measure every change.
- Value semantics cost: `s = f(s)` copies `s`'s arrays, because the caller still holds `s`. Hot loops in the sims update a local in place instead (see the `fly` comment in `simulations/circumbinary.spl`).

## Open work

1. Raise `UNIVERSES` in the circumbinary/island sims (16-24 now, so their percentages are coarse) and regenerate the `.out` files. The interpreter is now ~2x faster on both.
2. Reactor-style programs (many forks and universes) are bound by timeline copies. Ideas, unmeasured: one allocation per scope (slots inline), or copy-on-write scopes so a fork shares levels it never writes.
3. Not built, but discussed: lazy selection for an Open index (`a[open]` returning a future). The user chose "index observes" for now.
