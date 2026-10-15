//! The agents' command-line tools, run as agents run them: help, parse errors, quoting,
//! --grep, and the spec workflow, against a fake county server.

mod support;

use eriscountygis::assist::Assistant;
use eriscountygis::counties::Counties;
use eriscountygis::http::{Http, Limits};
use eriscountygis::journal::Job;
use eriscountygis::registry::Registry;
use eriscountygis::runtime::{Runtime, VALIDATE_WITHIN};
use eriscountygis::tools::{Arcgis, SpecTool, Web};
use eriscountygis::work::{AgentJob, Work};
use erisharness::tools::{Content, Tool, ToolOutput};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn work(dir: &std::path::Path, job: Job) -> Arc<Work> {
    work_with(dir, job, Assistant::default())
}

fn work_with(dir: &std::path::Path, job: Job, assistant: Assistant) -> Arc<Work> {
    let work = Arc::new(Work {
        counties: Arc::new(Counties::embedded().unwrap()),
        registry: Mutex::new(Registry::open(&dir.join("registry")).unwrap()),
        runtime: Runtime {
            http: Http::new(Limits { allow_private: true, ..Limits::default() }).unwrap(),
            cache: dir.join("cache"),
            validate_within: VALIDATE_WITHIN,
        },
        assistant,
        agents: Mutex::default(),
        validate_budget: 3,
        log: Mutex::default(),
        logged: Default::default(),
    });
    work.agents.lock().unwrap().insert("agent".into(), AgentJob::new(job, 0, "mini".into()));
    work
}

fn text(output: &ToolOutput) -> String {
    output.content.iter().map(|c| if let Content::Text(t) = c { t.as_str() } else { "" }).collect()
}

async fn server() -> String {
    let dane = Counties::embedded().unwrap().get("55025").unwrap().interior;
    support::serve(support::Grid::around(dane[0], dane[1])).await
}

#[tokio::test]
async fn help_is_an_answer_and_mistakes_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let work = work(dir.path(), Job::County("55025".into()));
    let web = Web(work.clone());
    let help = web.run("agent", "--help").await;
    assert!(!help.is_error && text(&help).contains("search"), "{}", text(&help));
    let get_help = web.run("agent", "get --help").await;
    assert!(!get_help.is_error && text(&get_help).contains("--grep"));
    assert!(web.run("agent", "get").await.is_error, "a missing URL");
    assert!(web.run("agent", "fetch https://x.org").await.is_error, "an unknown command");
    assert!(web.run("agent", "get 'unclosed").await.is_error);
    assert!(web.run("agent", "get https://x.org --context 2").await.is_error, "--context needs --grep");
    assert!(web.description().contains("Usage: web"));
    assert!(web.description().contains("get"));
    let spec = SpecTool { work: work.clone(), statewide: false };
    assert!(spec.description().contains("Give the JSON as one argument in single quotes"));
    assert!(spec.description().contains("unavailable"));
    assert!(spec.run("agent", "unavailable paywalled --note x").await.is_error, "--evidence is required");
    assert!(spec.run("agent", "unavailable broke --evidence https://x.org --note x").await.is_error);
}

#[tokio::test]
async fn web_get_greps_long_answers() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let web = Web(work(dir.path(), Job::County("55025".into())));
    let dane = Counties::embedded().unwrap().get("55025").unwrap().interior;
    let url = format!(
        "{root}/arcgis/good/FeatureServer/0/query?geometry={},{},{},{}&f=geojson&resultRecordCount=3",
        dane[0] - 0.001,
        dane[1] - 0.001,
        dane[0] + 0.001,
        dane[1] + 0.001
    );
    let output = web.run("agent", &format!("web get \"{url}\" --grep 'parcelno' --context 1")).await;
    let shown = text(&output);
    assert!(!output.is_error, "{shown}");
    assert!(shown.contains("3 of ") && shown.contains("lines match parcelno"), "{shown}");
    assert!(
        shown.contains(":         \"PARCELNO\": \"0810-"),
        "the JSON is pretty-printed, one value per line: {shown}"
    );
    assert_eq!(
        work_activity(&web.0),
        format!("web get \"{url}\" --grep 'parcelno' --context 1"),
        "the TUI shows the command line"
    );
}

fn work_activity(work: &Work) -> String {
    work.with_agent("agent", |a| a.activity.clone()).unwrap()
}

