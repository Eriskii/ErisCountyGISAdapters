//! Runs the jobs. Each selected state first gets one agent looking for a statewide layer; then
//! each county still open gets agents of its own, after a check (no model involved) of every
//! adapter already claiming it. A county climbs the model ladder: it starts on the cheapest
//! rung, and an attempt that gives up or fails hands a summary of what it tried to the next.
//!
//! Every attempt is journaled as it starts and ends, and the harness keeps its agents, so a
//! killed run picks its agents up again and goes on.

use crate::journal::{Event, Job, JobHistory, Journal, Outcome, Tokens, replay};
use crate::models::Rung;
use crate::prompts;
use crate::tools;
use crate::work::{AgentJob, Resolution, Work};
use anyhow::Result;
use erisgis::registry::{Standing, now};
use erisgis::spec::Spec;
use erisgis::validate;
use erisharness::Harness;
use erisharness::agent::{AgentSpec, AgentState, Item, Observation};
use erisharness::machine::{DirectSpec, MachineSpec};
use futures_util::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Settings {
    /// Jobs running at once.
    pub concurrency: usize,
    pub ladder: Vec<Rung>,
    /// Attempts on each rung before climbing.
    pub attempts_per_rung: usize,
    /// Whether states are searched for statewide layers before their counties.
    pub statewide: bool,
    /// Whether counties that used up the ladder are tried again.
    pub retry_failed: bool,
}

/// Attempts and tokens on one rung of the ladder.
#[derive(Clone, Debug, Default)]
pub struct RungStats {
    pub attempts: usize,
    pub successes: usize,
    pub tokens: Tokens,
}

/// Where a selected county stands, for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Covered,
    Unavailable,
    Running,
    /// Every rung was tried.
    Failed,
    Pending,
}

/// Claim checks that run at once, beside the agents' slots; each host still takes at most the
/// HTTP client's few requests at a time.
const CHECK_SLOTS: usize = 12;

