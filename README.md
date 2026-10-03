# Superposition Language (SPL)

SPL is a ternary-substrate probabilistic programming language. Unlike traditional languages where values are either `null` or defined, SPL values exist in three states: **Open** (superposition), **Resolved** (computed but unobserved), and **Collapsed** (observed).

It treats execution as observation (`observe`) and supports timeline management (`fork`, `commit`, `pin`) as first-class primitives.

## The Core Ontology

| State | Symbol | Definition | Analogy |
| :--- | :--- | :--- | :--- |
| **Open** | `?T` | A raw source of probability. | A spinning coin. |
| **Resolved** | `~T` | A computed value depending on Open sources. | "Heads + 5". The math is ready, but the coin hasn't stopped. |
| **Collapsed** | `T` | A fixed, immutable value. | The coin showed Heads; the result is 10. |

## Key Concepts

* **Observation**: The act of using a value (`print`, `if`, `observe`) collapses its wave function. Collapsing a Resolved value also collapses every Open value it depends on.
* **Propagation**: Operations on `Open` values return `Resolved` futures. They do not collapse until necessary.
* **The Multiverse**: `fork` creates a branched reality. You can inspect the result of a dangerous calculation in a fork and decide to `commit` (keep side effects) or `discard` (rollback).
* **Pinning**: `pin x = open` persists a specific collapse for the rest of the interpreter run, surviving re-seeds (`seed(n)`) and forks, until it is cleared with `reset x;`. This lets you hold one draw fixed while re-running the rest of a simulation under different seeds.
* **Determinism**: Every run is driven by a single seeded RNG. Pass `--seed N` (or call `seed(N)` in the program) and the run is fully reproducible. Without a seed, the interpreter picks one and prints it so the run can be replayed.

## Running

```sh
python interpreter.py program.spl            # random seed, printed so you can replay it
python interpreter.py --seed 42 program.spl  # deterministic run
python tests/run_tests.py                    # run the test suite (use --update to regenerate expected output)
```

Execution runs every top-level statement in order, then calls `main()` if it is defined.

### Built-ins

| Function | Behaviour |
| :--- | :--- |
| `print(a, b, ...)` | Observes (collapses) each argument and prints it. |
| `seed(n)` | Re-seeds the RNG with integer `n`. Pinned values are not affected. |

## EBNF Grammar

```ebnf
(* === Lexical === *)
letter      = 'a'..'z' | 'A'..'Z' | '_' ;
digit       = '0'..'9' ;
identifier  = letter , { letter | digit } ;  (* except keywords *)
integer     = digit , { digit } ;
keyword     = 'fn' | 'let' | 'pin' | 'reset' | 'type' | 'commit' | 'discard'
            | 'fork' | 'observe' | 'open' | 'if' | 'else' ;
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
statement   = let_stmt | pin_stmt | reset_stmt | type_def
            | commit_stmt | discard_stmt | func_decl | expr_stmt ;

let_stmt    = 'let' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
pin_stmt    = 'pin' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
reset_stmt  = 'reset' , identifier , ';' ;
type_def    = 'type' , identifier , '=' , '{' , [ field_list ] , '}' , ';' ;
commit_stmt = 'commit' , identifier , ';' ;
discard_stmt= 'discard' , identifier , ';' ;
func_decl   = 'fn' , identifier , '(' , [ param_list ] , ')' , '=' , block ;
expr_stmt   = expr , ';'
            | block_expr , [ ';' ] ;          (* if / fork need no ';' *)

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
postfix     = primary , { '.' , identifier } ;           (* member access *)

primary     = integer
            | 'open'
            | identifier , '(' , [ arg_list ] , ')'      (* call *)
            | identifier , '{' , [ field_inits ] , '}'   (* struct init *)
            | identifier                                 (* variable *)
            | 'observe' , expr          (* extends as far right as possible *)
            | block_expr
            | '(' , expr , ')' ;

block_expr  = 'fork' , block
            | 'if' , expr , block , [ 'else' , block ] ;
(* In an 'if' condition a struct init is only allowed inside parentheses,
   so `if flag { ... }` reads `flag` as a variable, not a struct. *)
```