#[tokio::test]
async fn arcgis_probe_lists_a_layers_fields() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let arcgis = Arcgis(work(dir.path(), Job::County("55025".into())));
    let output = arcgis.run("agent", &format!("probe {root}/arcgis/good/FeatureServer/0")).await;
    let shown = text(&output);
    assert!(shown.contains("maxRecordCount: 50") && shown.contains("features: 144"), "{shown}");
    assert!(shown.contains("PARCELNO (String) Parcel number"), "{shown}");
    let grepped = text(&arcgis.run("agent", &format!("probe {root}/arcgis/good/FeatureServer/0 --grep owner")).await);
    assert!(grepped.contains("OWNER") && !grepped.contains("SITEADDR"), "{grepped}");
}

#[tokio::test]
async fn specs_are_validated_shown_and_submitted_from_the_command_line() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let work = work(dir.path(), Job::County("55025".into()));
    let spec = SpecTool { work: work.clone(), statewide: false };

    assert!(spec.run("agent", "submit").await.is_error, "nothing validated yet");
    let bad = json!({"id": "fake", "publisher": "Fake", "source": {"kind": "arcgis-feature", "url": format!("{root}/arcgis/good/FeatureServer/0")},
        "coverage": {"counties": ["55025"]}, "fields": {"apn": "PIN"}});
    let output = spec.run("agent", &format!("validate '{bad}'")).await;
    assert!(text(&output).contains("\"passed\": false") && text(&output).contains("2 left"), "{}", text(&output));
    assert!(spec.run("agent", "submit").await.is_error, "the last spec failed");

    let good = json!({"id": "fake", "publisher": "Fake", "source": {"kind": "arcgis-feature", "url": format!("{root}/arcgis/good/FeatureServer/0")},
        "coverage": {"counties": ["55025"]}, "fields": {"apn": "PARCELNO", "owner": "OWNER"}, "filter": "NAME <> 'X'"});
    let line = format!("spec validate '{}'", good.to_string().replace('\'', r"'\''"));
    let output = spec.run("agent", &line).await;
    assert!(text(&output).contains("\"passed\": true"), "{}", text(&output));
    let shown = text(&spec.run("agent", "show").await);
    assert!(shown.contains("\"filter\": \"NAME <> 'X'\"") && shown.contains("\"passed\": true"), "{shown}");
    let saved = spec.run("agent", "submit").await;
    assert!(!saved.is_error, "{}", text(&saved));
    let registry = Registry::open(&dir.path().join("registry")).unwrap();
    assert_eq!(registry.adapters["fake"].filter.as_deref(), Some("NAME <> 'X'"));
    assert_eq!(registry.index["55025"], ["fake"]);
}

#[tokio::test]
async fn a_statewide_search_cannot_mark_counties_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let spec = SpecTool { work: work(dir.path(), Job::State("WI".into())), statewide: true };
    assert_eq!(spec.name(), "statewide_spec");
    assert!(!spec.description().contains("unavailable"), "{}", spec.description());
    let output = spec.run("agent", "unavailable paywalled --evidence https://x.org --note x").await;
    assert!(output.is_error);
    assert!(spec.run("agent", "statewide_spec show").await.is_error, "nothing validated yet");
}

/// A Claude login bound to a Claude Code credentials file in `dir`, on the stand-in API.
async fn claude_assistant(dir: &std::path::Path, model: &support::model::Model) -> Assistant {
    use erisagentauth::claude::ClaudeAccount;
    let expires = eriscountygis::registry::now() + 3_600_000;
    let credentials = dir.join("claude-code.json");
    std::fs::write(
        &credentials,
        json!({"claudeAiOauth": {"accessToken": "from-claude-code", "expiresAt": expires, "scopes": ["user:inference"]}})
            .to_string(),
    )
    .unwrap();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let app = erisagentauth::App { name: "test", originator: "test" };
    let openai =
        erisagentauth::openai::OpenAiAccount::load(dir.join("chatgpt.json"), &model.url, app, events.clone()).unwrap();
    let claude = ClaudeAccount::load(dir.join("claude.json"), &model.url, &model.url, events).unwrap();
    claude.bind(&credentials).await.unwrap();
    let endpoints = eriscountygis::models::Endpoints { codex: model.url.clone(), claude_api: model.url.clone() };
    let listed = eriscountygis::models::list_claude(&claude, &model.url).await.unwrap();
    let providers = eriscountygis::models::providers(&openai, &claude, &endpoints, "test").await.unwrap();
    Assistant::from_models(&listed, &providers, &claude, &model.url)
}

