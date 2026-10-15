//! A stand-in for the Codex Responses endpoint: each request is answered by a brain that reads
//! it (the model asked for, the briefing, the tool results so far) and decides the reply.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum Reply {
    Says(String),
    Calls(String, Value),
    /// Never answers, like a model call still in flight.
    Hangs,
}

pub type Brain = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// What the brain sees of a request.
pub struct Request {
    pub model: String,
    pub instructions: String,
    /// The user messages, in order.
    pub user: Vec<String>,
    /// Tool results, in order.
    pub outputs: Vec<String>,
}

impl Request {
    pub fn last_output(&self) -> &str {
        self.outputs.last().map_or("", String::as_str)
    }
    pub fn briefing(&self) -> &str {
        self.user.first().map_or("", String::as_str)
    }
}

#[derive(Clone)]
struct Shared {
    brain: Brain,
    requests: Arc<Mutex<Vec<Request>>>,
}

pub struct Model {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

fn read(body: &Value) -> Request {
    let mut user = Vec::new();
    let mut outputs = Vec::new();
    for item in body["input"].as_array().into_iter().flatten() {
        if item["role"] == "user" {
            for part in item["content"].as_array().into_iter().flatten() {
                if let Some(text) = part["text"].as_str() {
                    user.push(text.to_owned());
                }
            }
        } else if item["type"] == "function_call_output" {
            outputs.push(item["output"].as_str().unwrap_or_default().to_owned());
        }
    }
    Request {
        model: body["model"].as_str().unwrap_or_default().to_owned(),
        instructions: body["instructions"].as_str().unwrap_or_default().to_owned(),
        user,
        outputs,
    }
}

async fn respond(State(shared): State<Shared>, body: String) -> Response {
    let request = read(&serde_json::from_str(&body).unwrap());
    let reply = (shared.brain)(&request);
    shared.requests.lock().unwrap().push(request);
    let completed = json!({"type": "response.completed", "response": {"usage": {"input_tokens": 100, "output_tokens": 20, "input_tokens_details": {"cached_tokens": 50}}}});
    let item = match reply {
        Reply::Hangs => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            return ().into_response();
        }
        Reply::Says(text) => {
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
        }
        Reply::Calls(name, args) => {
            static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            json!({"type": "function_call", "call_id": format!("call-{id}"), "name": name, "arguments": args.to_string()})
        }
    };
    let events = [json!({"type": "response.output_item.done", "item": item}), completed];
    let body: String =
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
    ([("content-type", "text/event-stream")], body).into_response()
}

impl Model {
    pub async fn start(brain: Brain) -> Self {
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let app = axum::Router::new()
            .route("/responses", axum::routing::post(respond))
            .with_state(Shared { brain, requests: requests.clone() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, requests }
    }
}
