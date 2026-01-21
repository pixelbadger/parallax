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

* **Observation**: The act of using a value (`print`, `if`) collapses its wave function.
* **Propagation**: Operations on `Open` values return `Resolved` futures. They do not collapse until necessary.
* **The Multiverse**: `fork` creates a branched reality. You can inspect the result of a dangerous calculation in a fork and decide to `commit` (keep side effects) or `discard` (rollback).
* **Pinning**: `pin x = open` persists a specific collapse across multiple execution runs (program restarts), enabling iterative simulation design.

## EBNF Grammar

```ebnf
(* === Lexical === *)
identifier  = letter , { letter | digit | '_' } ;
integer     = digit , { digit } ;

(* === Types === *)
type_ann    = [ '?' | '~' ] , identifier ;  (* ?Int, ~String *)

(* === Top Level === *)
program     = { statement } ;
block       = '{' , { statement } , [ expr ] , '}' ;

(* === Statements === *)
statement   = let_stmt | pin_stmt | reset_stmt | type_def 
            | commit_stmt | discard_stmt | func_decl | expr_stmt ;

let_stmt    = 'let' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
pin_stmt    = 'pin' , identifier , [ ':' , type_ann ] , '=' , expr , ';' ;
reset_stmt  = 'reset' , identifier , ';' ;

type_def    = 'type' , identifier , '=' , '{' , field_list , '}' , ';' ;
commit_stmt = 'commit' , identifier , ';' ;
discard_stmt= 'discard' , identifier , ';' ;

func_decl   = 'fn' , identifier , '(' , param_list , ')' , '=' , block ;

(* === Expressions === *)
expr        = term , { bin_op , term } ;
term        = factor , { '.' , identifier } ; (* Member access *)

factor      = integer 
            | 'open'
            | identifier , [ '(' , arg_list , ')' ] (* Var or Call *)
            | identifier , '{' , field_init , '}'   (* Struct Init *)
            | 'fork' , block
            | 'observe' , expr 
            | 'if' , expr , block , [ 'else' , block ] 
            | '(' , expr , ')' ;

bin_op      = '+' | '-' | '*' | '/' | '==' | '>' | '<' | '&&' | '||' ;
