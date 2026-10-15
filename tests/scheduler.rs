//! Whole runs against a stand-in model and a fake county server: agents validate and submit
//! specs, give up and hand over to the next rung, cover a state with one layer, and resume
//! after the run is stopped mid-turn.

mod support;

use eriscountygis::counties::Counties;
use eriscountygis::http::{Http, Limits};
use eriscountygis::journal::{Event, Journal, Outcome};
use eriscountygis::models::Rung;
use eriscountygis::registry::Registry;
use eriscountygis::runtime::Runtime;
use eriscountygis::scheduler::{PROVIDER, Scheduler, Settings};
use eriscountygis::tools;
use eriscountygis::work::Work;
use erisharness::Harness;
use erisharness::provider::{RateGate, Responses, ResponsesConfig, StaticToken};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::model::{Brain, Model, Reply, Request};

fn ladder() -> Vec<Rung> {
    vec![
        Rung { model: "mini".into(), effort: None, context_window: None },
        Rung { model: "big".into(), effort: Some("high".into()), context_window: None },
    ]
}

async fn scheduler(dir: &Path, model: &Model, counties: Vec<String>, statewide: bool) -> Arc<Scheduler> {
    let all = Arc::new(Counties::embedded().unwrap());
    let work = Arc::new(Work {
        counties: all,
        registry: Mutex::new(Registry::open(&dir.join("registry")).unwrap()),
        runtime: Runtime {
            http: Http::new(Limits { allow_private: true, ..Limits::default() }).unwrap(),
            cache: dir.join("cache"),
        },
        agents: Mutex::default(),
        validate_budget: 3,
        log: Mutex::default(),
        logged: Default::default(),
    });
    let provider = Arc::new(Responses::new(ResponsesConfig {
        base_url: model.url.clone(),
        headers: vec![],
        store: false,
        max_retries: 0,
        credentials: Arc::new(StaticToken("token".into())),
        gate: RateGate::new(16, 64),
    }));
    let empty = dir.join("harness/empty");
    std::fs::create_dir_all(&empty).unwrap();
    let harness = Harness::builder(dir.join("harness"))
        .provider(PROVIDER, provider)
        .tools(tools::all(&work))
        .open()
        .await
        .unwrap();
    let (journal, past) = Journal::open(&dir.join("journal.jsonl")).unwrap();
    let settings = Settings { concurrency: 4, ladder: ladder(), attempts_per_rung: 1, statewide, retry_failed: false };
    Scheduler::new(work, harness, journal, past, settings, counties, empty, Box::new(|| Box::pin(async { true })))
}

fn good_spec(root: &str, coverage: Value) -> Value {
    json!({
        "id": "fake-parcels", "publisher": "Fake GIS",
        "source": {"kind": "arcgis-feature", "url": format!("{root}/arcgis/good/FeatureServer/0")},
        "coverage": coverage,
        "fields": {"apn": "PARCELNO", "owner": "OWNER", "address": "SITEADDR", "acres": "GISACRES"},
    })
}

/// Validates the spec, submits it once it passes, then says it is done.
fn diligent(request: &Request, spec: &Value) -> Reply {
    let last = request.last_output();
    if last.contains("Saved ") {
        Reply::Says("Saved the adapter.".into())
    } else if last.contains("\"passed\": true") {
        Reply::Calls("submit_spec".into(), json!({"spec": spec}))
    } else {
        Reply::Calls("validate".into(), json!({"spec": spec}))
    }
}

fn events(dir: &Path) -> Vec<Event> {
    Journal::open(&dir.join("journal.jsonl")).unwrap().1
}

