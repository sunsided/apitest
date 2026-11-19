//! `apitest` command-line runner.
//!
//! Exit codes: `0` when everything passed, `1` when a test, threshold or
//! contract check failed (or `apitest call` got an unexpected status), `2` when
//! the run itself could not be carried out.

#![forbid(unsafe_code)]

mod call;
mod drift;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

use apitest::config::ProjectConfig;
use apitest::contract::{Contract, DriftReport, Spec};
use apitest::diagnostic;
use apitest::load::{Executor, LoadPlan, Scenario, Stage, Threshold, parse_duration, run_load};
use apitest::report::junit_xml;
use apitest::runner::{RunConfig, check_drift, run_suite};
use apitest::script::{LoadOptions, load_suites};
use apitest::suite::Suite;

/// Top-level command line.
#[derive(Parser)]
#[command(
    name = "apitest",
    version,
    about = "API tests, load tests and contract pinning",
    long_about = None
)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    cmd: Cmd,
}

/// Options shared by every subcommand that runs suites.
#[derive(Args, Clone)]
struct Common {
    /// Project config (default: apitest.nova in the current directory or a parent).
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
    /// Base URL for relative request paths.
    #[arg(long, short = 'u')]
    base_url: Option<String>,
    /// Lockfile path (default from config, else apitest.lock.nova next to the config).
    #[arg(long)]
    lockfile: Option<PathBuf>,
    /// Do not fail when a compared spec is incompatible with the pinned one.
    #[arg(long)]
    no_drift_check: bool,
    /// Drift targets to compare against.
    #[command(flatten)]
    drift: drift::Flags,
    /// Suppress `cx.log()` output.
    #[arg(long, short = 'q')]
    quiet: bool,
    /// Variables, User-Agent and tracing.
    #[command(flatten)]
    request: RequestFlags,
}

/// Request-level options shared by every subcommand that sends requests.
#[derive(Args, Clone)]
struct RequestFlags {
    /// Variable for `cx.env()` and `${NAME}` in the config, e.g. --var `API_USER=demo` (repeatable).
    #[arg(long = "var", value_parser = parse_kv)]
    vars: Vec<(String, String)>,
    /// User-Agent for every request (default: from the config, else `apitest/<version>`).
    #[arg(long)]
    user_agent: Option<String>,
    /// Do not send trace-context headers.
    #[arg(long)]
    no_trace: bool,
    /// Trace header formats, comma-separated: w3c, b3, b3multi, xray, gcp, datadog.
    #[arg(long, value_delimiter = ',')]
    trace_format: Vec<String>,
}

impl RequestFlags {
    /// Layers the flags over `cfg`: vars are added, the rest replace the config.
    ///
    /// # Errors
    ///
    /// Fails on an unknown `--trace-format`.
    fn apply(&self, cfg: &mut RunConfig) -> Result<()> {
        cfg.vars.extend(self.vars.iter().cloned());
        if let Some(ua) = &self.user_agent {
            cfg.user_agent.clone_from(ua);
        }

        if self.no_trace {
            cfg.tracing.enabled = false;
        }

        if !self.trace_format.is_empty() {
            cfg.tracing.formats = self
                .trace_format
                .iter()
                .map(|f| f.parse())
                .collect::<Result<_>>()?;
        }

        Ok(())
    }
}

/// Subcommands.
#[derive(Subcommand)]
enum Cmd {
    /// Run suites functionally.
    Run(RunArgs),
    /// Run one test of a suite as a load scenario.
    Load(LoadArgs),
    /// Send one request to a configured service, no `OpenAPI` spec needed.
    ///
    /// Prints the response body to stdout. Exits 0 for a 2xx status (or the
    /// --expect status), 1 for any other status, 2 for errors.
    Call(call::Args),
    /// Contract operations.
    Contract {
        /// The contract operation to run.
        #[command(subcommand)]
        cmd: ContractCmd,
    },
}

