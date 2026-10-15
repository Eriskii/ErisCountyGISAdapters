//! The models agents run on and the providers that serve them: a ChatGPT login's models through
//! the Codex endpoint, and a Claude login's through the Anthropic API as Claude Code calls it.
//! Both logins come from ErisAgentAuth. The ladder of models runs cheapest first.

use anyhow::{Result, bail, ensure};
use erisagentauth::claude::{self, ClaudeAccount};
use erisagentauth::openai::OpenAiAccount;
use erisharness::provider::{
    Anthropic, AnthropicAuth, AnthropicConfig, Provider, RateGate, Responses, ResponsesConfig, Thinking,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// The ChatGPT login's models, through the Codex endpoint.
pub const CODEX: &str = "codex";
/// The Claude login's models, through the Anthropic API.
pub const ANTHROPIC: &str = "anthropic";
/// The backend lists models by the Codex client version asking; this asks as a current one.
const CLIENT_VERSION: &str = "99.0.0";
/// Claude model families a ladder may name, each meaning the newest listed model of it.
const CLAUDE_FAMILIES: [&str; 3] = ["haiku", "sonnet", "opus"];
/// The efforts a Claude model may list, in order.
const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    /// [`CODEX`] or [`ANTHROPIC`].
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// Reasoning efforts the model takes, in the backend's order.
    pub efforts: Vec<String>,
    /// Why agents cannot run on this model, when they cannot.
    pub cannot_run: Option<String>,
}

/// One rung of the ladder: a model, who serves it, and how hard it thinks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rung {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub context_window: Option<u64>,
}

impl Rung {
    pub fn label(&self) -> String {
        match &self.effort {
            Some(effort) => format!("{} ({effort})", self.model),
            None => self.model.clone(),
        }
    }
}

/// Where the providers are: the Codex endpoint, and the Anthropic API's root (without `/v1`).
pub struct Endpoints {
    pub codex: String,
    pub claude_api: String,
}

/// The ChatGPT login's models, most preferred first.
pub async fn list_codex(account: &OpenAiAccount, endpoint: &str, originator: &str) -> Result<Vec<Model>> {
    let credential = account.current().await?;
    let response = reqwest::Client::new()
        .get(format!("{}/models?client_version={CLIENT_VERSION}", endpoint.trim_end_matches('/')))
        .bearer_auth(&credential.access)
        .header("chatgpt-account-id", &credential.account_id)
        .header("originator", originator)
        .send()
        .await?;
    let status = response.status();
    ensure!(status.is_success(), "ChatGPT did not list its models ({status})");
    Ok(codex_models(&response.json().await?))
}

/// The Claude login's models, newest first, from the Anthropic API at `api`.
pub async fn list_claude(account: &ClaudeAccount, api: &str) -> Result<Vec<Model>> {
    let login = account.current().await?;
    let client = reqwest::Client::new();
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut request = client
            .get(format!("{}/v1/models", api.trim_end_matches('/')))
            .query(&[("limit", "1000")])
            .bearer_auth(&login.access)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", claude::OAUTH_BETA);
        if let Some(after) = &after {
            request = request.query(&[("after_id", after)]);
        }
        let response = request.send().await?;
        let status = response.status();
        ensure!(status.is_success(), "Anthropic did not list the Claude login's models ({status})");
        let page: Value = response.json().await?;
        models.extend(claude_models(&page));
        match page["last_id"].as_str() {
            Some(last) if page["has_more"] == true => after = Some(last.to_owned()),
            _ => return Ok(models),
        }
    }
}

fn codex_models(body: &Value) -> Vec<Model> {
    body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            let id = m["slug"].as_str()?;
            Some(Model {
                provider: CODEX.into(),
                id: id.to_owned(),
                name: m["display_name"].as_str().unwrap_or(id).to_owned(),
                context_window: m["context_window"].as_u64(),
                efforts: m["supported_reasoning_levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["effort"].as_str().map(str::to_owned))
                    .collect(),
                cannot_run: None,
            })
        })
        .collect()
}

/// The Claude models a listing names. Agents run on them with adaptive thinking (see
/// [`providers`]), at the efforts each model's capabilities list; a model that thinks only on a
/// budget cannot run.
fn claude_models(body: &Value) -> Vec<Model> {
    let supported = |capability: &Value| capability["supported"] == true;
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let capabilities = &m["capabilities"];
            let adaptive = supported(&capabilities["thinking"]["types"]["adaptive"]);
            let effort = &capabilities["effort"];
            Some(Model {
                provider: ANTHROPIC.into(),
                id: id.to_owned(),
                name: m["display_name"].as_str().unwrap_or(id).to_owned(),
                context_window: m["max_input_tokens"].as_u64(),
                efforts: CLAUDE_EFFORTS
                    .iter()
                    .filter(|level| adaptive && supported(effort) && supported(&effort[**level]))
                    .map(|level| level.to_string())
                    .collect(),
                cannot_run: (!adaptive)
                    .then(|| format!("{id} does not think adaptively, and agents run on adaptive thinking")),
            })
        })
        .collect()
}

