//! The agents' tools. Agents read the web (GET only, through the polite client), search for
//! layers, inspect ArcGIS services, and validate specs; a spec reaches the registry only through
//! `submit_spec`, which accepts nothing that has not passed validation.

use crate::journal::Job;
use crate::registry::now;
use crate::spec::{Reason, SPEC_VERSION, Spec, Unavailable};
use crate::work::{Resolution, Work, canonical};
use erisharness::tools::{Tool, ToolContext, ToolOutput};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;

/// Biggest page an agent may fetch.
const FETCH_CAP: usize = 4 << 20;
/// Characters of a page shown per call.
const WINDOW: usize = 16_000;
const DAY_MS: u64 = 86_400_000;

pub fn all(work: &Arc<Work>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Fetch(work.clone())),
        Arc::new(WebSearch(work.clone())),
        Arc::new(ArcgisSearch(work.clone())),
        Arc::new(ArcgisProbe(work.clone())),
        Arc::new(Validate(work.clone())),
        Arc::new(SubmitSpec(work.clone())),
        Arc::new(MarkUnavailable(work.clone())),
    ]
}

pub const COUNTY_TOOLS: [&str; 7] =
    ["fetch", "web_search", "arcgis_search", "arcgis_probe", "validate", "submit_spec", "mark_unavailable"];
pub const STATE_TOOLS: [&str; 6] = ["fetch", "web_search", "arcgis_search", "arcgis_probe", "validate", "submit_spec"];

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutput> {
    args[key].as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| ToolOutput::error(format!("{key} is required")))
}

fn job_of(work: &Work, context: &ToolContext) -> Result<Job, ToolOutput> {
    work.with_agent(&context.agent, |a| a.job.clone()).ok_or_else(|| ToolOutput::error("this agent has no job"))
}

fn activity(work: &Work, context: &ToolContext, line: String) {
    work.with_agent(&context.agent, |a| a.activity = line);
}

/// The window of `text` starting at `offset` characters, with where it sits.
fn window(text: &str, offset: usize) -> String {
    let total = text.chars().count();
    let body: String = text.chars().skip(offset).take(WINDOW).collect();
    let end = (offset + WINDOW).min(total);
    if offset == 0 && end == total {
        body
    } else {
        format!("[characters {offset}–{end} of {total}; pass offset {end} for more]\n{body}")
    }
}

/// A page's text for reading: scripts and styles dropped, links kept as `[text](href)`, tags
/// removed, whitespace collapsed.
pub fn readable(html: &str, base: &str) -> String {
    let base = url::Url::parse(base).ok();
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    let mut link: Option<String> = None;
    while i < html.len() {
        if html.as_bytes()[i] == b'<' {
            let end = html[i..].find('>').map_or(html.len(), |e| i + e + 1);
            let tag = &lower[i..end];
            for skip in ["<script", "<style", "<noscript", "<svg"] {
                if tag.starts_with(skip) {
                    let close = format!("</{}", &skip[1..]);
                    let after = lower[end..].find(&close).map_or(html.len(), |e| end + e);
                    i = lower[after..].find('>').map_or(html.len(), |e| after + e + 1);
                    break;
                }
            }
            if i > end || i >= html.len() {
                continue;
            }
            if tag.starts_with("<a ") || tag.starts_with("<a\n") {
                let raw = &html[i..end];
                link = attribute(raw, "href").map(|h| match &base {
                    Some(base) => base.join(&h).map_or(h, |u| u.to_string()),
                    None => h,
                });
                out.push('[');
            } else if tag.starts_with("</a") {
                match link.take() {
                    Some(href) => out.push_str(&format!("]({href})")),
                    None => out.push(']'),
                }
            } else if ["<br", "<p", "</p", "<div", "</div", "<li", "<tr", "<h", "</h", "<table"]
                .iter()
                .any(|t| tag.starts_with(t))
            {
                out.push('\n');
            } else if tag.starts_with("<td") || tag.starts_with("<th") {
                out.push(' ');
            }
            i = end;
        } else {
            let next = html[i..].find('<').map_or(html.len(), |n| i + n);
            out.push_str(&decode_entities(&html[i..next]));
            i = next;
        }
    }
    let mut collapsed = String::new();
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() && line != "[]" {
            collapsed.push_str(&line);
            collapsed.push('\n');
        }
    }
    collapsed
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let value = match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next()?,
        _ => rest.split(|c: char| c.is_whitespace() || c == '>').next()?,
    };
    Some(decode_entities(value))
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

