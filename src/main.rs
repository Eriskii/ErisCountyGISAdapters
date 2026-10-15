//! `eris-county-gis`: runs agents over US counties to write parcel adapters, validates specs,
//! signs in, and reports where the registry stands.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use erisagentauth::claude::{self, ClaudeAccount};
use erisagentauth::openai::{self, Method, OpenAiAccount};
use erisagentauth::{App, Event};
use eriscountygis::assist::Assistant;
use eriscountygis::counties::{self, Counties};
use eriscountygis::http::{Http, Limits};
use eriscountygis::journal::Journal;
use eriscountygis::models::{ANTHROPIC, CODEX, Endpoints};
use eriscountygis::registry::{Registry, Standing, now};
use eriscountygis::runtime::{Runtime, VALIDATE_WITHIN};
use eriscountygis::scheduler::{Scheduler, Settings};
use eriscountygis::spec::Spec;
use eriscountygis::view::View;
use eriscountygis::work::Work;
use eriscountygis::{models, tools, tui, validate};
use erisharness::Harness;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APP: App = App { name: "ErisCountyGISAdapters", originator: "eriscountygis" };

#[derive(Parser)]
#[command(name = "eris-county-gis", about = "Write and validate a parcel adapter for every US county")]
struct Cli {
    /// The adapter registry the repository publishes.
    #[arg(long, global = true, default_value = "registry")]
    registry: PathBuf,
    /// Run state: the journal, the agents, the login, downloads. Kept out of the repository.
    #[arg(long, global = true, default_value = "state")]
    state: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run agents over the selected counties (every county when none are named).
    Run {
        /// States by postal abbreviation, such as WI,GA.
        #[arg(long, value_delimiter = ',')]
        states: Vec<String>,
        /// Counties by FIPS, such as 13121.
        #[arg(long, value_delimiter = ',')]
        counties: Vec<String>,
        /// Jobs running at once.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Models cheapest first, as model[:effort],…, such as haiku:medium,sonnet:high; haiku,
        /// sonnet and opus mean the newest Claude model of that family. By default: with a Claude
        /// login, Haiku at medium, Sonnet at high, then ChatGPT's flagship (or Opus) at high; with
        /// ChatGPT alone, its small model at medium, then its flagship at medium and high.
        #[arg(long)]
        ladder: Option<String>,
        /// Attempts on each rung before climbing to the next.
        #[arg(long, default_value_t = 1)]
        attempts_per_rung: usize,
        /// Failed validations an attempt may make.
        #[arg(long, default_value_t = 8)]
        validate_budget: usize,
        /// Skip the statewide search and go straight to counties.
        #[arg(long)]
        no_statewide: bool,
        /// Validate every saved adapter first; counties whose adapter fails are searched again.
        #[arg(long)]
        recheck: bool,
        /// Try counties again that used up the ladder.
        #[arg(long)]
        retry_failed: bool,
        /// Print progress lines instead of drawing the terminal view.
        #[arg(long)]
        no_tui: bool,
    },
    /// Validate a spec file for a county and print the report.
    Validate { spec: PathBuf, fips: String },
    /// Sign in with ChatGPT, or bind a Claude login with --claude.
    Login {
        /// Use a device code instead of the browser.
        #[arg(long, conflicts_with = "claude")]
        device: bool,
        /// Bind Claude Code's credentials file (by default $CLAUDE_CONFIG_DIR/.credentials.json,
        /// or ~/.claude/.credentials.json). The login is read from that file on every run and
        /// never refreshed here, so Claude Code stays signed in; it lasts until another
        /// `login --claude`. When the file's token has expired, open Claude Code to refresh it.
        #[arg(long, value_name = "FILE", num_args = 0..=1)]
        claude: Option<Option<PathBuf>>,
    },
    /// Use the ChatGPT login in a credential file another Eris app wrote.
    ImportLogin { file: PathBuf },
    /// List the signed-in logins' models and the default ladder.
    Models,
    /// Where the registry stands, by state.
    Status,
    /// Rebuild the embedded county file from census.gov.
    RegenerateCounties {
        #[arg(default_value = "data/counties.json.gz")]
        out: PathBuf,
    },
}