/// Arguments of `apitest run`.
#[derive(Args)]
struct RunArgs {
    /// Suite files (.rhai, .js). Default: `suites` from the config.
    files: Vec<PathBuf>,
    /// Shared options.
    #[command(flatten)]
    common: Common,
    /// Only run tests whose name contains this (repeatable).
    #[arg(long)]
    only: Vec<String>,
    /// Write a `JUnit` XML report.
    #[arg(long)]
    junit: Option<PathBuf>,
    /// Write a JSON report.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Record the used API surface into the lockfile.
    #[arg(long)]
    update_lock: bool,
    /// Maximum number of parallel tests (default from config).
    #[arg(long)]
    max_parallel: Option<usize>,
}

/// Arguments of `apitest load`.
#[derive(Args)]
struct LoadArgs {
    /// Suite file (.rhai, .js) that defines the scenario test.
    file: PathBuf,
    /// Shared options.
    #[command(flatten)]
    common: Common,
    /// Suite name (if the file defines several).
    #[arg(long)]
    suite: Option<String>,
    /// Test to use as the scenario.
    #[arg(long, short = 't')]
    test: String,
    /// Virtual users for constant-VU and iteration runs.
    #[arg(long, default_value_t = 10)]
    vus: usize,
    /// Constant-VU duration, e.g. 30s.
    #[arg(long, default_value = "10s")]
    duration: String,
    /// Shared iteration budget instead of a duration.
    #[arg(long)]
    iterations: Option<u64>,
    /// Ramping stages, e.g. "10s:20,30s:20,10s:0" (duration:target).
    #[arg(long)]
    stages: Option<String>,
    /// Pass/fail criteria, e.g. "http.p95 < 300ms" (repeatable).
    #[arg(long = "threshold")]
    thresholds: Vec<String>,
    /// Validate every n-th contract call.
    #[arg(long, default_value_t = 100)]
    validate_every: u64,
    /// Write a JSON report.
    #[arg(long)]
    json: Option<PathBuf>,
}

/// Subcommands of `apitest contract`.
#[derive(Subcommand)]
enum ContractCmd {
    /// Diff the suites' contracts against their pins (no tests run): the live
    /// specs, or the drift targets chosen with --against.
    Check {
        /// Suite files (.rhai, .js). Default: `suites` from the config.
        files: Vec<PathBuf>,
        /// Shared options.
        #[command(flatten)]
        common: Common,
    },
    /// Diff two spec files (pinned → candidate), e.g. in CI before deploying.
    Diff {
        /// The pinned spec file.
        pinned: PathBuf,
        /// The candidate spec file to check against the pin.
        candidate: PathBuf,
        /// Compatibility mode: tolerant or strict.
        #[arg(long, default_value = "tolerant")]
        mode: String,
    },
}

/// Parses a `--var KEY=VALUE` argument; the value may itself contain `=`.
fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))
}

/// Parses the command line, runs it on a multi-threaded runtime and maps the
/// outcome to the exit codes documented on the crate.
///
/// # Panics
///
/// Panics when the tokio runtime cannot be built.
fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match rt.block_on(run(cli)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            let color = diagnostic::use_color(&std::io::stderr());
            eprint!("{}", diagnostic::render_error(&e, color));
            ExitCode::from(2)
        }
    }
}

/// Loads the project config from `--config`, or discovers `apitest.nova`
/// upwards from the current directory (`None` when there is none).
fn project(common: &Common) -> Result<Option<ProjectConfig>> {
    match &common.config {
        Some(p) => ProjectConfig::load(p).map(Some),
        None => ProjectConfig::discover(&std::env::current_dir()?),
    }
}

/// Builds the run configuration: defaults, then the project config, then the
/// command-line flags, each layer overriding the previous one.
fn run_config(common: &Common, project: Option<&ProjectConfig>) -> Result<RunConfig> {
    let mut cfg = RunConfig::default();
    if let Some(p) = project {
        cfg = p.apply(cfg);
        if cfg.lockfile.is_none() {
            cfg.lockfile = Some(p.root.join("apitest.lock.nova"));
        }
    }

    if let Some(u) = &common.base_url {
        cfg.base_url.clone_from(u);
    }

    if let Some(l) = &common.lockfile {
        cfg.lockfile = Some(l.clone());
    }

    cfg.check_drift = !common.no_drift_check;
    common.drift.apply(&mut cfg)?;
    cfg.quiet = common.quiet;
    common.request.apply(&mut cfg)?;

    Ok(cfg)
}

