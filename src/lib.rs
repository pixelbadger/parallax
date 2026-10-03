//! Interpreter for the Superposition Language (SPL).
//!
//! ```
//! let out = spl::run_source("print(2 + 3 * 4);", 0).unwrap();
//! assert_eq!(out, "14\n");
//! ```

pub mod ast;
pub mod error;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod rng;
pub mod stats;
pub mod value;

pub use error::Error;
pub use interp::Interpreter;

/// Runs `src` with the given seed and returns everything it printed.
///
/// The program runs on its own thread with [`interp::STACK_SIZE`] of stack.
pub fn run_source(src: &str, seed: u64) -> Result<String, Error> {
    with_stack(|| {
        let mut interp = Interpreter::new(seed, Vec::new());
        let program = interp.parse(src)?;
        interp.run(&program)?;
        Ok(String::from_utf8(interp.into_output()).expect("SPL output is UTF-8"))
    })
}

/// Runs `f` on a thread with enough stack for deep SPL recursion.
pub fn with_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(interp::STACK_SIZE)
            .spawn_scoped(scope, f)
            .expect("spawn interpreter thread")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