fn small(model: &Model) -> bool {
    ["mini", "nano", "spark", "small", "lite"].iter().any(|w| model.id.contains(w))
}

/// The newest listed Claude model of `family` (haiku, sonnet or opus).
fn newest_claude<'a>(models: &'a [Model], family: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.provider == ANTHROPIC && m.cannot_run.is_none() && m.id.contains(family))
}

fn rung(model: &Model, effort: Option<&str>) -> Rung {
    Rung {
        provider: model.provider.clone(),
        model: model.id.clone(),
        effort: effort.filter(|e| model.efforts.iter().any(|m| m == e)).map(str::to_owned),
        context_window: model.context_window,
    }
}

/// The cheapest-first ladder from the signed-in logins' models. With Claude: the newest Haiku at
/// medium effort, then Sonnet at high, then ChatGPT's flagship at high, or Opus at high without
/// ChatGPT. With ChatGPT
/// alone: its most preferred small model at medium effort, then its flagship at medium and high.
pub fn default_ladder(models: &[Model]) -> Vec<Rung> {
    let codex: Vec<&Model> = models.iter().filter(|m| m.provider == CODEX).collect();
    let flagship = codex.iter().find(|m| !small(m));
    let mut ladder = Vec::new();
    if models.iter().any(|m| m.provider == ANTHROPIC) {
        ladder.extend(newest_claude(models, "haiku").map(|m| rung(m, Some("medium"))));
        ladder.extend(newest_claude(models, "sonnet").map(|m| rung(m, Some("high"))));
        match flagship {
            Some(m) => ladder.push(rung(m, Some("high"))),
            None => ladder.extend(newest_claude(models, "opus").map(|m| rung(m, Some("high")))),
        }
        return ladder;
    }
    if let Some(m) = codex.iter().find(|m| small(m)) {
        ladder.push(rung(m, Some("medium")));
    }
    if let Some(m) = flagship {
        ladder.push(rung(m, Some("medium")));
        if m.efforts.iter().any(|e| e == "high") {
            ladder.push(rung(m, Some("high")));
        }
    }
    ladder
}

/// A ladder written as `model[:effort],model[:effort],…`. `haiku`, `sonnet` and `opus` mean the
/// newest listed Claude model of that family. A listed model must be one agents can run on, and
/// take the effort named. A model the listing names is served by its
/// provider; an unlisted one by Anthropic when its id starts with `claude`, else by Codex.
pub fn parse_ladder(text: &str, models: &[Model]) -> Result<Vec<Rung>> {
    let mut ladder = Vec::new();
    for part in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (name, effort) = match part.split_once(':') {
            Some((m, e)) => (m, Some(e.to_owned())),
            None => (part, None),
        };
        let listed = if CLAUDE_FAMILIES.contains(&name) {
            match newest_claude(models, name) {
                Some(model) => Some(model),
                None => bail!("no Claude {name} model is listed; bind a Claude login (login --claude)"),
            }
        } else {
            models.iter().find(|m| m.id == name)
        };
        let provider = match listed {
            Some(model) => model.provider.clone(),
            None if name.starts_with("claude") => ANTHROPIC.into(),
            None => CODEX.into(),
        };
        if let Some(model) = listed {
            if let Some(reason) = &model.cannot_run {
                bail!("{part}: {reason}");
            }
            if let Some(effort) = &effort
                && !model.efforts.contains(effort)
            {
                bail!("{part}: {} takes the efforts {}, not {effort}", model.id, model.efforts.join(", "));
            }
        }
        ladder.push(Rung {
            provider,
            model: listed.map_or(name, |m| m.id.as_str()).to_owned(),
            effort,
            context_window: listed.and_then(|m| m.context_window),
        });
    }
    Ok(ladder)
}

