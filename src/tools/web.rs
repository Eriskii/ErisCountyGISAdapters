//! `web`: GET public pages and endpoints, and search.

use super::text::{WINDOW, duckduckgo_results, readable, window};
use crate::cli;
use crate::work::Work;
use clap::{CommandFactory, Parser, Subcommand};
use erisharness::tools::{Tool, ToolContext, ToolOutput};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

/// Biggest answer `web get` reads.
const CAP: usize = 8 << 20;

#[derive(Parser)]
#[command(
    name = "web",
    disable_version_flag = true,
    about = "Read the public web: GET pages and endpoints, and search."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// GET a page or endpoint. HTML reads as text with links as [text](url); JSON is
    /// pretty-printed, one value per line, so --grep can pick lines out of it.
    #[command(after_help = format!("Answers come in windows of {WINDOW} characters; pass --offset to read on. \
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
    /// Search the web; prints titles, URLs and snippets.
    Search {
        #[arg(required = true)]
        query: Vec<String>,
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
                    window(&text, offset)
                ))
            }
            Command::Search { query } => {
                let query = query.join(" ");
                let mut url = url::Url::parse("https://html.duckduckgo.com/html/").expect("a URL");
                url.query_pairs_mut().append_pair("q", &query);
                match self.0.runtime.http.get(url.as_str(), 2 << 20).await {
                    Ok(fetched) => {
                        let results = duckduckgo_results(&fetched.text());
                        if results.is_empty() {
                            return ToolOutput::error(format!(
                                "no results (HTTP {}); try other words, or arcgis search",
                                fetched.status
                            ));
                        }
                        let lines: Vec<String> =
                            results.iter().take(12).map(|(t, u, s)| format!("- {t}\n  {u}\n  {s}")).collect();
                        ToolOutput::text(lines.join("\n"))
                    }
                    Err(error) => ToolOutput::error(format!("{error:#}")),
                }
            }
        }
    }
}

impl Tool for Web {
    fn name(&self) -> &str {
        "web"
    }
    fn description(&self) -> String {
        format!(
            "Runs a `web` command, given as a command line. There is no shell: no pipes, no redirects; \
             use --grep to cut long answers down.\n\n{}",
            Cli::command().render_long_help()
        )
    }
    fn parameters(&self) -> Value {
        cli::parameters("get https://example.gov/arcgis/rest/services --grep parcel")
    }
    fn snippet(&self) -> &str {
        "web: GET public pages and endpoints (with --grep), and search the web"
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
