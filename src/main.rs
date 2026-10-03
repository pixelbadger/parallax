use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use spl::Interpreter;

/// Run an SPL program.
#[derive(Parser)]
#[command(version)]
struct Args {
    filename: PathBuf,
    /// Initial RNG seed; omit for a random one (it is printed so the run can be replayed)
    #[arg(long, allow_negative_numbers = true)]
    seed: Option<i64>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    spl::with_stack(|| run(&args))
}

fn run(args: &Args) -> ExitCode {
    let code = match std::fs::read_to_string(&args.filename) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: cannot read {}: {e}", args.filename.display());
            return ExitCode::FAILURE;
        }
    };
    let mut out = BufWriter::new(io::stdout().lock());
    let _ = writeln!(out, "--- Executing {} ---", args.filename.display());

    let seed = args.seed.map_or_else(
        || RandomState::new().hash_one(0u8) as u32 as u64,
        |s| s as u64,
    );
    let mut interp = Interpreter::new(seed, out);
    let result = interp.parse(&code).and_then(|program| {
        if args.seed.is_none() {
            writeln!(
                interp.out(),
                "[SYS] Initial seed: {seed} (replay with --seed {seed})"
            )?;
        }
        interp.run(&program)
    });
    let _ = interp.out().flush();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