pub struct Fetch(pub Arc<Work>);

impl Tool for Fetch {
    fn name(&self) -> &str {
        "fetch"
    }
    fn description(&self) -> String {
        "GET a public web page or JSON endpoint and read it. HTML is shown as text with links as [text](url); \
         JSON is shown as is. Long answers come in windows: pass offset to read on."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {
            "url": {"type": "string"},
            "offset": {"type": "integer", "description": "Character to start from, for long answers."},
        }, "required": ["url"]})
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let url = match text_arg(&args, "url") {
                Ok(url) => url,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("fetch {url}"));
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            match self.0.runtime.http.get(url, FETCH_CAP).await {
                Ok(fetched) => {
                    let text = fetched.text();
                    let shown = if fetched.content_type.contains("html") || text.trim_start().starts_with('<') {
                        readable(&text, &fetched.url)
                    } else {
                        text
                    };
                    let cut = if fetched.truncated { " (cut at 4 MB)" } else { "" };
                    ToolOutput::text(format!(
                        "HTTP {} {} from {}{cut}\n\n{}",
                        fetched.status,
                        fetched.content_type,
                        fetched.url,
                        window(&shown, offset)
                    ))
                }
                Err(error) => ToolOutput::error(format!("{error:#}")),
            }
        })
    }
}

pub struct WebSearch(pub Arc<Work>);

/// Results from DuckDuckGo's HTML page: title, URL and snippet.
pub fn duckduckgo_results(html: &str) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        let href = attribute(&format!(" {block}"), "href").unwrap_or_default();
        let target = url::Url::parse(&format!("https:{href}"))
            .ok()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned()))
            .unwrap_or(href);
        let title =
            block.split_once('>').map(|(_, rest)| rest.split("</a>").next().unwrap_or_default()).unwrap_or_default();
        let snippet = block
            .split_once("class=\"result__snippet\"")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(_, rest)| rest.split("</a>").next().unwrap_or_default())
            .unwrap_or_default();
        let clean = |s: &str| readable(s, "https://duckduckgo.com/").trim().replace('\n', " ");
        results.push((clean(title), target, clean(snippet)));
    }
    results
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> String {
        "Search the web. Returns titles, URLs and snippets.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]})
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let query = match text_arg(&args, "query") {
                Ok(q) => q,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("search {query}"));
            let mut url = url::Url::parse("https://html.duckduckgo.com/html/").expect("a URL");
            url.query_pairs_mut().append_pair("q", query);
            match self.0.runtime.http.get(url.as_str(), 2 << 20).await {
                Ok(fetched) => {
                    let results = duckduckgo_results(&fetched.text());
                    if results.is_empty() {
                        return ToolOutput::error(format!(
                            "no results (HTTP {}); try other words or arcgis_search",
                            fetched.status
                        ));
                    }
                    let lines: Vec<String> =
                        results.iter().take(12).map(|(t, u, s)| format!("- {t}\n  {u}\n  {s}")).collect();
                    ToolOutput::text(lines.join("\n"))
                }
                Err(error) => ToolOutput::error(format!("{error:#}")),
            }
        })
    }
}

pub struct ArcgisSearch(pub Arc<Work>);

fn strip_html(text: &str) -> String {
    readable(text, "https://www.arcgis.com/").trim().replace('\n', " ")
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text.to_owned()
    }
}