/// The suite files to run: the ones given on the command line, else the
/// project's `suites`.
fn suite_files(files: &[PathBuf], project: Option<&ProjectConfig>) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }

    match project {
        Some(p) if !p.suites.is_empty() => Ok(p.suites.iter().map(|s| p.resolve(s)).collect()),
        _ => bail!("no suite files given and no `suites` in apitest.nova"),
    }
}

/// Loads every suite of the files to run (see [`suite_files`]).
fn load_all(
    files: &[PathBuf],
    project: Option<&ProjectConfig>,
    opts: &LoadOptions,
) -> Result<Vec<Suite>> {
    let mut suites = vec![];
    for f in suite_files(files, project)? {
        suites.extend(load_suites(&f, opts)?);
    }

    Ok(suites)
}

/// The project whose other suites [`drift::check_targets`] must also look at:
/// only when files were given, since otherwise every project suite is loaded.
fn extra_project<'a>(
    files: &[PathBuf],
    project: Option<&'a ProjectConfig>,
) -> Option<&'a ProjectConfig> {
    project.filter(|_| !files.is_empty())
}

/// Writes `content` to `path`, creating missing parent directories.
fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }

    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

/// Runs a subcommand; `Ok(false)` means it ran but something failed.
async fn run(cli: Cli) -> Result<bool> {
    match cli.cmd {
        Cmd::Run(args) => run_tests(args).await,
        Cmd::Load(args) => run_load_test(args).await,
        Cmd::Call(args) => call::call(args).await,
        Cmd::Contract {
            cmd: ContractCmd::Check { files, common },
        } => contract_check(&files, &common).await,
        Cmd::Contract {
            cmd:
                ContractCmd::Diff {
                    pinned,
                    candidate,
                    mode,
                },
        } => contract_diff(&pinned, &candidate, &mode),
    }
}

/// `apitest run`: runs every suite, prints the reports and writes the
/// requested `JUnit`/JSON files.
async fn run_tests(args: RunArgs) -> Result<bool> {
    let project = project(&args.common)?;
    let mut cfg = run_config(&args.common, project.as_ref())?;
    cfg.only = args.only;
    cfg.update_lock = args.update_lock;
    if let Some(n) = args.max_parallel {
        cfg.max_parallel = n;
    }

    let opts = LoadOptions {
        js_workers: project.as_ref().and_then(|p| p.js_workers).unwrap_or(2),
    };
    let suites = load_all(&args.files, project.as_ref(), &opts)?;
    drift::check_targets(&cfg, &suites, extra_project(&args.files, project.as_ref()))?;

    let color = diagnostic::use_color(&std::io::stdout());
    let mut reports = vec![];
    for s in &suites {
        let r = run_suite(s, &cfg).await;
        // miette on the terminal; the JUnit and JSON reports stay plain.
        print!("{}", r.render_terminal(color));
        reports.push(r);
    }

    let junit = args.junit.or_else(|| {
        project
            .as_ref()
            .and_then(|p| p.reports.junit.as_ref().map(|j| p.resolve(j)))
    });
    let json = args.json.or_else(|| {
        project
            .as_ref()
            .and_then(|p| p.reports.json.as_ref().map(|j| p.resolve(j)))
    });
    if let Some(p) = junit {
        write(&p, &junit_xml(&reports))?;
        eprintln!("wrote {}", p.display());
    }

    if let Some(p) = json {
        write(&p, &serde_json::to_string_pretty(&reports)?)?;
        eprintln!("wrote {}", p.display());
    }

    if args.update_lock
        && let Some(l) = &cfg.lockfile
    {
        eprintln!("updated {}", l.display());
    }

    Ok(reports.iter().all(apitest::SuiteReport::passed))
}

/// `apitest load`: runs one test as a load scenario and checks its thresholds.
async fn run_load_test(args: LoadArgs) -> Result<bool> {
    let project = project(&args.common)?;
    let mut cfg = run_config(&args.common, project.as_ref())?;
    cfg.validate_every = args.validate_every;
    cfg.quiet = true;

    let mut scenario = Scenario::new(&args.test, executor(&args)?);
    for t in &args.thresholds {
        scenario.thresholds.push(Threshold::parse(t)?);
    }

    let suites = load_suites(
        &args.file,
        &LoadOptions {
            js_workers: max_vus(&scenario.executor).max(1),
        },
    )?;
    drift::check_targets(&cfg, &suites, project.as_ref())?;
    let s = pick_suite(suites, args.suite.as_deref())?;

    let report = run_load(&LoadPlan::new(s).scenario(scenario), &cfg).await;
    print!("{}", report.render());
    if let Some(p) = args.json {
        write(&p, &serde_json::to_string_pretty(&report)?)?;
    }

    Ok(report.passed())
}

