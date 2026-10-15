//! The models a ChatGPT login offers through the Codex endpoint, and the default model
//! ladder made from them: the smallest models first, then the flagship.

use anyhow::{Result, ensure};
use erisagentauth::openai::OpenAiAccount;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The backend lists models by the Codex client version asking; this asks as a current one.
const CLIENT_VERSION: &str = "99.0.0";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// Reasoning efforts the model takes, in the backend's order.
    pub efforts: Vec<String>,
}

/// One rung of the ladder: a model and how hard it thinks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rung {
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

/// The signed-in account's models, most preferred first.
pub async fn list(account: &OpenAiAccount, endpoint: &str, originator: &str) -> Result<Vec<Model>> {
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
    Ok(parse(&response.json().await?))
}

fn parse(body: &Value) -> Vec<Model> {
    body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            let id = m["slug"].as_str()?;
            Some(Model {
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

fn small(model: &Model) -> bool {
    ["mini", "nano", "spark", "small", "lite"].iter().any(|w| model.id.contains(w))
}

/// The cheapest-first ladder: the most preferred small model at medium effort, then the most
/// preferred full model at medium, then at high.
pub fn default_ladder(models: &[Model]) -> Vec<Rung> {
    let rung = |m: &Model, effort: &str| Rung {
        model: m.id.clone(),
        effort: m.efforts.iter().any(|e| e == effort).then(|| effort.to_owned()),
        context_window: m.context_window,
    };
    let mut ladder = Vec::new();
    if let Some(m) = models.iter().find(|m| small(m)) {
        ladder.push(rung(m, "medium"));
    }
    if let Some(m) = models.iter().find(|m| !small(m)) {
        ladder.push(rung(m, "medium"));
        if m.efforts.iter().any(|e| e == "high") {
            ladder.push(rung(m, "high"));
        }
    }
    ladder
}

/// A ladder written as `model[:effort],model[:effort],…`, with context windows from the
/// listing where it names the model.
pub fn parse_ladder(text: &str, models: &[Model]) -> Vec<Rung> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|part| {
            let (model, effort) = match part.split_once(':') {
                Some((m, e)) => (m.to_owned(), Some(e.to_owned())),
                None => (part.to_owned(), None),
            };
            let context_window = models.iter().find(|m| m.id == model).and_then(|m| m.context_window);
            Rung { model, effort, context_window }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_ladder_climbs_from_the_small_model_to_the_flagship() {
        let models = parse(&json!({"models": [
            {"slug": "gpt-9-codex", "visibility": "list", "context_window": 400000, "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]},
            {"slug": "gpt-9-codex-mini", "visibility": "list", "context_window": 200000, "supported_reasoning_levels": [{"effort": "medium"}]},
            {"slug": "hidden", "visibility": "hide"},
        ]}));
        let ladder = default_ladder(&models);
        let labels: Vec<String> = ladder.iter().map(Rung::label).collect();
        assert_eq!(labels, ["gpt-9-codex-mini (medium)", "gpt-9-codex (medium)", "gpt-9-codex (high)"]);
        assert_eq!(ladder[0].context_window, Some(200000));
        let custom = parse_ladder("gpt-9-codex-mini, gpt-9-codex:high", &models);
        assert_eq!(
            custom[1],
            Rung { model: "gpt-9-codex".into(), effort: Some("high".into()), context_window: Some(400000) }
        );
    }
}
