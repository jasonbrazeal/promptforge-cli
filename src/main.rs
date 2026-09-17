//! `promptforge run <file.md> [--args TEXT | --args-file PATH] [--input
//! NAME=PATH]... [--output NAME=PATH]...`: run one PromptForge prompt against
//! a gateway. The run's input - what the prompt sees as `args` - comes from
//! `--args`, from the contents of `--args-file`, or is empty. No bare
//! positional input is accepted.
//!
//! `--input` seeds the run's store with a file before the run, under the
//! name the prompt reads with `store.read` (its `input:` declaration);
//! `--output` copies a store file the prompt wrote with `store.write` (its
//! `output:` declaration) to disk after a successful run. The prompt never
//! learns a host path: the store is the only door.
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
use promptforge_api_runtime::client::{GatewayClient, fetch_model_catalog};
use promptforge_api_runtime::types::cancel::CancelHandle;
use promptforge_api_runtime::types::models::{ModelCatalog, ModelDescriptor, ModelId};
use promptforge_api_runtime::types::observe::{NullObserver, Observer};
use promptforge_api_runtime::{
    CapabilityRegistry, Environment, Prompt, RequirementCheck, Requirements, RunContext, RunResult,
    Web, promptforge_version,
};
use shared_vfs::{Origin, VfsError, VfsRef};

use crate::terminal::{StderrObserver, StdinBroker};

/// The exit status of a run the operator interrupted with Ctrl-C.
const EXIT_CANCELLED: u8 = 130;
/// The exit status of every other failure.
const EXIT_FAILURE: u8 = 1;

/// The mount prefix of the run-scoped store inside the run's VFS: the
/// prompt's `store.read("paper.md")` resolves to `<mount>/paper.md`. The
/// engine defines this as `promptforge_vfs::STORE_MOUNT`, in a crate the
/// one-door rule keeps internal, and does not re-export it through
/// `promptforge-api-runtime`; this mirrors it. If the engine moves the
/// mount, the seed probe in [`seed_store`] fails loudly rather than writing
/// beside it.
const STORE_MOUNT: &str = "/_promptforge/store";

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
    /// Seed the run's store with the file at PATH under the store name NAME
    /// before the run, so the prompt can `store.read(NAME)`. Repeatable.
    #[arg(long, value_name = "NAME=PATH", value_parser = FileMapping::parse)]
    input: Vec<FileMapping>,
    /// After a successful run, copy the store file NAME the prompt wrote with
    /// `store.write(NAME)` to PATH on disk. Repeatable.
    #[arg(long, value_name = "NAME=PATH", value_parser = FileMapping::parse)]
    output: Vec<FileMapping>,
    /// Print the configuration in use and run lifecycle observations to
    /// stderr.
    #[arg(short, long)]
    verbose: bool,
}

/// One `NAME=PATH` pair from `--input` or `--output`: a store-internal name
/// the prompt addresses and a host path the CLI reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMapping {
    /// The store path as the prompt names it, e.g. `paper.md`.
    name: String,
    /// The host file the store entry is read from or written to.
    path: PathBuf,
}

impl FileMapping {
    /// Parses `NAME=PATH`. The name is a relative store path: non-empty, no
    /// leading `/`, no `..` segments, no backslashes - the same shape the
    /// prompt's `store` table accepts, checked here so a bad mapping fails
    /// at argument parsing rather than inside the run.
    fn parse(text: &str) -> Result<FileMapping, String> {
        let Some((name, path)) = text.split_once('=') else {
            return Err(format!("expected NAME=PATH, got {text:?}"));
        };
        if name.is_empty() {
            return Err(format!("the store name is empty in {text:?}"));
        }
        if name.starts_with('/') || name.contains('\\') || name.split('/').any(|s| s == "..") {
            return Err(format!(
                "the store name {name:?} must be a relative path without `..` or backslashes"
            ));
        }
        if path.is_empty() {
            return Err(format!("the host path is empty in {text:?}"));
        }
        Ok(FileMapping {
            name: name.to_owned(),
            path: PathBuf::from(path),
        })
    }