#[tokio::test(flavor = "multi_thread")]
async fn searching_and_reading_bring_back_a_few_lines_and_count_their_tokens() {
    use support::model::{Brain, Model, Reply, Request};
    let dir = tempfile::tempdir().unwrap();
    let root = server().await;
    let brain: Brain = Arc::new(|request: &Request| {
        if request.web_search {
            let lines: Vec<String> =
                (0..10).map(|i| format!("https://county{i}.example.gov/gis — county {i}'s parcel viewer")).collect();
            return Reply::Says(lines.join("\n"));
        }
        let page = request.briefing();
        assert!(page.contains(support::PAGE_FILLER), "the reader gets the whole page");
        let service = page.split("[Parcels service](").nth(1).unwrap().split(')').next().unwrap().to_owned();
        Reply::Says(format!("The parcel layer is {service}"))
    });
    let model = Model::start(brain).await;
    let assistant = claude_assistant(dir.path(), &model).await;
    assert_eq!(assistant.reader.as_ref().unwrap().effort.as_deref(), Some("low"));
    let work = work_with(dir.path(), Job::County("55025".into()), assistant);
    let web = Web(work.clone());

    let search = web.run("agent", "search dane county parcels gis").await;
    assert!(!search.is_error, "{}", text(&search));
    let shown = text(&search);
    assert_eq!(shown.lines().filter(|l| l.starts_with("- ")).count(), 8, "{shown}");
    assert!(shown.contains(
        "- Title of https://county0.example.gov/gis\n  https://county0.example.gov/gis\n  county 0's parcel viewer"
    ));
    assert!(!shown.contains("DuckDuckGo"));

    let read = web.run("agent", &format!("read {root}/page where is the parcel service")).await;
    assert!(!read.is_error, "{}", text(&read));
    let answer = text(&read);
    assert!(answer.contains(&format!("{root}/arcgis/good/FeatureServer/0")), "{answer}");
    assert!(!answer.contains(support::PAGE_FILLER) && answer.len() < 500, "the page stays out: {answer}");

    let requests = model.requests.lock().unwrap();
    let reader = requests.iter().find(|r| !r.web_search).unwrap();
    assert_eq!((reader.model.as_str(), reader.effort.as_deref()), ("claude-haiku-9", Some("low")));
    drop(requests);
    let (tools, total) = work.with_agent("agent", |a| (a.tool_tokens.clone(), a.tokens.clone())).unwrap();
    assert_eq!((tools["web search"].input, tools["web search"].output), (300, 40));
    assert!(tools["web read"].input > 0);
    assert_eq!(total.input, tools["web search"].input + tools["web read"].input, "tool tokens count toward the job");
}

#[tokio::test]
async fn without_a_login_search_falls_back_and_reading_points_to_web_get() {
    let dir = tempfile::tempdir().unwrap();
    let root = server().await;
    let assistant = Assistant { fallback_search: format!("{root}/ddg"), ..Assistant::default() };
    let work = work_with(dir.path(), Job::County("55025".into()), assistant);
    let web = Web(work.clone());
    let search = web.run("agent", "search dane county parcels").await;
    let shown = text(&search);
    assert!(!search.is_error, "{shown}");
    assert_eq!(shown.lines().filter(|l| l.starts_with("- ")).count(), 8, "{shown}");
    assert!(shown.contains("https://county0.example.gov/gis"));
    assert!(shown.contains("from DuckDuckGo: web search needs a Claude login"), "{shown}");
    let read = web.run("agent", &format!("read {root}/page where is the parcel service")).await;
    assert!(read.is_error && text(&read).contains("web get"), "{}", text(&read));
    assert!(work.with_agent("agent", |a| a.tool_tokens.is_empty()).unwrap());
}

#[tokio::test]
async fn web_get_shows_a_page_in_small_windows() {
    let dir = tempfile::tempdir().unwrap();
    let root = server().await;
    let web = Web(work(dir.path(), Job::County("55025".into())));
    let shown = text(&web.run("agent", &format!("get {root}/page")).await);
    assert!(shown.contains("[characters 0–4000 of"), "{}", &shown[..200.min(shown.len())]);
    assert!(shown.len() < 4_500);
}
