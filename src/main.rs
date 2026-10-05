use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Map, Value as Json, json};

use parallax::{Error, ErrorKind, Limits, Options};

/// parallax: compare interventions across the same uncertain worlds.
///
/// Results, and errors, are printed as JSON.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Common {
    file: PathBuf,
    /// A JSON file of input values
    #[arg(long)]
    inputs: Option<PathBuf>,
    /// One input, as name=value (a JSON value, or text like "5 min")
    #[arg(long = "input", value_name = "NAME=VALUE")]
    input: Vec<String>,
    /// Only this study
    #[arg(long)]
    study: Option<String>,
    /// Override every study's seed
    #[arg(long, allow_negative_numbers = true)]
    seed: Option<i64>,
    /// Override every study's number of worlds
    #[arg(long)]
    worlds: Option<u64>,
    /// Refuse studies estimated to need more operations than this
    #[arg(long, default_value_t = Limits::default().max_operations)]
    max_operations: f64,
    /// Refuse studies with more worlds than this
    #[arg(long, default_value_t = Limits::default().max_worlds)]
    max_worlds: u64,
    /// Print compact JSON
    #[arg(long)]
    compact: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the studies and print their results
    Run(Common),
    /// Check the program, describe its inputs and estimate each study's work
    Check(Common),
}

fn options(c: &Common) -> Result<Options, Error> {
    let mut inputs = Map::new();
    if let Some(path) = &c.inputs {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                ErrorKind::Input,
                format!("can't read {}: {e}", path.display()),
            )
        })?;
        match serde_json::from_str(&text) {
            Ok(Json::Object(m)) => inputs = m,
            _ => {
                return Err(Error::new(
                    ErrorKind::Input,
                    format!("{} must hold a JSON object of inputs", path.display()),
                ));
            }
        }
    }
    for kv in &c.input {
        let Some((k, v)) = kv.split_once('=') else {
            return Err(Error::new(
                ErrorKind::Input,
                format!("--input takes name=value, not `{kv}`"),
            ));
        };
        let v = serde_json::from_str(v).unwrap_or_else(|_| Json::String(v.to_string()));
        inputs.insert(k.trim().to_string(), v);
    }
    Ok(Options {
        inputs,
        seed: c.seed,
        worlds: c.worlds,
        study: c.study.clone(),
        limits: Limits {
            max_operations: c.max_operations,
            max_worlds: c.max_worlds,
            ..Limits::default()
        },
    })
}

fn print(v: &Json, compact: bool) {
    let s = if compact {
        serde_json::to_string(v)
    } else {
        serde_json::to_string_pretty(v)
    };
    println!("{}", s.expect("JSON output"));
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (c, is_run) = match &cli.command {
        Command::Run(c) => (c, true),
        Command::Check(c) => (c, false),
    };
    let result = (|| -> Result<Json, Error> {
        let src = std::fs::read_to_string(&c.file).map_err(|e| {
            Error::new(
                ErrorKind::Input,
                format!("can't read {}: {e}", c.file.display()),
            )
        })?;
        let opts = options(c)?;
        Ok(if is_run {
            serde_json::to_value(parallax::run(&src, &opts)?)
        } else {
            serde_json::to_value(parallax::check(&src, &opts)?)
        }
        .expect("serialisable report"))
    })();
    match result {
        Ok(v) => {
            print(&v, c.compact);
            ExitCode::SUCCESS
        }
        Err(e) => {
            print(&json!({ "error": e }), c.compact);
            ExitCode::FAILURE
        }
    }
}
