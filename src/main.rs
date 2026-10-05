use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Map, Value as Json, json};

use parallax::{Engine, Error, ErrorKind, Limits, Options, Request};

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

#[derive(clap::Args)]
struct Serving {
    file: PathBuf,
    /// A JSON file of input values
    #[arg(long)]
    inputs: Option<PathBuf>,
    /// One input, as name=value (a JSON value, or text like "5 min")
    #[arg(long = "input", value_name = "NAME=VALUE")]
    input: Vec<String>,
    /// Which worlds forecasts imagine (default 0)
    #[arg(long, allow_negative_numbers = true)]
    seed: Option<i64>,
    /// Refuse a decision estimated to need more operations than this
    #[arg(long, default_value_t = Limits::default().max_operations)]
    max_operations: f64,
    /// Print compact JSON
    #[arg(long)]
    compact: bool,
}

#[derive(clap::Args)]
struct DecideArgs {
    #[command(flatten)]
    serving: Serving,
    /// The policy to follow: `name`, or `name[param]` in a family
    #[arg(long)]
    policy: String,
    /// A sequential policy's observation: JSON, or a file holding it
    #[arg(long)]
    observation: Option<String>,
    /// A sequential policy's step number
    #[arg(long)]
    step: Option<i64>,
    /// Earlier observations, `[{"step": n, "observation": ...}]`: JSON or a file
    #[arg(long)]
    history: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the studies and print their results
    Run(Common),
    /// Check the program, describe its inputs and estimate each study's work
    Check(Common),
    /// Serve one decision with a chosen policy, given what is known now
    Decide(DecideArgs),
    /// Serve decisions: read one JSON request per line on stdin and answer
    /// each on a line of stdout, checking the program once
    Serve(Serving),
}

fn read_json(path: &Path, what: &str) -> Result<Json, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::new(
            ErrorKind::Input,
            format!("can't read {}: {e}", path.display()),
        )
    })?;
    serde_json::from_str(&text).map_err(|e| {
        Error::new(
            ErrorKind::Input,
            format!("{} must hold {what}: {e}", path.display()),
        )
    })
}

/// JSON given inline, or the name of a file holding it.
fn json_arg(arg: &str, what: &str) -> Result<Json, Error> {
    match serde_json::from_str(arg) {
        Ok(j) => Ok(j),
        Err(_) if Path::new(arg).is_file() => read_json(Path::new(arg), what),
        Err(e) => Err(Error::new(
            ErrorKind::Input,
            format!("can't read {what} `{arg}` as JSON or a file: {e}"),
        )),
    }
}

fn inputs(file: &Option<PathBuf>, pairs: &[String]) -> Result<Map<String, Json>, Error> {
    let mut inputs = Map::new();
    if let Some(path) = file {
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
    for kv in pairs {
        let Some((k, v)) = kv.split_once('=') else {
            return Err(Error::new(
                ErrorKind::Input,
                format!("--input takes name=value, not `{kv}`"),
            ));
        };
        let v = serde_json::from_str(v).unwrap_or_else(|_| Json::String(v.to_string()));
        inputs.insert(k.trim().to_string(), v);
    }
    Ok(inputs)
}

fn options(c: &Common) -> Result<Options, Error> {
    Ok(Options {
        inputs: inputs(&c.inputs, &c.input)?,
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

fn read_src(file: &Path) -> Result<String, Error> {
    std::fs::read_to_string(file).map_err(|e| {
        Error::new(
            ErrorKind::Input,
            format!("can't read {}: {e}", file.display()),
        )
    })
}

fn engine(s: &Serving) -> Result<Engine, Error> {
    let mut e = Engine::new(&read_src(&s.file)?)?;
    e.limits.max_operations = s.max_operations;
    Ok(e)
}

fn decide(d: &DecideArgs) -> Result<Json, Error> {
    let s = &d.serving;
    let engine = engine(s)?;
    let history = match &d.history {
        Some(h) => serde_json::from_value(json_arg(h, "the history")?).map_err(|e| {
            Error::new(
                ErrorKind::Input,
                format!("the history must be [{{\"step\": n, \"observation\": ...}}]: {e}"),
            )
        })?,
        None => Vec::new(),
    };
    let req = Request {
        id: None,
        policy: d.policy.clone(),
        inputs: inputs(&s.inputs, &s.input)?,
        observation: d
            .observation
            .as_deref()
            .map(|o| json_arg(o, "the observation"))
            .transpose()?,
        step: d.step,
        history,
        seed: s.seed,
    };
    Ok(serde_json::to_value(engine.decide(&req)?).expect("serialisable decision"))
}

/// Answer requests from stdin until it closes. Each request's inputs are
/// laid over the ones given on the command line.
fn serve(s: &Serving) -> Result<(), Error> {
    let engine = engine(s)?;
    let base = inputs(&s.inputs, &s.input)?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Request>(&line) {
            Err(e) => json!({ "error": Error::new(ErrorKind::Input, format!("bad request: {e}")) }),
            Ok(mut req) => {
                let mut inputs = base.clone();
                inputs.extend(std::mem::take(&mut req.inputs));
                req.inputs = inputs;
                req.seed = req.seed.or(s.seed);
                match engine.decide(&req) {
                    Ok(d) => serde_json::to_value(d).expect("serialisable decision"),
                    Err(e) => match &req.id {
                        Some(id) => json!({ "id": id, "error": e }),
                        None => json!({ "error": e }),
                    },
                }
            }
        };
        // One line per answer, whatever `--compact` says.
        let text = serde_json::to_string(&answer).expect("JSON output");
        if writeln!(out, "{text}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (result, compact) = match &cli.command {
        Command::Run(c) | Command::Check(c) => {
            let is_run = matches!(cli.command, Command::Run(_));
            let r = (|| -> Result<Json, Error> {
                let src = read_src(&c.file)?;
                let opts = options(c)?;
                Ok(if is_run {
                    serde_json::to_value(parallax::run(&src, &opts)?)
                } else {
                    serde_json::to_value(parallax::check(&src, &opts)?)
                }
                .expect("serialisable report"))
            })();
            (r, c.compact)
        }
        Command::Decide(d) => (decide(d), d.serving.compact),
        Command::Serve(s) => match serve(s) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e) => (Err(e), true),
        },
    };
    match result {
        Ok(v) => {
            print(&v, compact);
            ExitCode::SUCCESS
        }
        Err(e) => {
            print(&json!({ "error": e }), compact);
            ExitCode::FAILURE
        }
    }
}
