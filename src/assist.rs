//! Models that work for the agents' tools, outside the agents' own context: one searches the
//! web, one reads a page and answers a question about it. Each returns a few lines and the
//! tokens it spent, which count toward the calling agent's job.
//!
//! Search is Claude's server-side `web_search` tool, asked for in one Messages request made as
//! Claude Code makes them (ErisHarness's Anthropic provider sends only function tools, so the
//! request is made here with its public `fingerprint` helpers). Reading runs through an
//! ErisHarness provider like any model call.

use crate::journal::Tokens;
use crate::models::{ANTHROPIC, CODEX, Model};
use anyhow::{Result, anyhow, ensure};
use erisagentauth::claude::ClaudeAccount;
use erisharness::agent::Item;
use erisharness::provider::anthropic::fingerprint;
use erisharness::provider::{Provider, Request};
use serde_json::{Value, json};
use std::sync::Arc;

/// Most results a search returns.
pub const SEARCH_RESULTS: usize = 8;
/// Most characters of a page the reader is given.
pub const PAGE_CHARS: usize = 120_000;
/// Longest answer the reader gives, in tokens.
const READ_MAX_TOKENS: u64 = 1_000;

const SEARCH_SYSTEM: &str = "You run one web search for a GIS researcher and report what it found. Search \
once with the query you are given. Then reply with one line per useful result, at most 8, each exactly \
`<url> — <one short line on what the page holds>`, best first. No other text.";

const READ_SYSTEM: &str = "You read one web page for a GIS researcher and answer their question about it \
from the page alone. Answer in at most a few short lines. Quote every URL you rely on exactly as the page \
gives it (links appear as [text](url)). If the page does not answer the question, say so in one line and \
name the links on it most likely to lead to the answer.";

/// A model and the provider that serves it, for reading pages.
pub struct Reader {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub effort: Option<String>,
}

/// Claude's web search on a Claude login, at the Anthropic API's root `api`.
pub struct Searcher {
    pub account: ClaudeAccount,
    pub api: String,
    pub model: String,
}

/// DuckDuckGo's HTML results page, which search falls back to without Claude's search.
pub const DUCKDUCKGO: &str = "https://html.duckduckgo.com/html/";

pub struct Assistant {
    pub reader: Option<Reader>,
    pub searcher: Option<Searcher>,
    /// Where search falls back to: an HTML results page in DuckDuckGo's form.
    pub fallback_search: String,
}

impl Default for Assistant {
    /// No models: search falls back to DuckDuckGo, and pages cannot be read.
    fn default() -> Self {
        Self { reader: None, searcher: None, fallback_search: DUCKDUCKGO.into() }
    }
}

impl Assistant {
    /// The assistant the signed-in logins allow, from their listed models and the providers
    /// that serve them: with a Claude login, the newest Haiku searches and reads; with ChatGPT
    /// alone, its small model reads and search falls back to DuckDuckGo. Both work at low effort.
    pub fn from_models(
        listed: &[Model],
        providers: &[(&'static str, Arc<dyn Provider>)],
        claude: &ClaudeAccount,
        claude_api: &str,
    ) -> Self {
        let provider = |name: &str| providers.iter().find(|(n, _)| *n == name).map(|(_, p)| p.clone());
        let low = |model: &Model| model.efforts.iter().any(|e| e == "low").then(|| "low".to_owned());
        let haiku = listed.iter().find(|m| m.provider == ANTHROPIC && m.cannot_run.is_none() && m.id.contains("haiku"));
        let small = ["mini", "nano", "spark", "small", "lite"];
        let codex_small = listed.iter().find(|m| m.provider == CODEX && small.iter().any(|w| m.id.contains(w)));
        let reader = match (haiku, codex_small) {
            (Some(m), _) => provider(ANTHROPIC).map(|p| (p, m)),
            (None, Some(m)) => provider(CODEX).map(|p| (p, m)),
            (None, None) => None,
        }
        .map(|(provider, m)| Reader { provider, model: m.id.clone(), effort: low(m) });
        let searcher =
            haiku.map(|m| Searcher { account: claude.clone(), api: claude_api.to_owned(), model: m.id.clone() });
        Self { reader, searcher, fallback_search: DUCKDUCKGO.into() }
    }
}

/// One search result as agents read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub title: String,
    pub url: String,
    pub about: String,
}

fn usage_tokens(usage: &Value) -> Tokens {
    let cached = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
    Tokens {
        input: usage["input_tokens"].as_u64().unwrap_or(0)
            + cached
            + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        cached_input: cached,
        output: usage["output_tokens"].as_u64().unwrap_or(0),
    }
}

impl Assistant {
    /// Answers `question` from `page` (the text of the page at `url`), with the tokens it took.
    pub async fn read(&self, url: &str, page: &str, question: &str) -> Result<(String, Tokens)> {
        let reader = self.reader.as_ref().ok_or_else(|| anyhow!("no model can read pages: no login is signed in"))?;
        let mut text: String = page.chars().take(PAGE_CHARS).collect();
        if text.len() < page.len() {
            text.push_str("\n[the page goes on; this is its beginning]");
        }
        let items = [Item::Input {
            from: "user".into(),
            text: format!("The page at {url}:\n\n{text}\n\nQuestion: {question}"),
            images: Vec::new(),
        }];
        let cache_key = uuid::Uuid::new_v4().to_string();
        let completion = reader
            .provider
            .complete(
                Request {
                    model: &reader.model,
                    reasoning_effort: reader.effort.as_deref(),
                    cache_key: &cache_key,
                    system: READ_SYSTEM,
                    tools: &[],
                    items: &items,
                },
                &|_| {},
            )
            .await?;
        let answer: Vec<&str> = completion
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let answer = answer.join("\n").trim().to_owned();
        ensure!(!answer.is_empty(), "the reader gave no answer");
        let usage = completion.usage;
        Ok((answer, Tokens { input: usage.input, cached_input: usage.cached_input, output: usage.output }))
    }

