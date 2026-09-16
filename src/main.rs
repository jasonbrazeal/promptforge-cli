//! `promptforge run <file.md> [--args TEXT | --args-file PATH]`: run one
//! PromptForge prompt against a gateway. The run's input - what the prompt
//! sees as `args` - comes from `--args`, from the contents of `--args-file`,
//! or is empty. No bare positional input is accepted.
//!
//! Configuration is environment only:
//! - `PROMPTFORGE_GATEWAY_URL`: the gateway's OpenAI-shaped API root, for
//!   example `http://127.0.0.1:8081/v1`. Required.
//! - `PROMPTFORGE_GATEWAY_API_KEY`: the gateway's shared bearer. Required.
//! - `PROMPTFORGE_MODEL`: the gateway model id every declared role binds to.
//!   Optional; defaults to the first model in the gateway catalog.
//!
//! The prompt's final text goes to stdout. Exit status is 0 on success, 130
//! when interrupted with Ctrl-C, 1 for any other failure (reported on stderr).
//!
//! `main` is the process boundary: arguments, the Ctrl-C signal, output,
//! and the exit status. [`run`] owns everything between.

mod terminal;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use promptforge_api::client::{GatewayClient, fetch_model_catalog};
use promptforge_api::{
    CapabilityRegistry, Environment, Prompt, RunContext, RunResult, Web, promptforge_version,
};
use shared_promptforge_api::cancel::CancelHandle;
use shared_promptforge_api::models::{ModelCatalog, ModelDescriptor, ModelId};
use shared_promptforge_api::observe::{NullObserver, Observer};

use crate::terminal::{StderrObserver, StdinBroker};

/// The exit status of a run the operator interrupted with Ctrl-C.
const EXIT_CANCELLED: u8 = 130;
/// The exit status of every other failure.
const EXIT_FAILURE: u8 = 1;

/// The `promptforge` command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "promptforge",
    version,
    about = "Run PromptForge prompts against a gateway."
)]
struct Cli {
    /// The subcommand to execute.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands the CLI accepts.
#[derive(Debug, Subcommand)]
enum Command {
    /// Parse a prompt file and run its sections top to bottom.
    Run(RunArgs),
}

/// Arguments for `promptforge run`.
#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Path to the prompt file; its frontmatter must declare `promptforge:`.
    file: PathBuf,
    /// The run's input, exposed to the prompt as `args`.
    #[arg(long, conflicts_with = "args_file")]
    args: Option<String>,
    /// Read the run's input from PATH instead; the file's contents become
    /// `args` verbatim.
    #[arg(long, value_name = "PATH")]
    args_file: Option<PathBuf>,
    /// Print the configuration in use and run lifecycle observations to
    /// stderr.
    #[arg(short, long)]
    verbose: bool,
}

/// Where the run's `args` string comes from.
#[derive(Debug, PartialEq, Eq)]
enum InputSource<'a> {
    /// Text given on the command line.
    Text(&'a str),
    /// The verbatim contents of a file.
    File(&'a Path),
    /// Nothing: `args` is the empty string.
    Empty,
}

impl RunArgs {
    /// Returns which input the flags selected. clap rejects the both-set
    /// state at parse time, so it cannot occur here.
    fn input_source(&self) -> InputSource<'_> {
        match (&self.args, &self.args_file) {
            (Some(text), _) => InputSource::Text(text),
            (None, Some(path)) => InputSource::File(path),
            (None, None) => InputSource::Empty,
        }
    }
}

/// The gateway settings read from the environment.
struct Gateway {
    /// The API root, e.g. `http://127.0.0.1:8081/v1`.
    url: String,
    /// The shared bearer token.
    key: String,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The bearer is a secret; never render it.
        f.debug_struct("Gateway")
            .field("url", &self.url)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Reads a required environment variable, naming it in every failure.
fn required_env(name: &str, hint: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        Ok(_) => bail!("{name} is empty; {hint}"),
        Err(std::env::VarError::NotPresent) => bail!("{name} is not set; {hint}"),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{name} is not valid Unicode; {hint}"),
    }
}

/// Reads an optional environment variable, treating blank as unset.
fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Reads the gateway URL and key from the environment.
fn gateway_from_env() -> Result<Gateway> {
    Ok(Gateway {
        url: required_env(
            "PROMPTFORGE_GATEWAY_URL",
            "set it to the gateway API root, e.g. http://127.0.0.1:8081/v1",
        )?,
        key: required_env(
            "PROMPTFORGE_GATEWAY_API_KEY",
            "set it to the gateway's shared bearer token ([server] api_key)",
        )?,
    })
}

