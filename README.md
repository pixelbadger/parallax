# Superposition Language (SPL)

SPL is a ternary-substrate probabilistic programming language. Unlike traditional languages where values are either `null` or defined, SPL values exist in three states: **Open** (superposition), **Resolved** (computed but unobserved), and **Collapsed** (observed).

It treats execution as observation (`observe`) and supports timeline management (`fork`, `commit`, `pin`) and Monte Carlo simulation (`multiverse`, `given`) as first-class primitives.

## The Core Ontology

| State | Symbol | Definition | Analogy |
| :--- | :--- | :--- | :--- |
| **Open** | `?T` | A raw source of probability: `open` (0–99) or `open(lo, hi)`. | A spinning coin. |
| **Resolved** | `~T` | A computed value depending on Open sources. | "Heads + 5". The math is ready, but the coin hasn't stopped. |
| **Collapsed** | `T` | A fixed, immutable value. | The coin showed Heads; the result is 10. |

## Key Concepts

* **Observation**: The act of using a value (`print`, `if`, `observe`) collapses its wave function. Collapsing a Resolved value also collapses every Open value it depends on.
* **Propagation**: Operations on `Open` values return `Resolved` futures. They do not collapse until necessary.
* **Forks**: `fork` creates a branched reality. You can inspect the result of a dangerous calculation in a fork and decide to `commit` (apply the variables the fork wrote) or `discard` (rollback). A fork draws from its own random stream, so forking never changes what the parent timeline draws next. A fork's result is lazy like anything else: `observe` it inside the fork if it must be decided in that timeline.
* **The Multiverse**: `multiverse n { ... }` runs the block in `n` independent universes and aggregates what they produce (see below). `given cond;` inside a universe throws that universe away when `cond` is false, which conditions the results on `cond`.
* **Pinning**: `pin x = open` persists a specific collapse for the rest of the interpreter run, surviving re-seeds (`seed(n)`) and forks, until it is cleared with `reset x;`. This lets you hold one draw fixed while re-running the rest of a simulation under different seeds.
* **Determinism**: Every run is driven by a single seeded RNG (xoshiro256++, with the interpreter's own range sampling, so a seed gives the same draws on every platform). Pass `--seed N` (or call `seed(N)` in the program) and the run is fully reproducible. Without a seed, the interpreter picks one and prints it so the run can be replayed.
* **Mutation**: `x = expr;` rebinds an existing variable in the nearest scope that defines it. `grid[y][x] = v;` and `p.field = v;` write into part of a variable's value. Inside a fork either changes only the fork's copy until committed.

## Simulation

`multiverse n { block }` evaluates `block` once per universe, each in an isolated copy of the current scope, observes the result, and returns:

* for integer results, an `Ensemble { n, rejected, total, mean, min, max, median, hits, rate }`. `hits` counts non-zero results and `rate` is `hits` as a whole percentage, so a boolean condition gives a probability. `mean` is rounded; fields are `none` if every universe was rejected.
* for struct results, a struct of the same type whose every field is an `Ensemble`.

Universe `i` is seeded from (current seed, `i`), so **two multiverses of the same size see the same random draws**: comparing policies in them is a fair, low-noise comparison (common random numbers). A multiverse also leaves the outer random stream exactly where it was.

```
let r = multiverse 1000 { open(1, 6) + open(1, 6) == 7 };
print(r.rate);   # ~17
```

Full studies live in [`simulations/`](simulations):

* [`reactor.spl`](simulations/reactor.spl): five reactor-operating policies compared over the same 400 shifts, then conditioned on the coolant pump failing.
* [`circumbinary.spl`](simulations/circumbinary.spl): a three-body problem. A habitat orbits a binary star from an uncertain launch. How often is it flung out or burnt up, and can mission control save it by forecasting thruster burns in forks, or by living three futures and committing the best?
* [`island.spl`](simulations/island.spl): a spatial predator–prey ecology on a grid of patches, with seasons, droughts, disease and migration. A ranger's policies are compared on one surveyed island, held fixed with `pin` across universes and a re-seed. The final populations aggregate into a per-patch heat map.

Values are 64-bit signed integers (and string literals for labels); `/` is integer division, rounding down. Arithmetic that overflows is an error.

## Arrays

`[a, b, c]` makes an array, `a[i]` reads an element (from 0) and `a[i] = v;` writes one. Arrays nest, so `grid[y][x]` works, and they mix with structs: `p.cells[3] = 0;`.

* **Value semantics**: `let b = a; b[0] = 5;` leaves `a` unchanged, and a function can't modify the caller's array, so return the new one. Copies are copy-on-write: a write is in place unless the array is shared, so filling an array in a loop is linear time.
* **Laziness**: indexing observes the index, as `if` observes its condition. Elements are not observed until used, so `[open, open]` holds two unobserved values. `array(n, v)` repeats the *same* value `n` times: `array(3, open)` is one draw seen three times, so use a loop for independent draws.
* **Multiverse**: a universe may produce an array. The result is an array of the same length whose every element is aggregated (an `Ensemble` per element, or a struct of them). Every universe must produce the same length.

## Running

The interpreter is written in Rust. With a [Rust toolchain](https://rustup.rs) installed:

```sh
cargo run --release -- program.spl            # random seed, printed so you can replay it
cargo run --release -- --seed 42 program.spl  # deterministic run
cargo install --path .                        # or install the `spl` binary
```

### Testing

```sh
cargo test                                    # unit, semantics and program tests
UPDATE_EXPECT=1 cargo test --test programs    # regenerate tests/*.out and simulations/*.out
```

`tests/*.spl` and `simulations/*.spl` run with `--seed 0` and must print exactly their `.out` file. Every example in [`examples.md`](examples.md) must run cleanly, and an unseeded run must replay exactly from the seed it reports. `tests/semantics.rs` covers behaviour that holds for every seed. CI also runs `cargo fmt --check` and `cargo clippy -- -D warnings`.

Execution runs every top-level statement in order, then calls `main()` if it is defined.

### Built-ins

| Function | Behaviour |
| :--- | :--- |
| `print(a, b, ...)` | Observes (collapses) each argument and prints it. |
| `seed(n)` | Re-seeds the RNG with integer `n`. Pinned values are not affected. |
| `min(a, b)`, `max(a, b)`, `abs(a)` | Like operators, these stay unobserved futures if an argument is. |
| `len(a)` | Number of elements in array `a`. |
| `array(n, v)` | An array of `n` copies of `v`. |
| `push(a, v)` | A new array: `a` with `v` appended (`a` is unchanged; this copies `a`). |

SPL recursion is supported to a depth of tens of thousands of calls (over 100,000 in a release build); deeper recursion stops with "Recursion too deep".

## EBNF Grammar

```ebnf
(* === Lexical === *)
letter      = 'a'..'z' | 'A'..'Z' | '_' ;
digit       = '0'..'9' ;
identifier  = letter , { letter | digit } ;  (* except keywords *)
integer     = digit , { digit } ;
string      = '"' , { ? any character except '"' and newline ? } , '"' ;
keyword     = 'fn' | 'let' | 'pin' | 'reset' | 'type' | 'commit' | 'discard'
            | 'fork' | 'observe' | 'open' | 'if' | 'else'
            | 'repeat' | 'while' | 'multiverse' | 'given' ;
comment     = '#' , { ? any character except newline ? } ;
(* Whitespace and comments may appear between any two tokens and are ignored. *)

(* === Types === *)
type_ann    = [ '?' | '~' ] , identifier ;  (* Int = Collapsed, ?Int = Open, ~Int = Resolved *)

(* === Top Level === *)
program     = { statement } ;
block       = '{' , { statement } , [ expr ] , '}' ;
(* A block evaluates to its trailing expr, or to its last statement when that
   statement is an expression; otherwise to nothing. *)

(* === Statements === *)
statement   = let_stmt | pin_stmt | assign_stmt | place_stmt | reset_stmt | type_def
            | commit_stmt | discard_stmt | given_stmt | func_decl | expr_stmt ;

let_stmt    = 'let' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
assign_stmt = identifier , '=' , expr , ';' ;     (* variable must already exist *)
place_stmt  = identifier , place , { place } , '=' , expr , ';' ;
place       = '[' , expr , ']' | '.' , identifier ;  (* value first, then indices *)
pin_stmt    = 'pin' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
reset_stmt  = 'reset' , identifier , ';' ;
type_def    = 'type' , identifier , '=' , '{' , [ field_list ] , '}' , ';' ;
commit_stmt = 'commit' , identifier , ';' ;
discard_stmt= 'discard' , identifier , ';' ;
given_stmt  = 'given' , expr , ';' ;    (* false: reject the current universe *)
func_decl   = 'fn' , identifier , '(' , [ param_list ] , ')' , '=' , block ;
expr_stmt   = expr , ';'
            | block_expr , [ ';' ] ;          (* block expressions need no ';' *)

field_list  = identifier , { ',' , identifier } , [ ',' ] ;
param_list  = identifier , { ',' , identifier } , [ ',' ] ;
arg_list    = expr , { ',' , expr } ;
field_inits = identifier , ':' , expr , { ',' , identifier , ':' , expr } , [ ',' ] ;

(* === Expressions (loosest binding first; binary operators are left-associative) === *)
expr        = and_expr , { '||' , and_expr } ;
and_expr    = cmp_expr , { '&&' , cmp_expr } ;
cmp_expr    = add_expr , { ( '==' | '>' | '<' ) , add_expr } ;
add_expr    = mul_expr , { ( '+' | '-' ) , mul_expr } ;
mul_expr    = postfix , { ( '*' | '/' ) , postfix } ;   (* '/' is integer division *)
postfix     = primary , { '.' , identifier | '[' , expr , ']' } ;  (* member, element *)
(* '[' directly after a block_expr starts a new statement, not an index. *)

primary     = integer
            | string                     (* only for print and '==' *)
            | 'open' , [ '(' , expr , ',' , expr , ')' ]   (* 0..99, or lo..hi inclusive *)
            | identifier , '(' , [ arg_list ] , ')'      (* call *)
            | identifier , '{' , [ field_inits ] , '}'   (* struct init *)
            | identifier                                 (* variable *)
            | '[' , [ expr , { ',' , expr } , [ ',' ] ] , ']'   (* array *)
            | 'observe' , expr          (* extends as far right as possible *)
            | block_expr
            | '(' , expr , ')' ;

block_expr  = 'fork' , block
            | 'if' , expr , block , [ 'else' , block ]
            | 'repeat' , expr , block          (* run block expr times *)
            | 'while' , expr , block
            | 'multiverse' , expr , block ;    (* run block in expr universes *)
(* In the expr directly before a block a struct init is only allowed inside
   parentheses, so `if flag { ... }` reads `flag` as a variable, not a struct. *)
```
