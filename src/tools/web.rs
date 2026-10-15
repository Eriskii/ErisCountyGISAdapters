//! `web`: search the web, read a page by asking a question about it, and GET endpoints.
//! Searching and reading run on models of their own (see `assist`), so only their few lines
//! reach the agent; their tokens count toward the agent's job.

use super::text::{duckduckgo_results, readable, window_of};
use crate::assist::{Found, SEARCH_RESULTS};
use crate::cli;
use crate::work::Work;
use clap::{CommandFactory, Parser, Subcommand};
use erisharness::tools::{Tool, ToolContext, ToolOutput};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

/// Biggest answer `web get` and `web read` take.
const CAP: usize = 8 << 20;
/// Characters of a `web get` answer shown per call.
pub const GET_WINDOW: usize = 4_000;

#[derive(Parser)]
#[command(
    name = "web",
    disable_version_flag = true,
    about = "Read the public web: search it, ask a question of a page, and GET JSON endpoints."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search the web; prints up to 8 results, each a title, URL and one line on what it holds.
    Search {
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Read a web page and answer a question about it: a model reads the whole page and
    /// answers in a few lines, quoting the URLs it found. Use this for HTML pages.
    #[command(after_help = "Example: web read https://county.example.gov/gis which link leads to the parcel \
        map service or an ArcGIS REST URL?")]
    Read {
        url: String,
        /// The question, such as: which links lead to parcel data or ArcGIS services?
        #[arg(required = true)]
        question: Vec<String>,
    },
    /// GET an endpoint as it is, for JSON and REST answers: JSON is pretty-printed, one value
    /// per line, so --grep can pick lines out of it. For HTML pages, use `web read`.
    #[command(after_help = format!("Answers come in windows of {GET_WINDOW} characters; pass --offset to read on. \
        Example: web get https://gis.example.gov/arcgis/rest/services?f=json --grep 'parcel' --context 2"))]
    Get {
        url: String,
        /// Character of the answer (after --grep) to start from.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Show only lines matching this case-insensitive regular expression, such as
        /// 'FeatureServer|MapServer'.
        #[arg(long)]
        grep: Option<String>,
        /// Lines shown around each --grep match.
        #[arg(long, default_value_t = 0, requires = "grep")]
        context: usize,
    },
}

pub struct Web(pub Arc<Work>);

/// An answer as agents read it: HTML as text, JSON pretty-printed, anything else as is.
pub fn shown(body: &str, content_type: &str, url: &str) -> String {
    if content_type.contains("html") || body.trim_start().starts_with('<') {
        return readable(body, url);
    }
    match serde_json::from_str::<Value>(body) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_owned()),
        Err(_) => body.to_owned(),
    }
}

impl Web {
    pub async fn run(&self, agent: &str, line: &str) -> ToolOutput {
        super::activity(&self.0, agent, "web", line);
        let cli: Cli = match cli::parse("web", line) {
            Ok(cli) => cli,
            Err(output) => return output,
        };
        match cli.command {
            Command::Get { url, offset, grep, context } => {
                let fetched = match self.0.runtime.http.get(&url, CAP).await {
                    Ok(fetched) => fetched,
                    Err(error) => return ToolOutput::error(format!("{error:#}")),
                };
                let mut text = shown(&fetched.text(), &fetched.content_type, &fetched.url);
                if let Some(pattern) = grep {
                    text = match cli::grep(&text, &pattern, context) {
                        Ok(text) => text,
                        Err(error) => return ToolOutput::error(error),
                    };
                }
                let cut = if fetched.truncated { " (cut at 8 MB)" } else { "" };
                ToolOutput::text(format!(
                    "HTTP {} {} from {}{cut}\n\n{}",
                    fetched.status,
                    fetched.content_type,
                    fetched.url,
                    window_of(&text, offset, GET_WINDOW)
                ))
            }
            Command::Read { url, question } => self.read(agent, &url, &question.join(" ")).await,
            Command::Search { query } => self.search(agent, &query.join(" ")).await,
        }
    }

    async fn read(&self, agent: &str, url: &str, question: &str) -> ToolOutput {
        let fetched = match self.0.runtime.http.get(url, CAP).await {
            Ok(fetched) => fetched,
            Err(error) => return ToolOutput::error(format!("{error:#}")),
        };
        let page = shown(&fetched.text(), &fetched.content_type, &fetched.url);
        match self.0.assistant.read(&fetched.url, &page, question).await {
            Ok((answer, tokens)) => {
                self.0.charge(agent, "web read", &tokens);
                ToolOutput::text(format!("{answer}\n\n(HTTP {} from {})", fetched.status, fetched.url))
            }
            Err(error) => ToolOutput::error(format!(
                "reading the page failed: {error:#}. `web get {url} --grep <pattern>` shows lines of it instead"
            )),
        }
    }

    async fn search(&self, agent: &str, query: &str) -> ToolOutput {
        let note = match self.0.assistant.search(query).await {
            Ok((found, tokens)) if !found.is_empty() => {
                self.0.charge(agent, "web search", &tokens);
                return ToolOutput::text(listing(&found));
            }
            Ok((_, tokens)) => {
                self.0.charge(agent, "web search", &tokens);
                "the search found nothing".to_owned()
            }
            Err(error) => format!("{error:#}"),
        };
        let mut url = match url::Url::parse(&self.0.assistant.fallback_search) {
            Ok(url) => url,
            Err(error) => {
                return ToolOutput::error(format!("no search: {note}; the fallback search is not a URL: {error}"));
            }
        };
        url.query_pairs_mut().append_pair("q", query);
        match self.0.runtime.http.get(url.as_str(), 2 << 20).await {
            Ok(fetched) => {
                let found: Vec<Found> = duckduckgo_results(&fetched.text())
                    .into_iter()
                    .map(|(title, url, about)| Found { title, url, about })
                    .collect();
                if found.is_empty() {
                    return ToolOutput::error(format!(
                        "no results ({note}; DuckDuckGo answered HTTP {}); try other words, or arcgis search",
                        fetched.status
                    ));
                }
                ToolOutput::text(format!("{}\n(from DuckDuckGo: {note})", listing(&found)))
            }
            Err(error) => ToolOutput::error(format!("no search: {note}; DuckDuckGo: {error:#}")),
        }
    }
}

/// Search results as agents read them: at most [`SEARCH_RESULTS`], each on two lines.
fn listing(found: &[Found]) -> String {
    found
        .iter()
        .take(SEARCH_RESULTS)
        .map(|f| {
            let title = if f.title.is_empty() { f.url.as_str() } else { f.title.as_str() };
            let about = super::text::short(&f.about, 200);
            if about.is_empty() {
                format!("- {title}\n  {}", f.url)
            } else {
                format!("- {title}\n  {}\n  {about}", f.url)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tool for Web {
    fn name(&self) -> &str {
        "web"
    }
    fn description(&self) -> String {
        format!(
            "Runs a `web` command, given as a command line. There is no shell: no pipes, no redirects. \
             Search, then ask questions of pages with `web read`; `web get` is for JSON endpoints, with \
             --grep to cut long answers down.\n\n{}",
            Cli::command().render_long_help()
        )
    }
    fn parameters(&self) -> Value {
        cli::parameters("get https://example.gov/arcgis/rest/services --grep parcel")
    }
    fn snippet(&self) -> &str {
        "web: search the web, ask a question of a page, and GET JSON endpoints"
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match cli::line(&args, "web", "search fulton county parcels") {
                Ok(line) => cli::until_stopped(context, self.run(&context.agent, &line)).await,
                Err(output) => output,
            }
        })
    }
}
