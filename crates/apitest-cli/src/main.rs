//! `apitest` command-line runner.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

use apitest::config::ProjectConfig;
use apitest::contract::{Contract, Spec};
use apitest::load::{Executor, LoadPlan, Scenario, Stage, Threshold, parse_duration, run_load};
use apitest::report::junit_xml;
use apitest::runner::{RunConfig, check_drift, run_suite};
use apitest::script::{LoadOptions, load_suites};

#[derive(Parser)]
#[command(
    name = "apitest",
    version,
    about = "API tests, load tests and contract pinning"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Common {
    /// Project config (default: apitest.nova in the current directory or a parent).
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
    /// Base URL for relative request paths.
    #[arg(long, short = 'u')]
    base_url: Option<String>,
    /// Variable for `cx.env()`, e.g. --var `API_USER=demo` (repeatable).
    #[arg(long = "var", value_parser = parse_kv)]
    vars: Vec<(String, String)>,
    /// Lockfile path (default from config, else apitest.lock.nova next to the config).
    #[arg(long)]
    lockfile: Option<PathBuf>,
    /// Do not fail when the live spec is incompatible with the pinned one.
    #[arg(long)]
    no_drift_check: bool,
    /// Suppress `cx.log()` output.
    #[arg(long, short = 'q')]
    quiet: bool,
    /// User-Agent for every request (default: apitest/<version>).
    #[arg(long)]
    user_agent: Option<String>,
    /// Do not send trace-context headers.
    #[arg(long)]
    no_trace: bool,
    /// Trace header formats, comma-separated: w3c, b3, b3multi, xray, gcp, datadog.
    #[arg(long, value_delimiter = ',')]
    trace_format: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run suites functionally.
    Run {
        /// Suite files (.rhai, .js). Default: `suites` from the config.
        files: Vec<PathBuf>,
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
        #[arg(long)]
        max_parallel: Option<usize>,
    },
    /// Run one test of a suite as a load scenario.
    Load {
        file: PathBuf,
        #[command(flatten)]
        common: Common,
        /// Suite name (if the file defines several).
        #[arg(long)]
        suite: Option<String>,
        /// Test to use as the scenario.
        #[arg(long, short = 't')]
        test: String,
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
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Contract operations.
    Contract {
        #[command(subcommand)]
        cmd: ContractCmd,
    },
}

#[derive(Subcommand)]
enum ContractCmd {
    /// Diff the live specs of the suites' contracts against their pins (no tests run).
    Check {
        files: Vec<PathBuf>,
        #[command(flatten)]
        common: Common,
    },
    /// Diff two spec files (pinned → candidate), e.g. in CI before deploying.
    Diff {
        pinned: PathBuf,
        candidate: PathBuf,
        #[arg(long, default_value = "tolerant")]
        mode: String,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))
}

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
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn project(common: &Common) -> Result<Option<ProjectConfig>> {
    match &common.config {
        Some(p) => ProjectConfig::load(p).map(Some),
        None => ProjectConfig::discover(&std::env::current_dir()?),
    }
}

fn run_config(common: &Common, project: Option<&ProjectConfig>) -> Result<RunConfig> {
    let mut cfg = RunConfig::default();
    if let Some(p) = project {
        cfg = p.apply(cfg);
        if cfg.lockfile.is_none() {
            cfg.lockfile = Some(p.root.join("apitest.lock.nova"));
        }
    }
    if let Some(u) = &common.base_url {
        cfg.base_url = u.clone();
    }
    if let Some(l) = &common.lockfile {
        cfg.lockfile = Some(l.clone());
    }
    cfg.vars.extend(common.vars.iter().cloned());
    cfg.check_drift = !common.no_drift_check;
    cfg.quiet = common.quiet;
    if let Some(ua) = &common.user_agent {
        cfg.user_agent = ua.clone();
    }
    if common.no_trace {
        cfg.tracing.enabled = false;
    }
    if !common.trace_format.is_empty() {
        cfg.tracing.formats = common
            .trace_format
            .iter()
            .map(|f| f.parse())
            .collect::<Result<_>>()?;
    }
    Ok(cfg)
}

fn suite_files(files: &[PathBuf], project: Option<&ProjectConfig>) -> Result<Vec<PathBuf>> {
    if !files.is_empty() {
        return Ok(files.to_vec());
    }
    match project {
        Some(p) if !p.suites.is_empty() => Ok(p.suites.iter().map(|s| p.resolve(s)).collect()),
        _ => bail!("no suite files given and no `suites` in apitest.nova"),
    }
}

fn write(path: &Path, content: &str) -> Result<()> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