fn interiors(fips: &[&str]) -> Vec<(f64, f64)> {
    let all = Counties::embedded().unwrap();
    fips.iter().map(|f| all.get(f).unwrap()).map(|c| (c.interior[0], c.interior[1])).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_county_that_the_small_model_gives_up_on_is_covered_on_the_next_rung() {
    let dir = tempfile::tempdir().unwrap();
    let root = support::serve(support::Grid::around_all(&interiors(&["55025"]))).await;
    let spec = good_spec(&root, json!({"counties": ["55025"]}));
    let brain: Brain = Arc::new(move |request: &Request| match request.model.as_str() {
        "mini" => Reply::Says("I searched the Dane County site and found only a viewer.".into()),
        _ => diligent(request, &spec),
    });
    let model = Model::start(brain).await;
    let run = scheduler(dir.path(), &model, vec!["55025".into()], false).await;
    tokio::time::timeout(Duration::from_secs(60), run.run()).await.unwrap().unwrap();

    let registry = Registry::open(&dir.path().join("registry")).unwrap();
    let saved = &registry.adapters["fake-parcels"];
    assert_eq!(saved.verification.rung, Some(1));
    assert_eq!(saved.verification.model.as_deref(), Some("big"));
    assert_eq!(registry.index["55025"], ["fake-parcels"]);

    let outcomes: Vec<(usize, &str)> = events(dir.path())
        .iter()
        .filter_map(|e| match e {
            Event::Finished { rung, outcome, .. } => Some((
                *rung,
                match outcome {
                    Outcome::Done { .. } => "done",
                    Outcome::GaveUp { .. } => "gave-up",
                    _ => "other",
                },
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    assert_eq!(outcomes, [(0, "gave-up"), (1, "done")]);

    let requests = model.requests.lock().unwrap();
    let mini: Vec<&Request> = requests.iter().filter(|r| r.model == "mini").collect();
    assert_eq!(mini.len(), 2, "the small model is nudged once, then gives up");
    assert!(mini[1].user.last().unwrap().contains("stopped without calling submit_spec"));
    let big = requests.iter().find(|r| r.model == "big").unwrap();
    assert!(big.briefing().contains("Dane County"));
    assert!(big.briefing().contains("found only a viewer"), "the next rung reads what the last one tried");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_statewide_layer_covers_the_state_without_county_agents() {
    let dir = tempfile::tempdir().unwrap();
    let selected = ["55001", "55025", "55071", "55141"];
    let root = support::serve(support::Grid::around_all(&interiors(&selected))).await;
    let spec = good_spec(&root, json!({"states": ["WI"]}));
    let brain: Brain = Arc::new(move |request: &Request| {
        assert!(
            request.briefing().contains("Your state: Wisconsin"),
            "only the state's agent runs: {}",
            request.briefing()
        );
        diligent(request, &spec)
    });
    let model = Model::start(brain).await;
    let run = scheduler(dir.path(), &model, selected.map(String::from).to_vec(), true).await;
    tokio::time::timeout(Duration::from_secs(60), run.run()).await.unwrap().unwrap();

    let registry = Registry::open(&dir.path().join("registry")).unwrap();
    for fips in selected {
        assert_eq!(registry.index.get(fips).map(Vec::as_slice), Some(&["fake-parcels".to_owned()][..]), "{fips}");
    }
    assert_eq!(registry.adapters["fake-parcels"].verification.counties, selected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_county_without_a_service_is_marked_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let brain: Brain = Arc::new(|request: &Request| {
        if request.outputs.is_empty() {
            Reply::Calls(
                "mark_unavailable".into(),
                json!({"reason": "paywalled", "detail": "Parcels sold at https://county.example/store"}),
            )
        } else {
            Reply::Says("Marked unavailable.".into())
        }
    });
    let model = Model::start(brain).await;
    let run = scheduler(dir.path(), &model, vec!["13121".into()], false).await;
    tokio::time::timeout(Duration::from_secs(60), run.run()).await.unwrap().unwrap();
    let registry = Registry::open(&dir.path().join("registry")).unwrap();
    let record = &registry.unavailable["13121"];
    assert_eq!(record.reason.name(), "paywalled");
    assert!(record.recheck_after > record.recorded_at + 300 * 86_400_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_run_resumes_its_agent_where_it_stood() {
    let dir = tempfile::tempdir().unwrap();
    let root = support::serve(support::Grid::around_all(&interiors(&["55025"]))).await;

    // First run: the model never answers; the run is stopped mid-turn.
    let hanging = Model::start(Arc::new(|_: &Request| Reply::Hangs)).await;
    let run = scheduler(dir.path(), &hanging, vec!["55025".into()], false).await;
    let task = tokio::spawn({
        let run = run.clone();
        async move { run.run().await }
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        while hanging.requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    run.stop().await;
    task.await.unwrap().unwrap();
    drop(run);
    let started: Vec<String> = events(dir.path())
        .iter()
        .filter_map(|e| if let Event::Started { agent, .. } = e { Some(agent.clone()) } else { None })
        .collect();
    assert_eq!(started.len(), 1);

    // Second run: the same agent goes on and finishes; no new attempt starts.
    let spec = good_spec(&root, json!({"counties": ["55025"]}));
    let working = Model::start(Arc::new(move |request: &Request| diligent(request, &spec))).await;
    let run = scheduler(dir.path(), &working, vec!["55025".into()], false).await;
    tokio::time::timeout(Duration::from_secs(60), run.run()).await.unwrap().unwrap();
    let all = events(dir.path());
    let agents: Vec<(&str, &str)> = all
        .iter()
        .map(|e| match e {
            Event::Started { agent, .. } => ("started", agent.as_str()),
            Event::Finished { agent, .. } => ("finished", agent.as_str()),
        })
        .collect();
    assert_eq!(agents, [("started", started[0].as_str()), ("finished", started[0].as_str())]);
    assert!(Registry::open(&dir.path().join("registry")).unwrap().index.contains_key("55025"));
}
