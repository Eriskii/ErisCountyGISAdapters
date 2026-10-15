//! Calls to the real search and reading models on a bound Claude login, run by hand:
//! `ERISCOUNTYGIS_LIVE_STATE=<state dir> cargo test --test live -- --ignored --nocapture`.

use erisagentauth::App;
use erisagentauth::claude::{self, ClaudeAccount};
use erisagentauth::openai::OpenAiAccount;
use eriscountygis::assist::{Assistant, Reader, Searcher};
use eriscountygis::models::{self, Endpoints};
use std::path::PathBuf;

async fn assistant() -> Assistant {
    let state = PathBuf::from(std::env::var("ERISCOUNTYGIS_LIVE_STATE").expect("ERISCOUNTYGIS_LIVE_STATE"));
    let (events, _) = tokio::sync::broadcast::channel(16);
    let app = App { name: "live", originator: "live" };
    let claude =
        ClaudeAccount::load(state.join("accounts/claude.json"), claude::API, claude::AUTH, events.clone()).unwrap();
    let openai =
        OpenAiAccount::load(state.join("accounts/chatgpt.json"), erisagentauth::openai::AUTH, app, events).unwrap();
    let endpoints = Endpoints { codex: erisagentauth::openai::CODEX_API.into(), claude_api: claude::API.into() };
    let listed = models::list_claude(&claude, claude::API).await.unwrap();
    let haiku = listed.iter().find(|m| m.id.contains("haiku") && m.cannot_run.is_none()).unwrap().id.clone();
    let providers = models::providers(&openai, &claude, &endpoints, "live").await.unwrap();
    let provider = providers.into_iter().find(|(n, _)| *n == models::ANTHROPIC).unwrap().1;
    Assistant {
        reader: Some(Reader { provider, model: haiku.clone(), effort: Some("low".into()) }),
        searcher: Some(Searcher { account: claude, api: claude::API.into(), model: haiku }),
        ..Assistant::default()
    }
}

#[tokio::test]
#[ignore]
async fn a_real_search_and_read() {
    let assistant = assistant().await;
    let (found, tokens) = assistant.search("Wyandot County Ohio parcel GIS ArcGIS REST").await.unwrap();
    println!("search: {tokens:?}");
    for f in &found {
        println!("- {} | {} | {}", f.title, f.url, f.about);
    }
    assert!(!found.is_empty());
    let http = eriscountygis::http::Http::new(eriscountygis::http::Limits::default()).unwrap();
    let url = "https://landgauge.com/ohio/wyandot-county";
    let fetched = http.get(url, 8 << 20).await.unwrap();
    let page = eriscountygis::tools::shown(&fetched.text(), &fetched.content_type, &fetched.url);
    let (answer, tokens) =
        assistant.read(url, &page, "which links lead to parcel data or GIS services?").await.unwrap();
    println!("read ({} characters of page): {tokens:?}\n{answer}", page.chars().count());
}
