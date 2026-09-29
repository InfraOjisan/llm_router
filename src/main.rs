use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{collections::{HashMap, HashSet}, env, error::Error, fs, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, OnceCell};

#[derive(Clone, Deserialize)]
struct Config {
    listen: String,
    #[serde(default = "default_max_body")]
    max_body_bytes: usize,
    decision: Option<DecisionConfig>,
    endpoints: HashMap<String, Endpoint>,
    routes: Vec<RouteConfig>,
    multimodal_route: Option<String>,
    fallback_route: String,
}

fn default_max_body() -> usize { 10 * 1024 * 1024 }

#[derive(Clone, Deserialize)]
struct DecisionConfig {
    url: String,
    model: Option<String>,
    #[serde(default = "default_decision_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    min_confidence: f64,
    api_key_env: Option<String>,
}

fn default_decision_timeout() -> u64 { 1500 }

#[derive(Clone, Deserialize)]
struct Endpoint {
    base_url: String,
    model: String,
    api_key_env: Option<String>,
}

#[derive(Clone, Deserialize)]
struct RouteConfig {
    id: String,
    condition: String,
    endpoint: String,
}

impl Config {
    fn load(path: &str) -> Result<Self, Box<dyn Error>> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.validate().map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        self.listen.parse::<SocketAddr>().map_err(|_| "listen must be an IP:port socket address")?;
        if self.max_body_bytes == 0 { return Err("max_body_bytes must be positive".into()); }
        if self.routes.is_empty() { return Err("at least one route is required".into()); }
        let mut seen = HashSet::new();
        for route in &self.routes {
            if route.id.is_empty() || !route.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(format!("invalid route id: {}", route.id));
            }
            if !seen.insert(route.id.as_str()) { return Err(format!("duplicate route id: {}", route.id)); }
            if route.condition.trim().is_empty() { return Err(format!("empty condition: {}", route.id)); }
            if !self.endpoints.contains_key(&route.endpoint) { return Err(format!("unknown endpoint for route: {}", route.id)); }
        }
        if !seen.contains(self.fallback_route.as_str()) {
            return Err(format!("unknown fallback route: {}", self.fallback_route));
        }
        if let Some(route) = &self.multimodal_route {
            if !seen.contains(route.as_str()) { return Err(format!("unknown multimodal route: {route}")); }
        }
        if let Some(decision) = &self.decision {
            validate_url(&decision.url)?;
            if decision.timeout_ms == 0 { return Err("decision.timeout_ms must be positive".into()); }
            if !(0.0..=1.0).contains(&decision.min_confidence) { return Err("decision.min_confidence must be between 0 and 1".into()); }
        }
        for (name, endpoint) in &self.endpoints {
            validate_url(&endpoint.base_url)?;
            if endpoint.model.trim().is_empty() { return Err(format!("empty model for endpoint: {name}")); }
        }
        Ok(())
    }

    fn route(&self, id: &str) -> Option<&RouteConfig> { self.routes.iter().find(|route| route.id == id) }
}

fn validate_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| format!("invalid URL: {value}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return Err(format!("URL must be an HTTP(S) URL without query or fragment: {value}"));
    }
    Ok(())
}

#[derive(Hash, Eq, PartialEq)]
struct SessionKey { harness: String, session: String }

#[derive(Clone)]
struct PinnedRoute { id: String, source: &'static str }

struct AppState {
    config: Config,
    client: reqwest::Client,
    sessions: Mutex<HashMap<SessionKey, Arc<OnceCell<PinnedRoute>>>>,
}

#[derive(Default)]
struct TaskInfo { text: String, has_image: bool, has_pdf: bool }

fn append_text(out: &mut String, text: &str) {
    if !out.is_empty() { out.push('\n'); }
    out.push_str(text);
}

fn inspect_content(value: &Value, task: &mut TaskInfo) {
    match value {
        Value::String(text) => append_text(&mut task.text, text),
        Value::Array(parts) => for part in parts { inspect_content(part, task); },
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "image_url" | "input_image" | "image") || object.contains_key("image_url") {
                task.has_image = true;
            }
            if matches!(kind, "input_file" | "file" | "document")
                || object.get("mime_type").and_then(Value::as_str) == Some("application/pdf")
                || object.get("filename").and_then(Value::as_str).is_some_and(|name| name.to_ascii_lowercase().ends_with(".pdf")) {
                task.has_pdf = true;
            }
            for key in ["text", "content", "input_text", "prompt"] {
                if let Some(part) = object.get(key) { inspect_content(part, task); }
            }
        }
        _ => {}
    }
}