async fn run(cli: Cli) -> Result<bool> {
    match cli.cmd {
        Cmd::Run {
            files,
            common,
            only,
            junit,
            json,
            update_lock,
            max_parallel,
        } => {
            let project = project(&common)?;
            let mut cfg = run_config(&common, project.as_ref())?;
            cfg.only = only;
            cfg.update_lock = update_lock;
            if let Some(n) = max_parallel {
                cfg.max_parallel = n;
            }
            let opts = LoadOptions {
                js_workers: project.as_ref().and_then(|p| p.js_workers).unwrap_or(2),
            };
            let mut reports = vec![];
            for f in suite_files(&files, project.as_ref())? {
                for s in load_suites(&f, &opts)? {
                    let r = run_suite(&s, &cfg).await;
                    print!("{}", r.render());
                    reports.push(r);
                }
            }
            let junit = junit.or_else(|| {
                project
                    .as_ref()
                    .and_then(|p| p.reports.junit.as_ref().map(|j| p.resolve(j)))
            });
            let json = json.or_else(|| {
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
            if update_lock && let Some(l) = &cfg.lockfile {
                eprintln!("updated {}", l.display());
            }
            Ok(reports.iter().all(apitest::SuiteReport::passed))
        }
        Cmd::Load {
            file,
            common,
            suite,
            test,
            vus,
            duration,
            iterations,
            stages,
            thresholds,
            validate_every,
            json,
        } => {
            let project = project(&common)?;
            let mut cfg = run_config(&common, project.as_ref())?;
            cfg.validate_every = validate_every;
            cfg.quiet = true;
            let executor = if let Some(st) = stages {
                let stages = st
                    .split(',')
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
                    .collect::<Result<Vec<_>>>()?;
                Executor::RampingVus { stages }
            } else if let Some(n) = iterations {
                Executor::Iterations { vus, iterations: n }
            } else {
                Executor::ConstantVus {
                    vus,
                    duration: parse_duration(&duration)?,
                }
            };
            let mut scenario = Scenario::new(&test, executor);
            for t in &thresholds {
                scenario.thresholds.push(Threshold::parse(t)?);
            }
            let max_vus = match &scenario.executor {
                Executor::RampingVus { stages } => {
                    stages.iter().map(|s| s.target).max().unwrap_or(1)
                }
                Executor::ConstantVus { vus, .. } | Executor::Iterations { vus, .. } => *vus,
            };
            let mut suites = load_suites(
                &file,
                &LoadOptions {
                    js_workers: max_vus.max(1),
                },
            )?;
            let s = match suite {
                Some(name) => {
                    let i = suites
                        .iter()
                        .position(|s| s.name == name)
                        .ok_or_else(|| anyhow!("no suite '{name}'"))?;
                    suites.swap_remove(i)
                }
                None => suites
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow!("no suites in file"))?,
            };
            let report = run_load(&LoadPlan::new(s).scenario(scenario), &cfg).await;
            print!("{}", report.render());
            if let Some(p) = json {
                write(&p, &serde_json::to_string_pretty(&report)?)?;
            }
            Ok(report.passed())
        }
        Cmd::Contract {
            cmd: ContractCmd::Check { files, common },
        } => {
            let project = project(&common)?;
            let cfg = run_config(&common, project.as_ref())?;
            let mut ok = true;
            for f in suite_files(&files, project.as_ref())? {
                for s in load_suites(&f, &LoadOptions::default())? {
                    for d in check_drift(&s.contracts, &cfg).await? {
                        println!(
                            "{} / {}: pinned v{} vs live v{} → {}",
                            s.name,
                            d.contract,
                            d.pinned_version,
                            d.live_version,
                            if d.is_breaking() {
                                "BREAKING"
                            } else {
                                "compatible"
                            }
                        );
                        for f in &d.findings {
                            println!(
                                "  {:?} {} @ {}: {}",
                                f.severity, f.operation, f.location, f.message
                            );
                        }
                        ok &= !d.is_breaking();
                    }
                }
            }
            Ok(ok)
        }
        Cmd::Contract {
            cmd:
                ContractCmd::Diff {
                    pinned,
                    candidate,
                    mode,
                },
        } => {
            let c = Contract::pinned("pinned", &pinned)?.mode(mode.parse()?);
            let live = Spec::from_file(&candidate)?;
            let d = c.diff(&live, None, &candidate.display().to_string());
            println!(
                "pinned v{} → candidate v{}: {}",
                d.pinned_version,
                d.live_version,
                if d.is_breaking() {
                    "BREAKING"
                } else {
                    "compatible"
                }
            );
            for f in &d.findings {
                println!(
                    "  {:?} {} @ {}: {}",
                    f.severity, f.operation, f.location, f.message
                );
            }
            Ok(!d.is_breaking())
        }
    }
}