impl Tool for ArcgisSearch {
    fn name(&self) -> &str {
        "arcgis_search"
    }
    fn description(&self) -> String {
        "Search ArcGIS Online and ArcGIS Hub for public items (feature services, map services, open data \
         datasets) near your county or state. Returns titles, owners, types and service URLs."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {
            "query": {"type": "string", "description": "Such as `parcels` or `tax parcels Fulton`."},
        }, "required": ["query"]})
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let query = match text_arg(&args, "query") {
                Ok(q) => q,
                Err(e) => return e,
            };
            let job = match job_of(&self.0, context) {
                Ok(job) => job,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("arcgis_search {query}"));
            let bbox = match &job {
                Job::County(fips) => self.0.counties.get(fips).map(|c| c.bbox),
                Job::State(state) => {
                    let boxes: Vec<_> = self.0.counties.in_state(state).map(|c| c.bbox).collect();
                    boxes.into_iter().reduce(|a, b| {
                        crate::geom::Bbox([
                            a.west().min(b.west()),
                            a.south().min(b.south()),
                            a.east().max(b.east()),
                            a.north().max(b.north()),
                        ])
                    })
                }
            };
            let corners = bbox
                .map(|b| format!("{:.4},{:.4},{:.4},{:.4}", b.west(), b.south(), b.east(), b.north()))
                .unwrap_or_default();
            let mut out = Vec::new();

            let mut hub = url::Url::parse("https://hub.arcgis.com/api/search/v1/collections/all/items").expect("a URL");
            hub.query_pairs_mut().append_pair("q", query).append_pair("bbox", &corners).append_pair("limit", "15");
            match self.0.runtime.http.get_json(hub.as_str(), 8 << 20).await {
                Ok(body) => {
                    out.push("ArcGIS Hub:".to_owned());
                    for f in body["features"].as_array().into_iter().flatten() {
                        let p = &f["properties"];
                        out.push(format!(
                            "- {} [{}] by {}\n  {}\n  {}",
                            p["title"].as_str().unwrap_or("?"),
                            p["type"].as_str().unwrap_or("?"),
                            p["owner"].as_str().or(p["source"].as_str()).unwrap_or("?"),
                            p["url"].as_str().unwrap_or("(no service URL)"),
                            short(&strip_html(p["snippet"].as_str().unwrap_or_default()), 200),
                        ));
                    }
                }
                Err(error) => out.push(format!("ArcGIS Hub failed: {error:#}")),
            }

            let mut online = url::Url::parse("https://www.arcgis.com/sharing/rest/search").expect("a URL");
            online
                .query_pairs_mut()
                .append_pair("q", &format!("{query} (type:\"Feature Service\" OR type:\"Map Service\")"))
                .append_pair("bbox", &corners)
                .append_pair("num", "15")
                .append_pair("f", "json");
            match self.0.runtime.http.get_json(online.as_str(), 8 << 20).await {
                Ok(body) => {
                    out.push("ArcGIS Online:".to_owned());
                    for r in body["results"].as_array().into_iter().flatten() {
                        out.push(format!(
                            "- {} [{}] by {}\n  {}\n  {}",
                            r["title"].as_str().unwrap_or("?"),
                            r["type"].as_str().unwrap_or("?"),
                            r["owner"].as_str().unwrap_or("?"),
                            r["url"].as_str().unwrap_or("(no service URL)"),
                            short(&strip_html(r["snippet"].as_str().unwrap_or_default()), 200),
                        ));
                    }
                }
                Err(error) => out.push(format!("ArcGIS Online failed: {error:#}")),
            }
            ToolOutput::text(out.join("\n"))
        })
    }
}

pub struct ArcgisProbe(pub Arc<Work>);