    /// The name's absolute location inside the run's VFS.
    fn store_path(&self) -> String {
        format!("{STORE_MOUNT}/{}", self.name)
    }
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
    // Read every seed file before any network call: a missing paper fails
    // fast, not after the catalog fetch.
    let mut seeds = Vec::with_capacity(args.input.len());
    for mapping in &args.input {
        let contents = tokio::fs::read_to_string(&mapping.path)
            .await
            .with_context(|| {
                format!(
                    "read --input {} from {}",
                    mapping.name,
                    mapping.path.display()
                )
            })?;
        seeds.push((mapping, contents));
    }

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
        for (mapping, contents) in &seeds {
            eprintln!(
                "input:   {} <- {} ({} bytes)",
                mapping.name,
                mapping.path.display(),
                contents.len()
            );
        }
        for mapping in &args.output {
            eprintln!("output:  {} -> {}", mapping.name, mapping.path.display());
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

    // Prepare, seed, run, extract: `Environment::run` would prepare and run
    // in one step, but the store the prompt sees exists only after prepare,
    // so seeding needs the two halves apart. The refusal for an
    // unsatisfiable prompt is reproduced here because `Environment::run`
    // owns it on the one-step path.
    let (ctx, requirements) = environment.prepare(&prompt, ctx);
    if !requirements.is_satisfied() {
        bail!("{}", render_requirements(&requirements));
    }
    let vfs = ctx.vfs_handle().clone();
    seed_store(&vfs, &seeds)?;
    let result = promptforge_api_runtime::run(&prompt, &input, ctx).await;
    if matches!(result, RunResult::Ok(_)) {
        extract_store(&vfs, &args.output).await?;
    }
    Ok(result)
}

/// Writes every `--input` file into the run's store under its store name.
/// The seeding capability drops at return, releasing its claims, so the
/// run's own identity never meets the host's.
fn seed_store(vfs: &VfsRef, seeds: &[(&FileMapping, String)]) -> Result<()> {
    if seeds.is_empty() {
        return Ok(());
    }
    let access = vfs
        .acquire(Origin::new("promptforge-cli --input"))
        .context("acquire the run's store for seeding")?;
    // The mount must exist before anything is written under it: a stat of
    // the mount root answers for a mounted store and is `NotFound` for an
    // unmounted path, which would mean the engine moved the mount.
    access.stat(STORE_MOUNT).with_context(|| {
        format!("the run's store is not mounted at {STORE_MOUNT}; the engine's store mount moved")
    })?;
    for (mapping, contents) in seeds {
        access
            .write(&mapping.store_path(), contents.as_bytes())
            .with_context(|| format!("seed the store file {} for --input", mapping.name))?;
    }
    Ok(())
}

/// Copies every `--output` store file the run left behind to its host
/// path. A store file the prompt never wrote is an error naming the
/// promise, not a bare not-found.
async fn extract_store(vfs: &VfsRef, outputs: &[FileMapping]) -> Result<()> {
    if outputs.is_empty() {
        return Ok(());
    }
    let access = vfs
        .acquire(Origin::new("promptforge-cli --output"))
        .context("acquire the run's store for extraction")?;
    for mapping in outputs {
        let contents = match access.read(&mapping.store_path()) {
            Ok(contents) => contents,
            Err(VfsError::NotFound(_)) => bail!(
                "--output {}: the run left no store file named {:?}; \
                 the prompt must `store.write({:?}, ...)` before it returns",
                mapping.name,
                mapping.name,
                mapping.name
            ),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read the store file {} for --output", mapping.name));
            }
        };
        tokio::fs::write(&mapping.path, &contents)
            .await
            .with_context(|| {
                format!(
                    "write --output {} to {}",
                    mapping.name,
                    mapping.path.display()
                )
            })?;
    }
    Ok(())
}