fn task_info(body: &Value) -> TaskInfo {
    let mut task = TaskInfo::default();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        if let Some(message) = messages.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("user")) {
            inspect_content(&message["content"], &mut task);
        }
    } else if let Some(input) = body.get("input") {
        inspect_content(input, &mut task);
    } else if let Some(prompt) = body.get("prompt") {
        inspect_content(prompt, &mut task);
    }
    task
}

fn clipped_text(text: &str) -> String {
    const LIMIT: usize = 12_000;
    let count = text.chars().count();
    if count <= LIMIT { return text.to_owned(); }
    let head: String = text.chars().take(LIMIT / 2).collect();
    let tail: String = text.chars().skip(count - LIMIT / 2).collect();
    format!("{head}\n[... middle omitted ...]\n{tail}")
}

impl AppState {
    async fn choose(&self, task: &TaskInfo) -> PinnedRoute {
        if task.has_image || task.has_pdf {
            if let Some(id) = &self.config.multimodal_route {
                return PinnedRoute { id: id.clone(), source: "multimodal" };
            }
        }
        if let Some(decision) = &self.config.decision {
            if let Some(id) = self.classify(decision, task).await {
                return PinnedRoute { id, source: "decision" };
            }
        }
        PinnedRoute { id: self.config.fallback_route.clone(), source: "fallback" }
    }

    async fn classify(&self, config: &DecisionConfig, task: &TaskInfo) -> Option<String> {
        let criteria: Map<String, Value> = self.config.routes.iter()
            .map(|route| (route.id.clone(), Value::String(route.condition.clone()))).collect();
        let state = format!("Latest user request: {}\nInput characters: {}\nHas image: {}\nHas PDF: {}",
            clipped_text(&task.text), task.text.chars().count(), task.has_image, task.has_pdf);
        let mut payload = json!({
            "state": { "body": state },
            "questions": { "route": {
                "type": "choice",
                "instructions": "Choose the best execution route for this request. Use the task requirements, not just keywords.",
                "criteria": criteria
            }}
        });
        if let Some(model) = &config.model { payload["model"] = Value::String(model.clone()); }
        let mut request = self.client.post(config.url.as_str()).timeout(Duration::from_millis(config.timeout_ms)).json(&payload);
        if let Some(env_name) = &config.api_key_env {
            let key = env::var(env_name).ok()?;
            request = request.bearer_auth(key);
        }
        let response: Value = request.send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
        let answer = response.get("answers")?.get("route")?;
        let id = answer.get("choice")?.as_str()?;
        if self.config.route(id).is_none() { return None; }
        let confidence = answer.get("answer_confidence").and_then(Value::as_f64).unwrap_or(0.0);
        if confidence < config.min_confidence { return None; }
        Some(id.to_owned())
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message, "type": "router_error"}}))).into_response()
}

fn required_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 256 { return None; }
    Some(value.to_owned())
}

async fn chat(State(state): State<Arc<AppState>>, request: Request) -> Response {
    proxy(state, request, "chat/completions").await
}

async fn responses(State(state): State<Arc<AppState>>, request: Request) -> Response {
    proxy(state, request, "responses").await
}