/// Chooses the executor: `--stages` wins over `--iterations`, which wins over
/// a constant-VU `--duration`.
fn executor(args: &LoadArgs) -> Result<Executor> {
    if let Some(st) = &args.stages {
        return Ok(Executor::RampingVus {
            stages: parse_stages(st)?,
        });
    }

    if let Some(n) = args.iterations {
        return Ok(Executor::Iterations {
            vus: args.vus,
            iterations: n,
        });
    }

    Ok(Executor::ConstantVus {
        vus: args.vus,
        duration: parse_duration(&args.duration)?,
    })
}

/// Parses `--stages` such as `10s:20,30s:20,10s:0` (`duration:target VUs`).
fn parse_stages(spec: &str) -> Result<Vec<Stage>> {
    spec.split(',')
        .map(|s| {
            let (d, t) = s
                .trim()
                .split_once(':')
                .ok_or_else(|| anyhow!("stage '{s}' must be duration:target"))?;
            Ok(Stage {
                duration: parse_duration(d)?,
                target: t.parse()?,
            })
        })
        .collect()
}

/// Peak number of concurrent VUs the executor will run (one JS runtime each).
fn max_vus(executor: &Executor) -> usize {
    match executor {
        Executor::RampingVus { stages } => stages.iter().map(|s| s.target).max().unwrap_or(1),
        Executor::ConstantVus { vus, .. } | Executor::Iterations { vus, .. } => *vus,
    }
}

/// Picks the suite called `name`, or the first one when no name is given.
fn pick_suite(mut suites: Vec<Suite>, name: Option<&str>) -> Result<Suite> {
    match name {
        Some(name) => {
            let i = suites
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| anyhow!("no suite '{name}'"))?;
            Ok(suites.swap_remove(i))
        }
        None => suites
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no suites in file")),
    }
}

/// `apitest contract check`: diffs each suite's live specs against their pins.
async fn contract_check(files: &[PathBuf], common: &Common) -> Result<bool> {
    let project = project(common)?;
    let cfg = run_config(common, project.as_ref())?;
    let suites = load_all(files, project.as_ref(), &LoadOptions::default())?;
    drift::check_targets(&cfg, &suites, extra_project(files, project.as_ref()))?;

    let mut ok = true;
    for s in &suites {
        for d in check_drift(&s.contracts, &cfg).await? {
            println!(
                "{} / {}: pinned v{} vs {} v{} ({}) → {}",
                s.name,
                d.contract,
                d.pinned_version,
                d.target,
                d.live_version,
                d.source,
                verdict(&d)
            );
            print_findings(&d);
            ok &= !d.is_breaking();
        }
    }

    Ok(ok)
}

/// `apitest contract diff`: diffs a candidate spec file against a pinned one
/// under compatibility `mode`.
fn contract_diff(pinned: &Path, candidate: &Path, mode: &str) -> Result<bool> {
    let c = Contract::pinned("pinned", pinned)?.mode(mode.parse()?);
    let live = Spec::from_file(candidate)?;
    let d = c.diff(&live, None, &candidate.display().to_string());
    println!(
        "pinned v{} → candidate v{}: {}",
        d.pinned_version,
        d.live_version,
        verdict(&d)
    );
    print_findings(&d);

    Ok(!d.is_breaking())
}

/// One-word verdict for a drift report.
fn verdict(d: &DriftReport) -> &'static str {
    if d.is_breaking() {
        "BREAKING"
    } else {
        "compatible"
    }
}

/// Prints each finding of `d` on its own indented line.
fn print_findings(d: &DriftReport) {
    for f in &d.findings {
        println!(
            "  {:?} {} @ {}: {}",
            f.severity, f.operation, f.location, f.message
        );
    }
}