/// The providers agents run on, each authorized by its ErisAgentAuth account on every request:
/// [`CODEX`] with the ChatGPT login, and [`ANTHROPIC`] with the Claude login, calling as Claude
/// Code does.
pub async fn providers(
    openai: &OpenAiAccount,
    claude: &ClaudeAccount,
    endpoints: &Endpoints,
    originator: &str,
) -> Result<Vec<(&'static str, Arc<dyn Provider>)>> {
    let codex = Responses::new(ResponsesConfig {
        base_url: endpoints.codex.clone(),
        headers: vec![
            ("OpenAI-Beta".into(), "responses=experimental".into()),
            ("originator".into(), originator.into()),
        ],
        store: false,
        max_retries: 6,
        credentials: Arc::new(openai.clone()),
        gate: RateGate::new(16, 256),
    });
    let mut config =
        AnthropicConfig::new(Arc::new(claude.clone()), AnthropicAuth::ClaudeCode(claude.claude_code().await));
    config.base_url = format!("{}/v1", endpoints.claude_api.trim_end_matches('/'));
    config.max_retries = 6;
    // Each agent's reasoning effort reaches Claude as the effort of its adaptive thinking.
    config.thinking = Thinking::Adaptive { display: false };
    Ok(vec![(CODEX, Arc::new(codex)), (ANTHROPIC, Arc::new(Anthropic::new(config)?))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codex() -> Vec<Model> {
        codex_models(&json!({"models": [
            {"slug": "gpt-9-codex", "visibility": "list", "context_window": 400000, "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]},
            {"slug": "gpt-9-codex-mini", "visibility": "list", "context_window": 200000, "supported_reasoning_levels": [{"effort": "medium"}]},
            {"slug": "hidden", "visibility": "hide"},
        ]}))
    }

    /// A model as Anthropic lists it, thinking adaptively at `efforts`, or only on a budget when
    /// `efforts` is `None`.
    fn listed(id: &str, efforts: Option<&[&str]>) -> Value {
        let levels: serde_json::Map<String, Value> = ["low", "medium", "high", "xhigh", "max"]
            .iter()
            .map(|l| (l.to_string(), json!({"supported": efforts.is_some_and(|e| e.contains(l))})))
            .collect();
        json!({"id": id, "max_input_tokens": 200000, "capabilities": {
            "effort": {"supported": efforts.is_some(), "low": levels["low"], "medium": levels["medium"],
                "high": levels["high"], "xhigh": levels["xhigh"], "max": levels["max"]},
            "thinking": {"supported": true, "types": {
                "adaptive": {"supported": efforts.is_some()}, "enabled": {"supported": true}}}}})
    }

    const ALL: &[&str] = &["low", "medium", "high", "xhigh", "max"];

    fn claude() -> Vec<Model> {
        claude_models(&json!({"data": [
            listed("claude-sonnet-9", Some(ALL)),
            listed("claude-haiku-9", Some(ALL)),
            listed("claude-haiku-8", None),
            listed("claude-opus-9", Some(&["low", "medium", "high"])),
        ]}))
    }

    fn labels(ladder: &[Rung]) -> Vec<String> {
        ladder.iter().map(|r| format!("{}/{}", r.provider, r.label())).collect()
    }

    #[test]
    fn with_chatgpt_alone_the_ladder_climbs_from_the_small_model_to_the_flagship() {
        let ladder = default_ladder(&codex());
        assert_eq!(
            labels(&ladder),
            ["codex/gpt-9-codex-mini (medium)", "codex/gpt-9-codex (medium)", "codex/gpt-9-codex (high)"]
        );
        assert_eq!(ladder[0].context_window, Some(200000));
    }

    #[test]
    fn with_claude_the_ladder_starts_on_haiku() {
        assert_eq!(
            labels(&default_ladder(&claude())),
            ["anthropic/claude-haiku-9 (medium)", "anthropic/claude-sonnet-9 (high)", "anthropic/claude-opus-9 (high)"]
        );
        let both: Vec<Model> = claude().into_iter().chain(codex()).collect();
        assert_eq!(
            labels(&default_ladder(&both)),
            ["anthropic/claude-haiku-9 (medium)", "anthropic/claude-sonnet-9 (high)", "codex/gpt-9-codex (high)"]
        );
    }

    #[test]
    fn a_written_ladder_names_models_by_id_or_claude_family() {
        let both: Vec<Model> = claude().into_iter().chain(codex()).collect();
        let ladder = parse_ladder("haiku:low, sonnet, gpt-9-codex:high, claude-new:max", &both).unwrap();
        assert_eq!(
            labels(&ladder),
            [
                "anthropic/claude-haiku-9 (low)",
                "anthropic/claude-sonnet-9",
                "codex/gpt-9-codex (high)",
                "anthropic/claude-new (max)"
            ]
        );
        assert_eq!(ladder[0].context_window, Some(200000));
        assert!(parse_ladder("opus", &codex()).unwrap_err().to_string().contains("no Claude opus"));
    }

    #[test]
    fn a_rung_takes_only_an_effort_its_model_lists() {
        let both: Vec<Model> = claude().into_iter().chain(codex()).collect();
        let error = parse_ladder("opus:max", &both).unwrap_err().to_string();
        assert!(error.contains("low, medium, high"), "{error}");
        assert!(parse_ladder("gpt-9-codex-mini:high", &both).is_err());
        let error = parse_ladder("claude-haiku-8", &both).unwrap_err().to_string();
        assert!(error.contains("adaptive"), "a model that only thinks on a budget cannot run: {error}");
    }
}