pub struct Scheduler {
    pub work: Arc<Work>,
    harness: Arc<Harness>,
    journal: Journal,
    pub jobs: Mutex<BTreeMap<Job, JobHistory>>,
    pub settings: Mutex<Settings>,
    pub paused: AtomicBool,
    pub stopping: AtomicBool,
    /// The selected counties, by FIPS, and their states.
    pub counties: Vec<String>,
    pub states: Vec<String>,
    running: Mutex<HashSet<Job>>,
    pub rungs: Mutex<BTreeMap<usize, RungStats>>,
    /// Counties whose adapter stopped passing on a recheck: the spec and why.
    dead: Mutex<HashMap<String, (Spec, String)>>,
    /// Claims checked that failed, by adapter and county, kept in the journal so no run checks
    /// them again.
    refused: Mutex<HashSet<(String, String)>>,
    /// Counties whose claims are being checked now.
    checking: Mutex<HashSet<String>>,
    wake: Notify,
    /// Where agents' commands would run; they run none.
    empty: PathBuf,
    /// Asked before each launch; agents start only while it says the account can make calls.
    pub ready: Box<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        work: Arc<Work>,
        harness: Arc<Harness>,
        journal: Journal,
        events: Vec<Event>,
        settings: Settings,
        counties: Vec<String>,
        empty: PathBuf,
        ready: Box<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>,
    ) -> Arc<Self> {
        let mut states: Vec<String> =
            counties.iter().filter_map(|f| work.counties.get(f).map(|c| c.state.clone())).collect();
        states.sort();
        states.dedup();
        let mut rungs: BTreeMap<usize, RungStats> = BTreeMap::new();
        for event in &events {
            match event {
                Event::Started { rung, .. } => rungs.entry(*rung).or_default().attempts += 1,
                Event::Finished { rung, outcome, tokens, .. } => {
                    let stats = rungs.entry(*rung).or_default();
                    stats.tokens.add(tokens);
                    if matches!(outcome, Outcome::Done { .. } | Outcome::Unavailable { .. }) {
                        stats.successes += 1;
                    }
                }
                Event::Checked { .. } => {}
            }
        }
        let refused = events
            .iter()
            .filter_map(|e| match e {
                Event::Checked { adapter, fips, passed: false, .. } => Some((adapter.clone(), fips.clone())),
                _ => None,
            })
            .collect();
        Arc::new(Self {
            jobs: Mutex::new(replay(&events)),
            work,
            harness,
            journal,
            settings: Mutex::new(settings),
            paused: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            counties,
            states,
            running: Mutex::new(HashSet::new()),
            rungs: Mutex::new(rungs),
            dead: Mutex::new(HashMap::new()),
            refused: Mutex::new(refused),
            checking: Mutex::new(HashSet::new()),
            wake: Notify::new(),
            empty,
            ready,
        })
    }

    pub fn poke(&self) {
        self.wake.notify_one();
    }

    pub fn running_jobs(&self) -> usize {
        self.running.lock().unwrap().len()
    }

    /// Counties whose claims are being checked now; these hold no agent slot.
    pub fn checks_running(&self) -> usize {
        self.checking.lock().unwrap().len()
    }

    /// Whether an adapter claims the open county without having been checked there yet.
    fn claim_pending(&self, fips: &str) -> bool {
        let Some(county) = self.work.counties.get(fips) else { return false };
        let claims = self.work.registry.lock().unwrap().unverified_claims(fips, &county.state);
        let refused = self.refused.lock().unwrap();
        claims.iter().any(|spec| !refused.contains(&(spec.id.clone(), fips.to_owned())))
    }

    /// Selected open counties with a claim to check, whose state has settled, not being checked.
    fn checks_due(&self, statewide: bool) -> Vec<String> {
        let checking = self.checking.lock().unwrap().clone();
        self.counties
            .iter()
            .filter(|f| !checking.contains(*f) && self.open(f))
            .filter(|f| {
                !statewide
                    || self.state_settled(&self.work.counties.get(f).map(|c| c.state.clone()).unwrap_or_default())
            })
            .filter(|f| self.claim_pending(f))
            .cloned()
            .collect()
    }

    /// Starts checks of claims, up to [`CHECK_SLOTS`] at once, beside the agents' slots.
    fn start_checks(self: &Arc<Self>, statewide: bool) {
        let free = CHECK_SLOTS.saturating_sub(self.checks_running());
        for fips in self.checks_due(statewide).into_iter().take(free) {
            self.checking.lock().unwrap().insert(fips.clone());
            let this = self.clone();
            tokio::spawn(async move {
                this.verify_claims(&fips).await;
                this.checking.lock().unwrap().remove(&fips);
                this.poke();
            });
        }
    }

    /// The rung the job's next attempt runs on, or none once the ladder is used up.
    fn next_rung(&self, history: Option<&JobHistory>) -> Option<usize> {
        let settings = self.settings.lock().unwrap();
        let Some(history) = history.filter(|h| !h.attempts.is_empty()) else {
            return (!settings.ladder.is_empty()).then_some(0);
        };
        let rung = history.rung;
        if history.attempts.get(&rung).copied().unwrap_or(0) < settings.attempts_per_rung {
            Some(rung)
        } else if rung + 1 < settings.ladder.len() {
            Some(rung + 1)
        } else {
            None
        }
    }

    fn exhausted(&self, job: &Job) -> bool {
        let history = self.jobs.lock().unwrap().get(job).cloned();
        let ended_badly = matches!(
            history.as_ref().and_then(|h| h.finished.as_ref()),
            Some(Outcome::GaveUp { .. } | Outcome::Failed { .. })
        );
        ended_badly
            && history.as_ref().is_some_and(|h| h.running.is_none())
            && self.next_rung(history.as_ref()).is_none()
    }

    fn state_settled(&self, state: &str) -> bool {
        let job = Job::State(state.to_owned());
        if self.running.lock().unwrap().contains(&job) {
            return false;
        }
        let finished = self.jobs.lock().unwrap().get(&job).and_then(|h| h.finished.clone());
        matches!(finished, Some(Outcome::Done { .. })) || self.exhausted(&job)
    }

    pub fn status(&self, fips: &str) -> Status {
        if self.running.lock().unwrap().contains(&Job::County(fips.to_owned())) {
            return Status::Running;
        }
        match self.work.registry.lock().unwrap().standing(fips, now()) {
            Standing::Covered(_) => return Status::Covered,
            Standing::Unavailable(_) => return Status::Unavailable,
            Standing::Open => {}
        }
        if self.exhausted(&Job::County(fips.to_owned())) { Status::Failed } else { Status::Pending }
    }

    fn open(&self, fips: &str) -> bool {
        matches!(self.work.registry.lock().unwrap().standing(fips, now()), Standing::Open)
    }

    /// The next job ready to start: states first, then counties whose state has settled.
    fn next_job(&self) -> Option<Job> {
        let settings = self.settings.lock().unwrap().clone();
        let running = self.running.lock().unwrap().clone();
        if settings.statewide {
            for state in &self.states {
                let job = Job::State(state.clone());
                if running.contains(&job) || self.state_settled(state) {
                    continue;
                }
                let any_open = self
                    .counties
                    .iter()
                    .any(|f| self.work.counties.get(f).is_some_and(|c| &c.state == state) && self.open(f));
                if any_open {
                    return Some(job);
                }
            }
        }
        for fips in &self.counties {
            let job = Job::County(fips.clone());
            if running.contains(&job) || !self.open(fips) || self.checking.lock().unwrap().contains(fips) {
                continue;
            }
            // A county an adapter claims is checked first, outside the agents' slots.
            if self.claim_pending(fips) {
                continue;
            }
            if settings.statewide {
                let state = self.work.counties.get(fips).map(|c| c.state.clone()).unwrap_or_default();
                if !self.state_settled(&state) {
                    continue;
                }
            }
            // `--retry-failed` resets used-up counties once, as the run starts.
            if self.exhausted(&job) {
                continue;
            }
            return Some(job);
        }
        None
    }

    /// Runs until every selected job is settled, or until told to stop.
    pub async fn run(self: &Arc<Self>) -> Result<()> {
        if self.settings.lock().unwrap().retry_failed {
            // Retrying starts used-up counties from the bottom of the ladder again.
            for history in self.jobs.lock().unwrap().values_mut() {
                if matches!(history.finished, Some(Outcome::GaveUp { .. } | Outcome::Failed { .. })) {
                    history.attempts.clear();
                    history.rung = 0;
                }
            }
        }
        self.resume();
        tokio::spawn(self.clone().observe());
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if !self.paused.load(Ordering::SeqCst) {
                let statewide = self.settings.lock().unwrap().statewide;
                self.start_checks(statewide);
                let concurrency = self.settings.lock().unwrap().concurrency;
                while self.running_jobs() < concurrency {
                    let Some(job) = self.next_job() else { break };
                    if !(self.ready)().await {
                        self.paused.store(true, Ordering::SeqCst);
                        self.work.log(
                            "paused: the ladder's logins are not signed in (`eris-gis-scraper login`, or `login --claude`)",
                        );
                        break;
                    }
                    self.running.lock().unwrap().insert(job.clone());
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(error) = this.clone().run_job(&job).await {
                            this.work.log(format!("{}: {error:#}", job.label()));
                        }
                        this.running.lock().unwrap().remove(&job);
                        this.poke();
                    });
                }
            }
            if self.running_jobs() == 0
                && self.checks_running() == 0
                && !self.paused.load(Ordering::SeqCst)
                && self.next_job().is_none()
                && self.checks_due(self.settings.lock().unwrap().statewide).is_empty()
            {
                self.work.log("every selected job is settled");
                break;
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), self.wake.notified()).await;
        }
        Ok(())
    }

    /// Stops starting jobs and stops the agents' turns where they stand; the next run resumes
    /// them.
    pub async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.poke();
        self.harness.shutdown().await;
    }

    /// Picks up the agents a killed run left: each goes on in its own job.
    fn resume(self: &Arc<Self>) {
        let running: Vec<(Job, String, usize, String)> = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(job, h)| h.running.clone().map(|(agent, rung, model)| (job.clone(), agent, rung, model)))
            .collect();
        for (job, agent, rung, model) in running {
            if self.harness.agent(&agent).is_err() {
                let outcome = Outcome::Failed { error: "the agent was lost".into() };
                let _ = self.finish(&job, rung, &agent, outcome, Tokens::default());
                continue;
            }
            self.work.log(format!("{}: resuming agent {agent}", job.label()));
            self.work.agents.lock().unwrap().insert(agent.clone(), AgentJob::new(job.clone(), rung, model));
            self.running.lock().unwrap().insert(job.clone());
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(error) = this.clone().watch(&job, &agent, rung).await {
                    this.work.log(format!("{}: {error:#}", job.label()));
                }
                this.running.lock().unwrap().remove(&job);
                this.poke();
            });
        }
    }

    /// Keeps each live agent's token count current from the harness's events.
    async fn observe(self: Arc<Self>) {
        let mut observations = self.harness.subscribe();
        loop {
            match observations.recv().await {
                Ok(Observation::Usage { agent, usage }) => {
                    self.work.with_agent(&agent, |a| {
                        a.tokens.add(&Tokens {
                            input: usage.input,
                            cached_input: usage.cached_input,
                            output: usage.output,
                        })
                    });
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    }

    async fn run_job(self: Arc<Self>, job: &Job) -> Result<()> {
        match job {
            Job::County(_) | Job::State(_) => self.attempt(job).await,
        }
    }

    /// Checks every adapter claiming the county that is not verified for it; true once one
    /// passes.
    pub async fn verify_claims(&self, fips: &str) -> bool {
        let Some(county) = self.work.counties.get(fips) else { return false };
        let claims = self.work.registry.lock().unwrap().unverified_claims(fips, &county.state);
        for spec in claims {
            let key = (spec.id.clone(), fips.to_owned());
            if self.refused.lock().unwrap().contains(&key) {
                continue;
            }
            let report = validate::validate(&self.work.runtime, self.work.dataset, &spec, county).await;
            let _ = self.journal.append(&Event::Checked {
                at: now(),
                adapter: spec.id.clone(),
                fips: fips.to_owned(),
                passed: report.passed,
            });
            if report.passed {
                let _ = self.work.registry.lock().unwrap().verified(&spec.id, fips, now());
                self.work.log(format!("county {fips}: covered by {}", spec.id));
                return true;
            }
            self.refused.lock().unwrap().insert(key);
        }
        false
    }

    /// One attempt at a job on its next rung, start to end.
    async fn attempt(self: &Arc<Self>, job: &Job) -> Result<()> {
        let history = self.jobs.lock().unwrap().get(job).cloned().unwrap_or_default();
        let Some(rung) = self.next_rung(Some(&history)) else { return Ok(()) };
        let settings = self.settings.lock().unwrap().clone();
        let step = settings.ladder[rung].clone();
        let (system, briefing) = match job {
            Job::County(fips) => {
                let county = self.work.counties.get(fips).ok_or_else(|| anyhow::anyhow!("no county {fips}"))?;
                let dead = self.dead.lock().unwrap().get(fips).cloned();
                let briefing = {
                    let registry = self.work.registry.lock().unwrap();
                    prompts::county_briefing(
                        self.work.dataset,
                        county,
                        &self.work.counties,
                        &registry,
                        &history.summaries,
                        dead.as_ref().map(|(s, r)| (s, r.as_str())),
                    )
                };
                (prompts::system_prompt(self.work.dataset, false), briefing)
            }
            Job::State(state) => {
                let count = self.work.counties.in_state(state).count();
                let name = self.work.counties.in_state(state).next().map(|c| c.state_name.clone()).unwrap_or_default();
                (
                    prompts::system_prompt(self.work.dataset, true),
                    prompts::state_briefing(self.work.dataset, state, &name, count, &history.summaries),
                )
            }
        };
        let tools: Vec<String> = match job {
            Job::County(_) => tools::COUNTY_TOOLS.iter().map(|s| s.to_string()).collect(),
            Job::State(_) => tools::STATE_TOOLS.iter().map(|s| s.to_string()).collect(),
        };
        let agent = self.harness.create_agent(AgentSpec {
            system_prompt: system,
            tools,
            provider: step.provider.clone(),
            model: step.model.clone(),
            reasoning_effort: step.effort.clone(),
            context_window: step.context_window,
            metadata: serde_json::to_value(job)?,
            machine: MachineSpec::Direct(DirectSpec {
                cwd: self.empty.to_string_lossy().into(),
                env: Some(Vec::new()),
            }),
        })?;
        self.work.agents.lock().unwrap().insert(agent.clone(), AgentJob::new(job.clone(), rung, step.model.clone()));
        self.journal.append(&Event::Started {
            at: now(),
            job: job.clone(),
            rung,
            model: step.model.clone(),
            agent: agent.clone(),
        })?;
        {
            let mut jobs = self.jobs.lock().unwrap();
            let history = jobs.entry(job.clone()).or_default();
            *history.attempts.entry(rung).or_default() += 1;
            history.running = Some((agent.clone(), rung, step.model.clone()));
            history.finished = None;
            history.rung = history.rung.max(rung);
        }
        self.rungs.lock().unwrap().entry(rung).or_default().attempts += 1;
        self.work.log(format!("{}: started on {}", job.label(), step.label()));
        self.harness.send(&agent, "user", &briefing)?;
        self.watch(job, &agent, rung).await
    }

    /// Waits for the agent to settle and records how its attempt ended.
    async fn watch(self: &Arc<Self>, job: &Job, agent: &str, rung: usize) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.harness.wait_for(agent, AgentState::Idle) => {}
                _ = self.harness.wait_for(agent, AgentState::Failed) => {}
            }
            if self.stopping.load(Ordering::SeqCst) {
                // Stopped with the run: the agent resumes next time.
                return Ok(());
            }
            let record = self.harness.agent(agent)?;
            let resolution = self.work.with_agent(agent, |a| a.resolution.clone()).flatten().or_else(|| match job {
                // A resumed agent may have saved its result before the kill.
                Job::County(fips) => match self.work.registry.lock().unwrap().standing(fips, now()) {
                    Standing::Covered(ids) => Some(Resolution::Covered(ids[0].clone())),
                    Standing::Unavailable(u) => Some(Resolution::Unavailable(u.reason)),
                    Standing::Open => None,
                },
                Job::State(_) => None,
            });
            // An agent that saved one layer of a dataset taken per jurisdiction is asked once
            // about the jurisdictions it may have left out.
            if matches!(resolution, Some(Resolution::Covered(_)))
                && self.work.dataset.per_jurisdiction
                && record.state != AgentState::Failed
            {
                let ask = self
                    .work
                    .with_agent(agent, |a| {
                        let ask = a.saved == 1 && !a.followed_up;
                        a.followed_up = true;
                        ask
                    })
                    .unwrap_or(false);
                if ask {
                    self.harness.send(agent, "user", self.work.dataset.agents.follow_up)?;
                    continue;
                }
            }
            let last = last_text(&self.harness, agent);
            let outcome = match (resolution, record.state, job) {
                (Some(Resolution::Covered(id)), _, _) => Outcome::Done { detail: id },
                (Some(Resolution::Unavailable(reason)), _, _) => Outcome::Unavailable { reason: reason.name().into() },
                (None, AgentState::Failed, _) => Outcome::Failed { error: record.error.unwrap_or_default() },
                (None, _, Job::State(_)) => {
                    Outcome::Done { detail: last.unwrap_or_else(|| "no statewide layer".into()) }
                }
                (None, _, Job::County(_)) => {
                    let budget = self.work.validate_budget;
                    let nudge = self
                        .work
                        .with_agent(agent, |a| {
                            let nudge = !a.nudged && a.failures < budget;
                            a.nudged = true;
                            nudge
                        })
                        .unwrap_or(false);
                    if nudge {
                        self.harness.send(agent, "user", prompts::NUDGE)?;
                        continue;
                    }
                    let trail = self
                        .work
                        .with_agent(agent, |a| a.trail.iter().cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    let mut summary = last.unwrap_or_default();
                    if !trail.is_empty() {
                        summary.push_str("\nIts last validations: ");
                        summary.push_str(&trail.join(" | "));
                    }
                    Outcome::GaveUp { summary: shorten(&summary, 2500) }
                }
            };
            let tokens = Tokens {
                input: record.usage.input,
                cached_input: record.usage.cached_input,
                output: record.usage.output,
            };
            return self.finish(job, rung, agent, outcome, tokens);
        }
    }

    fn finish(&self, job: &Job, rung: usize, agent: &str, outcome: Outcome, mut tokens: Tokens) -> Result<()> {
        let tools = self.work.with_agent(agent, |a| a.tool_tokens.clone()).unwrap_or_default();
        for spent in tools.values() {
            tokens.add(spent);
        }
        self.journal.append(&Event::Finished {
            at: now(),
            job: job.clone(),
            rung,
            agent: agent.to_owned(),
            outcome: outcome.clone(),
            tokens: tokens.clone(),
            tools,
        })?;
        {
            let mut rungs = self.rungs.lock().unwrap();
            let stats = rungs.entry(rung).or_default();
            stats.tokens.add(&tokens);
            if matches!(outcome, Outcome::Done { .. } | Outcome::Unavailable { .. }) {
                stats.successes += 1;
            }
        }
        let line = match &outcome {
            Outcome::Done { detail } => format!("done: {}", shorten(detail, 160)),
            Outcome::Unavailable { reason } => format!("unavailable: {reason}"),
            Outcome::GaveUp { .. } => "gave up".into(),
            Outcome::Failed { error } => format!("failed: {}", shorten(error, 160)),
        };
        self.work.log(format!("{} (rung {rung}): {line}", job.label()));
        {
            let mut jobs = self.jobs.lock().unwrap();
            let history = jobs.entry(job.clone()).or_default();
            history.running = None;
            history.tokens.add(&tokens);
            match &outcome {
                Outcome::GaveUp { summary } => history.summaries.push(summary.clone()),
                Outcome::Failed { error } => history.summaries.push(format!("The attempt failed: {error}")),
                Outcome::Done { .. } | Outcome::Unavailable { .. } => {
                    history.attempts.clear();
                    history.summaries.clear();
                    history.rung = 0;
                }
            }
            history.finished = Some(outcome);
        }
        self.work.agents.lock().unwrap().remove(agent);
        Ok(())
    }

    /// Validates every saved adapter for each selected county it is verified for; one that
    /// fails loses the county, which is searched again with the dead spec as its start.
    pub async fn recheck(&self) {
        let pairs: Vec<(Spec, String)> = {
            let registry = self.work.registry.lock().unwrap();
            let selected: HashSet<&String> = self.counties.iter().collect();
            registry
                .index
                .iter()
                .filter(|(fips, _)| selected.contains(fips))
                .flat_map(|(fips, ids)| {
                    ids.iter().filter_map(|id| registry.adapters.get(id).map(|s| (s.clone(), fips.clone())))
                })
                .collect()
        };
        self.work.log(format!("rechecking {} adapter-county pairs", pairs.len()));
        let concurrency = self.settings.lock().unwrap().concurrency.max(4);
        futures_util::stream::iter(pairs)
            .for_each_concurrent(concurrency, |(spec, fips)| async move {
                let Some(county) = self.work.counties.get(&fips) else { return };
                let report = validate::validate(&self.work.runtime, self.work.dataset, &spec, county).await;
                if report.passed {
                    let _ = self.work.registry.lock().unwrap().verified(&spec.id, &fips, now());
                } else {
                    self.work.log(report.summary());
                    let _ = self.work.registry.lock().unwrap().unverified(&spec.id, &fips);
                    self.dead.lock().unwrap().insert(fips.clone(), (spec, report.summary()));
                }
            })
            .await;
    }
}

/// The agent's last words.
fn last_text(harness: &Harness, agent: &str) -> Option<String> {
    harness.transcript(agent).ok()?.into_iter().rev().find_map(|entry| match entry.item {
        Item::Assistant { text } if !text.trim().is_empty() => Some(text),
        _ => None,
    })
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text.to_owned()
    }
}