fn accounts_dir(state: &Path) -> Result<PathBuf> {
    let dir = state.join("accounts");
    std::fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}

/// The ChatGPT and Claude logins kept in the run state, announcing on one channel.
struct Accounts {
    openai: OpenAiAccount,
    claude: ClaudeAccount,
    events: tokio::sync::broadcast::Receiver<Event>,
}

fn accounts(state: &Path) -> Result<Accounts> {
    let dir = accounts_dir(state)?;
    let (events, receiver) = tokio::sync::broadcast::channel(64);
    let openai_auth = env_or("ERISCOUNTYGIS_OPENAI_AUTH", openai::AUTH);
    let claude_auth = env_or("ERISCOUNTYGIS_CLAUDE_AUTH", claude::AUTH);
    Ok(Accounts {
        openai: OpenAiAccount::load(dir.join("chatgpt.json"), &openai_auth, APP, events.clone())?,
        claude: ClaudeAccount::load(dir.join("claude.json"), &endpoints().claude_api, &claude_auth, events)?,
        events: receiver,
    })
}

fn endpoints() -> Endpoints {
    Endpoints {
        codex: env_or("ERISCOUNTYGIS_CODEX_API", openai::CODEX_API),
        claude_api: env_or("ERISCOUNTYGIS_CLAUDE_API", claude::API),
    }
}

/// Where Claude Code keeps its login: `$CLAUDE_CONFIG_DIR/.credentials.json`, or
/// `~/.claude/.credentials.json`.
fn claude_code_credentials() -> Result<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?).join(".claude"),
    };
    Ok(dir.join(".credentials.json"))
}

/// Every signed-in login's models: Claude's, then ChatGPT's. A login whose listing fails is
/// reported and left out.
async fn listed_models(accounts: &Accounts) -> Vec<models::Model> {
    let endpoints = endpoints();
    let mut listed = Vec::new();
    if accounts.claude.signed_in().await {
        match models::list_claude(&accounts.claude, &endpoints.claude_api).await {
            Ok(models) => listed.extend(models),
            Err(error) => eprintln!("listing Claude models failed: {error:#}"),
        }
    }
    if accounts.openai.signed_in().await {
        match models::list_codex(&accounts.openai, &endpoints.codex, APP.originator).await {
            Ok(models) => listed.extend(models),
            Err(error) => eprintln!("listing ChatGPT models failed: {error:#}"),
        }
    }
    listed
}