/// What an ArcGIS REST endpoint holds, briefly: a directory's folders and services, a
/// service's layers, or a layer's fields, record limit, paging and formats.
pub async fn probe(work: &Work, url: &str) -> anyhow::Result<String> {
    let base = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    let body = work.runtime.http.get_json(&format!("{base}?f=json"), 8 << 20).await?;
    if let Some(error) = body.get("error") {
        anyhow::bail!("the server refused: {}", error["message"].as_str().unwrap_or("(no message)"));
    }
    let mut out = Vec::new();
    if body.get("fields").is_some() || body["type"] == "Feature Layer" {
        out.push(format!(
            "Layer: {} ({})",
            body["name"].as_str().unwrap_or("?"),
            body["geometryType"].as_str().unwrap_or("no geometry")
        ));
        out.push(format!("maxRecordCount: {}", body["maxRecordCount"]));
        out.push(format!(
            "supportsPagination: {}",
            body["advancedQueryCapabilities"]["supportsPagination"]
                .as_bool()
                .map_or("unknown".into(), |b| b.to_string())
        ));
        out.push(format!("supportedQueryFormats: {}", body["supportedQueryFormats"].as_str().unwrap_or("?")));
        out.push(format!("spatial reference: {}", body["extent"]["spatialReference"]));
        let count = work
            .runtime
            .http
            .get_json(&format!("{base}/query?where=1%3D1&returnCountOnly=true&f=json"), 1 << 20)
            .await
            .ok()
            .and_then(|v| v["count"].as_u64());
        out.push(format!("features: {}", count.map_or("unknown".into(), |c| c.to_string())));
        out.push("fields:".into());
        for field in body["fields"].as_array().into_iter().flatten().take(120) {
            out.push(format!(
                "  {} ({}) {}",
                field["name"].as_str().unwrap_or("?"),
                field["type"].as_str().unwrap_or("?").trim_start_matches("esriFieldType"),
                field["alias"].as_str().filter(|a| Some(*a) != field["name"].as_str()).unwrap_or_default()
            ));
        }
    } else if body.get("layers").is_some() || body.get("services").is_some() || body.get("folders").is_some() {
        if let Some(description) = body["serviceDescription"].as_str().filter(|d| !d.is_empty()) {
            out.push(format!("Service: {}", short(&strip_html(description), 300)));
        }
        for folder in body["folders"].as_array().into_iter().flatten() {
            out.push(format!("folder: {base}/{}", folder.as_str().unwrap_or("?")));
        }
        for service in body["services"].as_array().into_iter().flatten() {
            let name = service["name"].as_str().unwrap_or("?");
            let leaf = name.rsplit('/').next().unwrap_or(name);
            out.push(format!("service: {base}/{leaf}/{} ", service["type"].as_str().unwrap_or("?")));
        }
        for layer in body["layers"].as_array().into_iter().flatten() {
            out.push(format!(
                "layer: {base}/{} {} ({})",
                layer["id"],
                layer["name"].as_str().unwrap_or("?"),
                layer["geometryType"].as_str().unwrap_or("?")
            ));
        }
        if let Some(count) = body["maxRecordCount"].as_u64() {
            out.push(format!("maxRecordCount: {count}"));
        }
    } else {
        out.push(short(&body.to_string(), 3000));
    }
    Ok(out.join("\n"))
}

impl Tool for ArcgisProbe {
    fn name(&self) -> &str {
        "arcgis_probe"
    }
    fn description(&self) -> String {
        "Inspect an ArcGIS REST endpoint: a services directory (…/rest/services) lists folders and services, \
         a FeatureServer or MapServer lists its layers, and a layer (…/MapServer/3) lists its fields, \
         maxRecordCount, paging support, query formats and feature count."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]})
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let url = match text_arg(&args, "url") {
                Ok(url) => url,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("probe {url}"));
            match probe(&self.0, url).await {
                Ok(text) => ToolOutput::text(text),
                Err(error) => ToolOutput::error(format!("{error:#}")),
            }
        })
    }
}

/// The spec in a call's arguments, with the version filled in when left out.
fn spec_arg(args: &Value) -> Result<Spec, ToolOutput> {
    let mut value = args["spec"].clone();
    if let Some(text) = value.as_str() {
        value = serde_json::from_str(text).map_err(|e| ToolOutput::error(format!("spec is not JSON: {e}")))?;
    }
    if value.is_object() && value.get("version").is_none() {
        value["version"] = json!(SPEC_VERSION);
    }
    serde_json::from_value(value).map_err(|e| ToolOutput::error(format!("the spec does not read: {e}")))
}