/// Renders an unsatisfied preflight report, one line per gap with required
/// versus actual, in the shape the engine's own refusal notice uses.
fn render_requirements(requirements: &Requirements) -> String {
    use std::fmt::Write as _;

    let mut notice = String::from("the environment cannot satisfy this prompt:");
    for id in &requirements.missing_required {
        // Writing into a String cannot fail; the Result is a trait artifact.
        let _ = write!(notice, "\n- missing required capability: {id}");
    }
    for conflict in &requirements.conflicts {
        let _ = write!(
            notice,
            "\n- conflicting capabilities: {} and {} cannot be activated together; \
             declare one or the other",
            conflict.first, conflict.second
        );
    }
    for unmet in &requirements.unmet_requirements {
        let line = match unmet.check {
            RequirementCheck::ContextMinimum => format!(
                "role '{}': requires a context of at least {} tokens; \
                 the current model provides {}",
                unmet.role, unmet.required, unmet.actual
            ),
            RequirementCheck::HardKeyword => format!(
                "role '{}': requires '{}'; the current model's thinking capability is {}",
                unmet.role, unmet.required, unmet.actual
            ),
            _ => format!(
                "role '{}': requires {}; the current model provides {}",
                unmet.role, unmet.required, unmet.actual
            ),
        };
        let _ = write!(notice, "\n- {line}");
    }
    notice
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

    use promptforge_api_runtime::types::models::ThinkingMode;

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
    fn input_and_output_mappings_parse_and_repeat() {
        let args = parse(&[
            "promptforge",
            "run",
            "p.md",
            "--input",
            "paper.md=/data/p2300r10.md",
            "--input",
            "meta.json=/data/meta.json",
            "--output",
            "report.json=/out/report.json",
        ])
        .expect("parses");
        assert_eq!(
            args.input,
            vec![
                FileMapping {
                    name: "paper.md".to_owned(),
                    path: PathBuf::from("/data/p2300r10.md"),
                },
                FileMapping {
                    name: "meta.json".to_owned(),
                    path: PathBuf::from("/data/meta.json"),
                },
            ]
        );
        assert_eq!(args.output.len(), 1);
        assert_eq!(
            args.output[0].store_path(),
            "/_promptforge/store/report.json"
        );
        // A host path may itself contain `=`; only the first one splits.
        let mapping = FileMapping::parse("a.md=/tmp/x=y.md").expect("parses");
        assert_eq!(mapping.path, PathBuf::from("/tmp/x=y.md"));
    }

    #[test]
    fn malformed_mappings_are_usage_errors() {
        for bad in [
            "paper.md",
            "=/tmp/x",
            "paper.md=",
            "/abs.md=/tmp/x",
            "../escape.md=/tmp/x",
            "a/../b.md=/tmp/x",
            "a\\b.md=/tmp/x",
        ] {
            assert!(FileMapping::parse(bad).is_err(), "{bad:?} must be rejected");
            assert!(
                parse(&["promptforge", "run", "p.md", "--input", bad]).is_err(),
                "{bad:?} must be rejected by clap"
            );
        }
    }

    #[tokio::test]
    async fn seeding_and_extraction_round_trip_through_the_store_mount() {
        let vfs = VfsRef::builder()
            .mount(STORE_MOUNT, shared_vfs::MemoryBackend::new())
            .build();
        let input = FileMapping::parse("paper.md=/unused").expect("parses");
        seed_store(&vfs, &[(&input, "# Title\n\nbody".to_owned())]).expect("seeds");

        let access = vfs.acquire(Origin::new("test")).expect("acquires");
        let seeded = access.read("/_promptforge/store/paper.md").expect("reads");
        assert_eq!(seeded, b"# Title\n\nbody");
        access
            .write("/_promptforge/store/report.json", b"{\"ok\":true}")
            .expect("writes");
        drop(access);

        let dir = std::env::temp_dir().join(format!("promptforge-cli-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).expect("creates the temp dir");
        let target = dir.join("report.json");
        let output = FileMapping {
            name: "report.json".to_owned(),
            path: target.clone(),
        };
        extract_store(&vfs, std::slice::from_ref(&output))
            .await
            .expect("extracts");
        assert_eq!(
            std::fs::read_to_string(&target).expect("the output landed on disk"),
            "{\"ok\":true}"
        );

        let missing = FileMapping {
            name: "never-written.json".to_owned(),
            path: dir.join("never.json"),
        };
        let error = extract_store(&vfs, std::slice::from_ref(&missing))
            .await
            .expect_err("a missing output is an error");
        let text = error.to_string();
        assert!(text.contains("never-written.json"), "{text}");
        assert!(text.contains("store.write"), "{text}");
        std::fs::remove_dir_all(&dir).expect("removes the temp dir");
    }

    #[test]
    fn an_unsatisfied_report_renders_every_gap() {
        let satisfied = Requirements::default();
        assert!(satisfied.is_satisfied());
        let text = render_requirements(&satisfied);
        assert_eq!(text, "the environment cannot satisfy this prompt:");
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