    /// Searches the web for `query`: up to [`SEARCH_RESULTS`] results, with the tokens it took.
    pub async fn search(&self, query: &str) -> Result<(Vec<Found>, Tokens)> {
        let searcher = self.searcher.as_ref().ok_or_else(|| anyhow!("web search needs a Claude login"))?;
        let login = searcher.account.current().await?;
        let identity = searcher.account.claude_code().await;
        let version = fingerprint::DEFAULT_VERSION;
        let session = uuid::Uuid::new_v4().to_string();
        let prompt = format!("Search the web for: {query}");
        let body = json!({
            "model": searcher.model,
            "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
            "system": [
                {"type": "text", "text": fingerprint::billing(&prompt, version)},
                {"type": "text", "text": fingerprint::IDENTITY, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": SEARCH_SYSTEM},
            ],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 1}],
            "metadata": fingerprint::metadata(&identity.install_id, identity.account_id.as_deref(), &session),
            "max_tokens": READ_MAX_TOKENS,
        });
        let bytes = fingerprint::serialize(&body, true)?;
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/messages?beta=true", searcher.api.trim_end_matches('/')))
            .bearer_auth(&login.access)
            .timeout(std::time::Duration::from_secs(120))
            .body(bytes);
        for (key, value) in fingerprint::headers(version, &session, true, false) {
            if key.eq_ignore_ascii_case("accept-encoding") {
                continue;
            }
            request = request.header(key, value);
        }
        let response = request.send().await.map_err(|e| anyhow!("web search failed: {}", e.without_url()))?;
        let status = response.status();
        let answer: Value = response.json().await.map_err(|e| anyhow!("web search answered badly: {e}"))?;
        ensure!(
            status.is_success(),
            "web search failed ({status}): {}",
            answer["error"]["message"].as_str().unwrap_or("no message")
        );
        Ok((found(&answer), usage_tokens(&answer["usage"])))
    }
}

/// The results of a search answer: the model's lines, titled from the search's own results, or
/// the search's results as they came when the model wrote no usable line.
pub fn found(answer: &Value) -> Vec<Found> {
    let blocks = answer["content"].as_array().cloned().unwrap_or_default();
    let mut titles: Vec<(String, String)> = Vec::new();
    for block in blocks.iter().filter(|b| b["type"] == "web_search_tool_result") {
        for result in block["content"].as_array().into_iter().flatten() {
            if let (Some(url), Some(title)) = (result["url"].as_str(), result["title"].as_str()) {
                titles.push((url.to_owned(), title.to_owned()));
            }
        }
    }
    let text: String =
        blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
    let mut out: Vec<Found> = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']).trim();
        let Some((url, about)) = line.split_once(" — ").or_else(|| line.split_once(" - ")) else { continue };
        let url = url.trim().trim_matches(['<', '>', '`']);
        if !url.starts_with("http") || out.iter().any(|f| f.url == url) {
            continue;
        }
        let title = titles.iter().find(|(u, _)| u == url).map(|(_, t)| t.clone()).unwrap_or_default();
        out.push(Found { title, url: url.to_owned(), about: about.trim().to_owned() });
    }
    if out.is_empty() {
        out = titles.into_iter().map(|(url, title)| Found { title, url, about: String::new() }).collect();
    }
    out.truncate(SEARCH_RESULTS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_answer_reads_as_titled_lines() {
        let answer = json!({"content": [
            {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
            {"type": "web_search_tool_result", "tool_use_id": "s1", "content": [
                {"type": "web_search_result", "url": "https://a.gov/parcels", "title": "A Parcels", "encrypted_content": "x"},
                {"type": "web_search_result", "url": "https://b.org/gis", "title": "B GIS", "encrypted_content": "y"},
            ]},
            {"type": "text", "text": "- https://a.gov/parcels — the county's parcel viewer\nnoise line\n"},
            {"type": "text", "text": "https://b.org/gis — open data hub"},
        ]});
        assert_eq!(
            found(&answer),
            [
                Found {
                    title: "A Parcels".into(),
                    url: "https://a.gov/parcels".into(),
                    about: "the county's parcel viewer".into()
                },
                Found { title: "B GIS".into(), url: "https://b.org/gis".into(), about: "open data hub".into() },
            ]
        );
        let bare = json!({"content": [{"type": "web_search_tool_result", "content": [
            {"type": "web_search_result", "url": "https://c.gov", "title": "C"}]}]});
        assert_eq!(found(&bare), [Found { title: "C".into(), url: "https://c.gov".into(), about: String::new() }]);
    }
}