fn spec_parameters(what: &str) -> Value {
    json!({"type": "object", "properties": {
        "spec": {"type": "object", "description": what},
    }, "required": ["spec"]})
}

pub struct Validate(pub Arc<Work>);

impl Tool for Validate {
    fn name(&self) -> &str {
        "validate"
    }
    fn description(&self) -> String {
        "Run a spec for real against your county (for a state, against three counties across it) and get \
         a report: each check, the parcels found, field fill rates, the source's attribute names and sample \
         attributes. Each failed validation uses one of your attempts."
            .into()
    }
    fn parameters(&self) -> Value {
        spec_parameters("The adapter spec, as described in your instructions.")
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let job = match job_of(&self.0, context) {
                Ok(job) => job,
                Err(e) => return e,
            };
            let spec = match spec_arg(&args) {
                Ok(spec) => spec,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("validate {}", spec.id));
            let (ok, reports) = self.0.validate_for(&job, &spec).await;
            let budget = self.0.validate_budget;
            let (failures, remaining) = self
                .0
                .with_agent(&context.agent, |a| {
                    for report in &reports {
                        a.note(report.summary());
                    }
                    if ok {
                        a.passed.insert(canonical(&spec));
                    } else {
                        a.failures += 1;
                    }
                    (a.failures, budget.saturating_sub(a.failures))
                })
                .unwrap_or((0, budget));
            let mut text = serde_json::to_string_pretty(&json!({"passed": ok, "reports": reports})).unwrap_or_default();
            if ok {
                text.push_str("\n\nThe spec passes. Call submit_spec with exactly this spec to save it.");
            } else if remaining == 0 {
                text.push_str(&format!(
                    "\n\nThat was your last of {budget} failed validations. Stop now: reply with a short summary of \
                     what you tried (URLs, layers, what failed) so the next attempt can start from it."
                ));
            } else {
                text.push_str(&format!("\n\n{failures} failed validations so far; {remaining} left."));
            }
            ToolOutput::text(text)
        })
    }
}

pub struct SubmitSpec(pub Arc<Work>);

impl Tool for SubmitSpec {
    fn name(&self) -> &str {
        "submit_spec"
    }
    fn description(&self) -> String {
        "Save a spec to the registry. Only a spec that passes validation is saved; this ends your job.".into()
    }
    fn parameters(&self) -> Value {
        spec_parameters("The spec that passed validation.")
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let job = match job_of(&self.0, context) {
                Ok(job) => job,
                Err(e) => return e,
            };
            let mut spec = match spec_arg(&args) {
                Ok(spec) => spec,
                Err(e) => return e,
            };
            activity(&self.0, context, format!("submit {}", spec.id));
            let (ok, reports) = self.0.validate_for(&job, &spec).await;
            if !ok {
                let summaries: Vec<String> = reports.iter().map(|r| r.summary()).collect();
                return ToolOutput::error(format!("Not saved: the spec does not pass.\n{}", summaries.join("\n")));
            }
            let (model, rung) = self.0.with_agent(&context.agent, |a| (a.model.clone(), a.rung)).unwrap_or_default();
            let at = now();
            spec.verification.model = Some(model);
            spec.verification.rung = Some(rung);
            spec.verification.verified_at = at;
            let passing: Vec<String> = reports.iter().filter(|r| r.passed).map(|r| r.fips.clone()).collect();
            spec.verification.counties = passing.clone();
            let saved = {
                let mut registry = self.0.registry.lock().unwrap();
                let saved = registry.save_adapter(spec);
                if let Ok(saved) = &saved {
                    for fips in &passing {
                        if let Err(error) = registry.verified(&saved.id, fips, at) {
                            return ToolOutput::error(format!("saving failed: {error:#}"));
                        }
                    }
                }
                saved
            };
            match saved {
                Ok(saved) => {
                    self.0.with_agent(&context.agent, |a| a.resolution = Some(Resolution::Covered(saved.id.clone())));
                    self.0.log(format!("{}: saved {} (verified for {})", job.label(), saved.id, passing.join(", ")));
                    ToolOutput::text(format!(
                        "Saved {}. Your job is done: reply with one line saying what you saved.",
                        saved.id
                    ))
                }
                Err(error) => ToolOutput::error(format!("{error:#}")),
            }
        })
    }
}

