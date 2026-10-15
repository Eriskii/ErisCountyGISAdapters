//! The models agents run on and the providers that serve them: a ChatGPT login's models through
//! the Codex endpoint, and a Claude login's through the Anthropic API as Claude Code calls it.
//! Both logins come from ErisAgentAuth. The ladder of models runs cheapest first.

use anyhow::{Result, bail, ensure};
use erisagentauth::claude::{self, ClaudeAccount};
use erisagentauth::openai::OpenAiAccount;
use erisharness::provider::{
    Anthropic, AnthropicAuth, AnthropicConfig, Provider, RateGate, Responses, ResponsesConfig,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    /// [`CODEX`] or [`ANTHROPIC`].
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// Reasoning efforts the model takes, in the backend's order.
    pub efforts: Vec<String>,
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
            })
        })
        .collect()
}

/// Claude models take no effort here: ErisHarness's Anthropic provider sets thinking for the
/// whole provider, and this one runs with thinking off.
fn claude_models(body: &Value) -> Vec<Model> {
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            Some(Model {
                provider: ANTHROPIC.into(),
                id: id.to_owned(),
                name: m["display_name"].as_str().unwrap_or(id).to_owned(),
                context_window: m["max_input_tokens"].as_u64(),
                efforts: Vec::new(),
            })
        })
        .collect()
}

fn small(model: &Model) -> bool {
    ["mini", "nano", "spark", "small", "lite"].iter().any(|w| model.id.contains(w))
}

/// The newest listed Claude model of `family` (haiku, sonnet or opus).
fn newest_claude<'a>(models: &'a [Model], family: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.provider == ANTHROPIC && m.id.contains(family))
}

fn rung(model: &Model, effort: Option<&str>) -> Rung {
    Rung {
        provider: model.provider.clone(),
        model: model.id.clone(),
        effort: effort.filter(|e| model.efforts.iter().any(|m| m == e)).map(str::to_owned),
        context_window: model.context_window,
    }
}

/// The cheapest-first ladder from the signed-in logins' models. With Claude: the newest Haiku,
/// then Sonnet, then ChatGPT's flagship at high effort, or Opus without ChatGPT. With ChatGPT
/// alone: its most preferred small model at medium effort, then its flagship at medium and high.
pub fn default_ladder(models: &[Model]) -> Vec<Rung> {
    let codex: Vec<&Model> = models.iter().filter(|m| m.provider == CODEX).collect();
    let flagship = codex.iter().find(|m| !small(m));
    let mut ladder = Vec::new();
    if models.iter().any(|m| m.provider == ANTHROPIC) {
        ladder.extend(["haiku", "sonnet"].into_iter().filter_map(|f| newest_claude(models, f)).map(|m| rung(m, None)));
        match flagship {
            Some(m) => ladder.push(rung(m, Some("high"))),
            None => ladder.extend(newest_claude(models, "opus").map(|m| rung(m, None))),
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
/// newest listed Claude model of that family. A model the listing names is served by its
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
        if provider == ANTHROPIC && effort.is_some() {
            bail!("{part}: Claude rungs take no effort; ErisHarness's Anthropic provider runs them with thinking off");
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

    fn claude() -> Vec<Model> {
        claude_models(&json!({"data": [
            {"id": "claude-sonnet-9", "display_name": "Claude Sonnet 9", "max_input_tokens": 1000000},
            {"id": "claude-haiku-9", "max_input_tokens": 200000},
            {"id": "claude-haiku-8", "max_input_tokens": 200000},
            {"id": "claude-opus-9", "max_input_tokens": 1000000},
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
            ["anthropic/claude-haiku-9", "anthropic/claude-sonnet-9", "anthropic/claude-opus-9"]
        );
        let both: Vec<Model> = claude().into_iter().chain(codex()).collect();
        assert_eq!(
            labels(&default_ladder(&both)),
            ["anthropic/claude-haiku-9", "anthropic/claude-sonnet-9", "codex/gpt-9-codex (high)"]
        );
    }

    #[test]
    fn a_written_ladder_names_models_by_id_or_claude_family() {
        let both: Vec<Model> = claude().into_iter().chain(codex()).collect();
        let ladder = parse_ladder("haiku, claude-haiku-8, gpt-9-codex:high, claude-new", &both).unwrap();
        assert_eq!(
            labels(&ladder),
            [
                "anthropic/claude-haiku-9",
                "anthropic/claude-haiku-8",
                "codex/gpt-9-codex (high)",
                "anthropic/claude-new"
            ]
        );
        assert_eq!(ladder[0].context_window, Some(200000));
        assert!(parse_ladder("sonnet:high", &both).unwrap_err().to_string().contains("take no effort"));
        assert!(parse_ladder("opus", &codex()).unwrap_err().to_string().contains("no Claude opus"));
    }
}
