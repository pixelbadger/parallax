//! parallax: a small language for comparing interventions across the same
//! uncertain worlds.
//!
//! A program declares what is known (inputs), what isn't (worlds of keyed
//! uncertain facts), what the world does (models), what we might do
//! (policies) and how to choose (studies). The host gets back a typed
//! result tree; nothing in a program can reach outside it.
//!
//! ```
//! let src = r#"
//! action Bet = small | big
//! world Coin { uncertain heads ~ bernoulli(0.5) }
//! model play(bet: Bet) -> Int = {
//!     let stake = match bet { small => 1, big => 3 }
//!     if Coin.heads { stake } else { -stake }
//! }
//! policy cautious = small
//! policy bold = big
//! study which {
//!     worlds 1000
//!     maximize mean(outcome)
//!     report probability(outcome > 0)
//! }
//! "#;
//! let report = parallax::run(src, &parallax::Options::default()).unwrap();
//! let study = &report.studies[0];
//! // Both policies saw the same 1000 coins.
//! assert_eq!(study.policies[0].metrics[0].value, study.policies[1].metrics[0].value);
//! ```

pub mod ast;
pub mod check;
pub mod cost;
pub mod decide;
pub mod error;
pub mod eval;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod stats;
pub mod study;
pub mod units;
pub mod value;
pub mod world;

pub use decide::{Decision, Engine, Request};
pub use error::{Error, ErrorKind};
pub use study::{CheckReport, Limits, Options, Report};

/// Parse and check a program.
pub fn compile(src: &str) -> Result<ir::Program, Error> {
    let ast = parser::parse(src)?;
    check::check(src, &ast)
}

/// Check a program and estimate each study's work, without running it.
pub fn check(src: &str, opts: &Options) -> Result<CheckReport, Error> {
    let p = compile(src)?;
    study::check(src, &p, opts)
}

/// Serve one decision with a chosen policy. A host serving many keeps an
/// [`Engine`] instead, so the program is checked once.
pub fn decide(src: &str, req: &Request) -> Result<Decision, Error> {
    Engine::new(src)?.decide(req)
}

/// Run every study (or `opts.study`).
pub fn run(src: &str, opts: &Options) -> Result<Report, Error> {
    let p = compile(src)?;
    study::run(src, &p, opts)
}