/// Picks the run's model from `catalog`: the requested id when given, else
/// the catalog's first entry. An id absent from the catalog is an error
/// listing what the gateway offers; an empty catalog is an error too.
fn select_model(catalog: &ModelCatalog, requested: Option<&str>) -> Result<ModelDescriptor> {
    let available = || {
        catalog
            .models()
            .iter()
            .map(|model| model.id().name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match requested {
        Some(name) => {
            let id = ModelId::gateway(name).with_context(|| format!("model id `{name}`"))?;
            catalog.get(&id).cloned().ok_or_else(|| {
                anyhow!(
                    "model `{name}` is not in the gateway catalog; available: {}",
                    available()
                )
            })
        }
        None => catalog
            .models()
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("the gateway catalog lists no models")),
    }
}

/// Runs one prompt with the gateway configuration read from the environment.
///
/// # Errors
/// Returns an error when a gateway variable is unset, the prompt or args
/// file cannot be read, the file is not a promptforge prompt or fails to
/// parse, the model catalog cannot be fetched, or the requested model is
/// absent from it. A failure inside the run itself is not an error here: it
/// is reported as [`RunResult::Failure`].
async fn run(args: RunArgs, cancel: CancelHandle) -> Result<RunResult> {
    let gateway = gateway_from_env()?;
    // Built from the same two variables, so a bad URL fails here, not on
    // the first inference.
    let client = GatewayClient::from_env().context("build the gateway client")?;

    let source = tokio::fs::read_to_string(&args.file)
        .await
        .with_context(|| format!("read prompt file {}", args.file.display()))?;
    if promptforge_version(&source).is_none() {
        bail!(
            "{} is not a promptforge prompt: its frontmatter declares no `promptforge:` version",
            args.file.display()
        );
    }

    let input = match args.input_source() {
        InputSource::Text(text) => text.to_owned(),
        InputSource::File(path) => tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read args file {}", path.display()))?,
        InputSource::Empty => String::new(),
    };

    let observer: Arc<dyn Observer> = if args.verbose {
        Arc::new(StderrObserver)
    } else {
        Arc::new(NullObserver::default())
    };
    let run_id = format!("cli-{:016x}", fastrand::u64(..));
    let prompt = Prompt::parse(&source, &run_id, observer.as_ref())
        .with_context(|| format!("parse prompt file {}", args.file.display()))?;

    if args.verbose {
        eprintln!("gateway: {}", gateway.url);
        eprintln!("api key: {} characters", gateway.key.len());
        eprintln!("prompt:  {}", args.file.display());
        match args.input_source() {
            InputSource::File(path) => {
                eprintln!("args:    {} bytes from {}", input.len(), path.display());
            }
            InputSource::Text(_) | InputSource::Empty => eprintln!("args:    {input:?}"),
        }
        eprintln!("run id:  {run_id}");
    }

    let catalog = fetch_model_catalog(&gateway.url, &gateway.key)
        .await
        .with_context(|| format!("fetch the model catalog from {}", gateway.url))?;
    let model = select_model(&catalog, optional_env("PROMPTFORGE_MODEL").as_deref())?;
    if args.verbose {
        eprintln!(
            "model:   {} (context {})",
            model.id().name(),
            model.context()
        );
    }

    let web = Web::new(&gateway.url, gateway.key.clone())
        .context("build the promptforge/web capability")?;
    let mut registry = CapabilityRegistry::new();
    registry
        .register(Arc::new(web))
        .context("register the promptforge/web capability")?;
    let environment = Environment::new().registry(registry).client(client);

    let ctx = RunContext::new(run_id)
        .observer(observer)
        .cancel(cancel)
        .input_broker(Arc::new(StdinBroker))
        .model(model);
    Ok(environment.run(&prompt, &input, ctx).await)
}

/// Trips `cancel` on the first Ctrl-C.
fn install_ctrl_c(cancel: &CancelHandle) {
    let cancel = cancel.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => cancel.cancel(),
            Err(error) => {
                eprintln!("warning: cannot listen for Ctrl-C, run is not cancellable: {error}");
            }
        }
    });
}