async fn proxy(state: Arc<AppState>, request: Request, path: &str) -> Response {
    let (parts, body) = request.into_parts();
    let Some(harness) = required_header(&parts.headers, "x-harness-id") else {
        return error_response(StatusCode::BAD_REQUEST, "x-harness-id header is required (1-256 characters)");
    };
    let Some(session) = required_header(&parts.headers, "x-session-id") else {
        return error_response(StatusCode::BAD_REQUEST, "x-session-id header is required (1-256 characters)");
    };
    let bytes = match to_bytes(body, state.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds max_body_bytes"),
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => return error_response(StatusCode::BAD_REQUEST, "request body must be a JSON object"),
    };
    let task = task_info(&payload);
    let key = SessionKey { harness, session };
    let cell = {
        let mut sessions = state.sessions.lock().await;
        sessions.entry(key).or_insert_with(|| Arc::new(OnceCell::new())).clone()
    };
    let already_pinned = cell.get().is_some();
    let selected = cell.get_or_init(|| state.choose(&task)).await;
    let route = state.config.route(&selected.id).expect("validated route");
    let endpoint = state.config.endpoints.get(&route.endpoint).expect("validated endpoint");
    payload["model"] = Value::String(endpoint.model.clone());
    let target_url = format!("{}/{}", endpoint.base_url.trim_end_matches('/'), path);
    let mut outgoing = state.client.post(target_url).header(header::CONTENT_TYPE, "application/json")
        .json(&payload);
    if let Some(accept) = parts.headers.get(header::ACCEPT) { outgoing = outgoing.header(header::ACCEPT, accept.clone()); }
    if let Some(env_name) = &endpoint.api_key_env {
        let Ok(key) = env::var(env_name) else {
            return error_response(StatusCode::BAD_GATEWAY, "upstream API key environment variable is missing");
        };
        outgoing = outgoing.bearer_auth(key);
    }
    let upstream = match outgoing.send().await {
        Ok(response) => response,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY, "upstream request failed"),
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    if let Some(content_type) = upstream.headers().get(header::CONTENT_TYPE) {
        response = response.header(header::CONTENT_TYPE, content_type.clone());
    }
    if let Some(cache_control) = upstream.headers().get(header::CACHE_CONTROL) {
        response = response.header(header::CACHE_CONTROL, cache_control.clone());
    }
    response = response.header("x-router-route", selected.id.as_str())
        .header("x-router-source", if already_pinned { "pinned" } else { selected.source });
    response.body(Body::from_stream(upstream.bytes_stream())).unwrap_or_else(|_| {
        error_response(StatusCode::BAD_GATEWAY, "failed to build upstream response")
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let config_path = match args.as_slice() {
        [_] => "config.json",
        [_, path] => path,
        [_, flag, path] if flag == "--check" => {
            Config::load(path)?;
            println!("configuration OK");
            return Ok(());
        }
        _ => {
            eprintln!("usage: hispeed-router [config.json] | --check config.json");
            std::process::exit(2);
        }
    };
    let config = Config::load(config_path)?;
    let listen = config.listen.clone();
    let state = Arc::new(AppState {
        config,
        client: reqwest::Client::builder().pool_max_idle_per_host(32).build()?,
        sessions: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    println!("hispeed-router listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_multimodal_without_copying_file_data_into_classifier_text() {
        let body = json!({"messages": [{"role":"user", "content": [
            {"type":"text", "text":"Please read this"},
            {"type":"input_file", "filename":"scan.pdf", "file_data":"AAAA"},
            {"type":"image_url", "image_url":{"url":"data:image/png;base64,BBBB"}}
        ]}]});
        let task = task_info(&body);
        assert!(task.has_image && task.has_pdf);
        assert_eq!(task.text, "Please read this");
    }

    #[test]
    fn takes_latest_user_message_for_routing() {
        let body = json!({"messages": [
            {"role":"user", "content":"Summarize"},
            {"role":"assistant", "content":"Done"},
            {"role":"user", "content":"Design the system"}
        ]});
        assert_eq!(task_info(&body).text, "Design the system");
    }

    #[test]
    fn clips_unicode_without_breaking_characters() {
        let input = "あ".repeat(13_000);
        let result = clipped_text(&input);
        assert!(result.starts_with("ああ") && result.ends_with("ああ"));
        assert!(result.contains("middle omitted"));
    }

    #[tokio::test]
    async fn multimodal_override_and_fallback_do_not_need_decision_server() {
        let mut config: Config = serde_json::from_str(include_str!("../config.example.json")).unwrap();
        config.validate().unwrap();
        config.decision = None;
        let state = AppState {
            config,
            client: reqwest::Client::new(),
            sessions: Mutex::new(HashMap::new()),
        };
        let multimodal = TaskInfo { has_image: true, ..TaskInfo::default() };
        assert_eq!(state.choose(&multimodal).await.id, "ocr");
        assert_eq!(state.choose(&TaskInfo::default()).await.id, "deep");
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_initial_decision() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cell = Arc::new(OnceCell::<PinnedRoute>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let cell = cell.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                cell.get_or_init(|| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    PinnedRoute { id: "deep".into(), source: "decision" }
                }).await.id.clone()
            }));
        }
        for handle in handles { assert_eq!(handle.await.unwrap(), "deep"); }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