pub struct MarkUnavailable(pub Arc<Work>);

impl Tool for MarkUnavailable {
    fn name(&self) -> &str {
        "mark_unavailable"
    }
    fn description(&self) -> String {
        format!(
            "Record that your county has no usable public parcel service, with the reason and the evidence \
             (links to the viewer, price list, terms or login page). Reasons: {}. This ends your job; use it \
             only after a real search.",
            Reason::ALL.map(Reason::name).join(", ")
        )
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {
            "reason": {"type": "string", "enum": Reason::ALL.map(Reason::name)},
            "detail": {"type": "string", "description": "What you found, with links."},
        }, "required": ["reason", "detail"]})
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Ok(Job::County(fips)) = job_of(&self.0, context) else {
                return ToolOutput::error("only a county's job can mark it unavailable");
            };
            let Some(reason) = args["reason"].as_str().and_then(Reason::parse) else {
                return ToolOutput::error(format!(
                    "reason must be one of {}",
                    Reason::ALL.map(Reason::name).join(", ")
                ));
            };
            let detail = match text_arg(&args, "detail") {
                Ok(d) => d.to_owned(),
                Err(e) => return e,
            };
            let model = self.0.with_agent(&context.agent, |a| a.model.clone());
            let wait = match reason {
                Reason::Paywalled | Reason::TosForbidsAutomation => 365,
                _ => 180,
            };
            let at = now();
            let record = Unavailable {
                version: 1,
                fips: fips.clone(),
                reason,
                detail,
                recorded_at: at,
                recheck_after: at + wait * DAY_MS,
                model,
            };
            if let Err(error) = self.0.registry.lock().unwrap().mark_unavailable(record) {
                return ToolOutput::error(format!("{error:#}"));
            }
            self.0.with_agent(&context.agent, |a| a.resolution = Some(Resolution::Unavailable(reason)));
            self.0.log(format!("county {fips}: unavailable ({})", reason.name()));
            ToolOutput::text("Recorded. Your job is done: reply with one line.")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_read_as_text_with_links() {
        let html = r#"<html><head><style>p{}</style><script>var x="<a>";</script></head><body>
            <h1>Fulton&nbsp;County GIS</h1><p>Open <a href="/datasets/parcels">Tax Parcels</a> &amp; more</p>
            <table><tr><td>A</td><td>B</td></tr></table></body></html>"#;
        let text = readable(html, "https://gis.example.gov/hub/");
        assert_eq!(
            text,
            "Fulton County GIS\nOpen [Tax Parcels](https://gis.example.gov/datasets/parcels) & more\nA B\n"
        );
    }

    #[test]
    fn duckduckgo_results_are_read() {
        let html = r#"<div><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgis.fulton.gov%2Frest&amp;rut=1">Fulton <b>GIS</b></a>
            <a class="result__snippet" href="x">Tax <b>parcels</b> service</a></div>"#;
        assert_eq!(
            duckduckgo_results(html),
            [("Fulton GIS".into(), "https://gis.fulton.gov/rest".into(), "Tax parcels service".into())]
        );
    }

    #[test]
    fn long_text_comes_in_windows() {
        let text = "x".repeat(WINDOW + 10);
        assert!(window(&text, 0).starts_with(&format!("[characters 0–{WINDOW} of {}", WINDOW + 10)));
        assert_eq!(window("short", 0), "short");
    }
}