/// Renders an error and its cause chain, one cause per line.
fn render_chain(error: &(dyn std::error::Error + 'static)) -> String {
    use std::fmt::Write as _;

    let mut text = format!("error: {error}");
    let mut cause = error.source();
    while let Some(next) = cause {
        // Writing into a String cannot fail; the Result is a trait artifact.
        let _ = write!(text, "\n  caused by: {next}");
        cause = next.source();
    }
    text
}

/// Maps the outcome of [`run`] to the text printed and the exit status.
/// Success text goes to stdout; everything else goes to stderr.
fn outcome(result: Result<RunResult>) -> (Option<String>, Option<String>, ExitCode) {
    match result {
        Ok(RunResult::Ok(text)) => (Some(text), None, ExitCode::SUCCESS),
        Ok(RunResult::Cancelled) => (
            None,
            Some("cancelled".to_owned()),
            ExitCode::from(EXIT_CANCELLED),
        ),
        Ok(RunResult::Failure(error)) => (
            None,
            Some(render_chain(&error)),
            ExitCode::from(EXIT_FAILURE),
        ),
        Err(error) => (
            None,
            Some(render_chain(error.as_ref())),
            ExitCode::from(EXIT_FAILURE),
        ),
    }
}

/// Parses arguments, runs the prompt, and selects the exit status. The
/// current-thread flavor suffices: the executor interleaves a run's chains
/// on one driver task.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let cancel = CancelHandle::new();
    install_ctrl_c(&cancel);

    let Command::Run(args) = cli.command;
    let (stdout, stderr, code) = outcome(run(args, cancel).await);
    if let Some(text) = stdout {
        println!("{text}");
    }
    if let Some(text) = stderr {
        eprintln!("{text}");
    }
    code
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use shared_promptforge_api::models::ThinkingMode;

    use super::*;

    fn parse(argv: &[&str]) -> Result<RunArgs, clap::Error> {
        Cli::try_parse_from(argv).map(|cli| {
            let Command::Run(run) = cli.command;
            run
        })
    }

    #[test]
    fn the_file_alone_means_empty_args() {
        let args = parse(&["promptforge", "run", "p.md"]).expect("parses");
        assert_eq!(args.file, PathBuf::from("p.md"));
        assert_eq!(args.input_source(), InputSource::Empty);
        assert!(!args.verbose);
    }

    #[test]
    fn args_and_args_file_select_their_sources() {
        let args = parse(&["promptforge", "run", "p.md", "--args", "hi"]).expect("parses");
        assert_eq!(args.input_source(), InputSource::Text("hi"));

        let args =
            parse(&["promptforge", "run", "p.md", "--args-file", "in.txt", "-v"]).expect("parses");
        assert_eq!(args.input_source(), InputSource::File(Path::new("in.txt")));
        assert!(args.verbose);
    }

    #[test]
    fn bare_positional_input_and_both_flags_are_usage_errors() {
        assert!(parse(&["promptforge", "run", "p.md", "hello"]).is_err());
        assert!(
            parse(&[
                "promptforge",
                "run",
                "p.md",
                "--args",
                "a",
                "--args-file",
                "b"
            ])
            .is_err()
        );
        assert!(parse(&["promptforge", "run"]).is_err());
        assert!(parse(&["promptforge", "run", "p.md", "--model", "m"]).is_err());
    }

    fn descriptor(name: &str) -> ModelDescriptor {
        ModelDescriptor::new(
            ModelId::gateway(name).expect("a valid id"),
            "d",
            NonZeroU32::new(4096).expect("non-zero"),
            ThinkingMode::Never,
        )
    }

    fn catalog(names: &[&str]) -> ModelCatalog {
        ModelCatalog::new(names.iter().map(|name| descriptor(name))).expect("unique ids")
    }

    #[test]
    fn model_selection_defaults_to_the_first_catalog_entry() {
        let model = select_model(&catalog(&["first", "second"]), None).expect("selects");
        assert_eq!(model.id().name(), "first");
    }

    #[test]
    fn a_requested_model_must_be_in_the_catalog() {
        let model = select_model(&catalog(&["a", "b"]), Some("b")).expect("selects");
        assert_eq!(model.id().name(), "b");

        let error = select_model(&catalog(&["a", "b"]), Some("zzz")).expect_err("absent");
        let text = error.to_string();
        assert!(text.contains("`zzz`"), "{text}");
        assert!(text.contains("available: a, b"), "{text}");
    }

    #[test]
    fn an_empty_catalog_is_an_error() {
        assert!(select_model(&catalog(&[]), None).is_err());
    }

    #[test]
    fn outcomes_map_to_streams_and_exit_codes() {
        let (out, err, code) = outcome(Ok(RunResult::Ok("done".to_owned())));
        assert_eq!(out.as_deref(), Some("done"));
        assert!(err.is_none());
        assert_eq!(code, ExitCode::SUCCESS);

        let (out, err, code) = outcome(Ok(RunResult::Cancelled));
        assert!(out.is_none());
        assert_eq!(err.as_deref(), Some("cancelled"));
        assert_eq!(code, ExitCode::from(EXIT_CANCELLED));

        let inner = std::io::Error::other("disk gone");
        let (out, err, code) = outcome(Err(anyhow::Error::new(inner).context("read prompt")));
        assert!(out.is_none());
        assert_eq!(
            err.as_deref(),
            Some("error: read prompt\n  caused by: disk gone")
        );
        assert_eq!(code, ExitCode::from(EXIT_FAILURE));
    }

    #[test]
    fn the_gateway_debug_output_redacts_the_key() {
        let gateway = Gateway {
            url: "http://127.0.0.1:8081/v1".to_owned(),
            key: "top-secret".to_owned(),
        };
        let text = format!("{gateway:?}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("top-secret"), "{text}");
    }
}