fn runtime(state: &Path) -> Result<Runtime> {
    Ok(Runtime {
        http: Http::new(Limits::default())?,
        cache: state.join("downloads"),
        validate_within: VALIDATE_WITHIN,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { spec, fips } => {
            let counties = Counties::embedded()?;
            let county = counties.get(&fips).ok_or_else(|| anyhow!("no county {fips}"))?;
            let spec: Spec = serde_json::from_slice(&std::fs::read(&spec)?).context("reading the spec")?;
            let report = validate::validate(&runtime(&cli.state)?, &spec, county).await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.passed {
                std::process::exit(1);
            }
        }
        Command::Login { claude: Some(path), .. } => {
            let path = match path {
                Some(path) => path,
                None => claude_code_credentials()?,
            };
            let accounts = accounts(&cli.state)?;
            let snapshot = accounts.claude.bind(&path).await?;
            println!(
                "Bound the Claude login of {} at {}. Runs read it from there and never refresh it.",
                snapshot.email.as_deref().unwrap_or("(unknown)"),
                path.display()
            );
        }
        Command::Login { device, claude: None } => {
            let Accounts { openai: account, mut events, .. } = accounts(&cli.state)?;
            let start = account.login(if device { Method::DeviceCode } else { Method::Browser }).await?;
            println!("Open {}", start.url);
            if let Some(code) = start.user_code {
                println!("and enter the code {code}");
            }
            while let Ok(event) = events.recv().await {
                if let Event::LoginCompleted { ok, error, .. } = event {
                    if ok {
                        println!("Signed in.");
                        return Ok(());
                    }
                    bail!("signing in failed: {}", error.unwrap_or_default());
                }
            }
        }
        Command::ImportLogin { file } => {
            let credential: openai::Credential =
                serde_json::from_slice(&std::fs::read(&file)?).context("the file is not a ChatGPT credential")?;
            let path = accounts_dir(&cli.state)?.join("chatgpt.json");
            let text = serde_json::to_vec_pretty(&serde_json::json!({"version": 1, "credential": credential}))?;
            eriscountygis::registry::write_atomic(&path, &text)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            println!("Imported the login of {}.", credential.email.as_deref().unwrap_or("(unknown)"));
        }
        Command::Models => {
            let accounts = accounts(&cli.state)?;
            let listed = listed_models(&accounts).await;
            if listed.is_empty() {
                bail!("no models: sign in first (eris-county-gis login, or login --claude)");
            }
            for m in &listed {
                let efforts = match &m.cannot_run {
                    Some(reason) => format!("cannot run: {reason}"),
                    None => format!("efforts {}", m.efforts.join(",")),
                };
                println!("{:10} {:28} context {:>8} {efforts}", m.provider, m.id, m.context_window.unwrap_or(0));
            }
            println!("\ndefault ladder:");
            for (i, rung) in models::default_ladder(&listed).iter().enumerate() {
                println!("  {i}: {}", rung.label());
            }
        }
        Command::Status => status(&cli.registry)?,
        Command::RegenerateCounties { out } => {
            let count = counties::regenerate(&out).await?;
            println!("wrote {count} counties to {}", out.display());
        }
        Command::Run {
            states,
            counties: fips,
            concurrency,
            ladder,
            attempts_per_rung,
            validate_budget,
            no_statewide,
            recheck,
            retry_failed,
            no_tui,
        } => {
            let all = Arc::new(Counties::embedded()?);
            let selected = select(&all, &states, &fips)?;
            // Naming counties alone skips the statewide search unless states are named too.
            let statewide = !no_statewide && (fips.is_empty() || !states.is_empty());
            let accounts = accounts(&cli.state)?;
            let listed = listed_models(&accounts).await;
            let ladder = match &ladder {
                Some(text) => models::parse_ladder(text, &listed)?,
                None => models::default_ladder(&listed),
            };
            if ladder.is_empty() {
                bail!("no model ladder: sign in first (eris-county-gis login, or login --claude), or pass --ladder");
            }
            let providers = models::providers(&accounts.openai, &accounts.claude, &endpoints(), APP.originator).await?;
            let assistant = Assistant::from_models(&listed, &providers, &accounts.claude, &endpoints().claude_api);
            let work = Arc::new(Work {
                counties: all.clone(),
                registry: Mutex::new(Registry::open(&cli.registry)?),
                runtime: runtime(&cli.state)?,
                assistant,
                agents: Mutex::new(Default::default()),
                validate_budget,
                log: Mutex::new(Default::default()),
                logged: Default::default(),
            });
            let harness_dir = cli.state.join("harness");
            let empty = harness_dir.join("empty");
            std::fs::create_dir_all(&empty)?;
            let mut builder = Harness::builder(&harness_dir);
            for (name, provider) in providers {
                builder = builder.provider(name, provider);
            }
            let harness = builder
                .tools(tools::all(&work))
                .max_concurrent_turns(256)
                .open()
                .await
                .context("opening the agents' state")?;
            let (journal, past) = Journal::open(&cli.state.join("journal.jsonl"))?;
            let settings = Settings { concurrency, ladder, attempts_per_rung, statewide, retry_failed };
            // Jobs start while every login the ladder runs on gives a token.
            let needs_codex = settings.ladder.iter().any(|r| r.provider == CODEX);
            let needs_claude = settings.ladder.iter().any(|r| r.provider == ANTHROPIC);
            let (openai, claude) = (accounts.openai.clone(), accounts.claude.clone());
            let ready: Box<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync> =
                Box::new(move || {
                    let (openai, claude) = (openai.clone(), claude.clone());
                    Box::pin(async move {
                        (!needs_codex || openai.current().await.is_ok())
                            && (!needs_claude || claude.current().await.is_ok())
                    })
                });
            let scheduler = Scheduler::new(work, harness, journal, past, settings, selected, empty, ready);
            if recheck {
                scheduler.recheck().await;
            }
            if no_tui {
                headless(scheduler).await?;
            } else {
                tui::run(scheduler, accounts.openai, accounts.claude, accounts.events).await?;
            }
        }
    }
    Ok(())
}

fn status(registry: &Path) -> Result<()> {
    let counties = Counties::embedded()?;
    let registry = Registry::open(registry)?;
    let mut by_state: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let at = now();
    for county in &counties.list {
        let entry = by_state.entry(county.state.clone()).or_default();
        entry.0 += 1;
        match registry.standing(&county.fips, at) {
            Standing::Covered(_) => entry.1 += 1,
            Standing::Unavailable(_) => entry.2 += 1,
            Standing::Open => {}
        }
    }
    let (mut total, mut covered, mut unavailable) = (0, 0, 0);
    println!("state  counties  covered  unavailable");
    for (state, (n, c, u)) in &by_state {
        println!("{state:5}  {n:8}  {c:7}  {u:11}");
        total += n;
        covered += c;
        unavailable += u;
    }
    println!("all    {total:8}  {covered:7}  {unavailable:11}   ({} adapters)", registry.adapters.len());
    Ok(())
}

/// The selected counties: those named, every county of the states named, or every county.
fn select(all: &Counties, states: &[String], fips: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for state in states {
        let state = state.to_uppercase();
        let before = out.len();
        out.extend(all.in_state(&state).map(|c| c.fips.clone()));
        if out.len() == before {
            bail!("no counties in {state}");
        }
    }
    for f in fips {
        if all.get(f).is_none() {
            bail!("no county {f}");
        }
        out.push(f.clone());
    }
    if states.is_empty() && fips.is_empty() {
        out = all.list.iter().map(|c| c.fips.clone()).collect();
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Prints the log as it grows and a progress line every 15 seconds, until the run settles or
/// Ctrl-C stops it.
async fn headless(scheduler: Arc<Scheduler>) -> Result<()> {
    let printed = Arc::new(Mutex::new(0u64));
    let printer = {
        let (scheduler, printed) = (scheduler.clone(), printed.clone());
        tokio::spawn(async move {
            let mut tick = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                print_new(&scheduler, &mut printed.lock().unwrap());
                tick += 1;
                if tick.is_multiple_of(15) {
                    println!("· {}", View::of(&scheduler).progress_line());
                }
            }
        })
    };
    let runner = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move { scheduler.run().await })
    };
    tokio::select! {
        result = runner => { result??; }
        _ = tokio::signal::ctrl_c() => { println!("stopping; agents resume on the next run"); }
    }
    printer.abort();
    print_new(&scheduler, &mut printed.lock().unwrap());
    println!("{}", View::of(&scheduler).progress_line());
    scheduler.stop().await;
    Ok(())
}

fn print_new(scheduler: &Scheduler, printed: &mut u64) {
    let log = scheduler.work.log.lock().unwrap();
    let logged = scheduler.work.logged.load(Ordering::SeqCst);
    let new = (logged - *printed).min(log.len() as u64) as usize;
    for line in log.iter().skip(log.len() - new) {
        println!("{line}");
    }
    *printed = logged;
}
