use crate::settings::{LlmRequestOptions, PostProcessProvider};
use log::{debug, error, info};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub enum ConfigError {
    NoProvider,
    NoModel,
    NoPrompt,
    PromptNotFound,
    EmptyPrompt,
    InvalidOptions(String),
    UnsupportedProvider,
    InvalidHeader,
    OptionsRejected { status: u16, detail: String },
}

#[derive(Debug)]
pub enum InputError {
    BlankText,
    TooLong {
        field: &'static str,
        limit: usize,
        actual: usize,
    },
}

#[derive(Debug, PartialEq)]
pub enum OutputError {
    MalformedResponse,
    Empty,
    ReasoningOnly,
    Truncated,
    Filtered,
    MalformedJson,
    MissingField,
    TooLarge,
}

pub enum LlmError {
    Config(ConfigError),
    Input(InputError),
    Timeout,
    Transport(String),
    ContextExceeded,
    Provider { status: u16, detail: String },
    ResponseTooLarge,
    InvalidOutput(OutputError),
}

impl std::fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OptionsRejected { status, .. } => f
                .debug_struct("OptionsRejected")
                .field("status", status)
                .field("detail", &"[REDACTED]")
                .finish(),
            Self::InvalidOptions(reason) => f.debug_tuple("InvalidOptions").field(reason).finish(),
            Self::NoProvider => f.write_str("NoProvider"),
            Self::NoModel => f.write_str("NoModel"),
            Self::NoPrompt => f.write_str("NoPrompt"),
            Self::PromptNotFound => f.write_str("PromptNotFound"),
            Self::EmptyPrompt => f.write_str("EmptyPrompt"),
            Self::UnsupportedProvider => f.write_str("UnsupportedProvider"),
            Self::InvalidHeader => f.write_str("InvalidHeader"),
        }
    }
}

impl std::fmt::Debug for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => f.debug_tuple("Config").field(error).finish(),
            Self::Input(error) => f.debug_tuple("Input").field(error).finish(),
            Self::Timeout => f.write_str("Timeout"),
            Self::Transport(reason) => f.debug_tuple("Transport").field(reason).finish(),
            Self::ContextExceeded => f.write_str("ContextExceeded"),
            Self::Provider { status, .. } => f
                .debug_struct("Provider")
                .field("status", status)
                .field("detail", &"[REDACTED]")
                .finish(),
            Self::ResponseTooLarge => f.write_str("ResponseTooLarge"),
            Self::InvalidOutput(error) => f.debug_tuple("InvalidOutput").field(error).finish(),
        }
    }
}

impl LlmError {
    /// Stable, content-free recovery detail for translation and diagnostics.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Config(error) => match error {
                ConfigError::NoProvider => "no_provider",
                ConfigError::NoModel => "no_model",
                ConfigError::NoPrompt => "no_prompt",
                ConfigError::PromptNotFound => "prompt_not_found",
                ConfigError::EmptyPrompt => "empty_prompt",
                ConfigError::InvalidOptions(_) => "invalid_options",
                ConfigError::UnsupportedProvider => "unsupported_provider",
                ConfigError::InvalidHeader => "invalid_header",
                ConfigError::OptionsRejected { .. } => "options_rejected",
            },
            Self::Input(InputError::BlankText) => "blank_text",
            Self::Input(InputError::TooLong { .. }) => "too_long",
            Self::Timeout => "timeout",
            Self::Transport(_) => "transport",
            Self::ContextExceeded => "context_exceeded",
            Self::Provider { .. } => "provider",
            Self::ResponseTooLarge => "response_too_large",
            Self::InvalidOutput(error) => match error {
                OutputError::MalformedResponse => "malformed_response",
                OutputError::Empty => "empty",
                OutputError::ReasoningOnly => "reasoning_only",
                OutputError::Truncated => "truncated",
                OutputError::Filtered => "filtered",
                OutputError::MalformedJson => "malformed_json",
                OutputError::MissingField => "missing_field",
                OutputError::TooLarge => "too_large",
            },
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "llm_config",
            Self::Input(_) => "llm_input",
            Self::Timeout => "llm_timeout",
            Self::Transport(_) => "llm_transport",
            Self::ContextExceeded => "llm_context_exceeded",
            Self::Provider { .. } => "llm_provider",
            Self::ResponseTooLarge => "llm_response_too_large",
            Self::InvalidOutput(_) => "llm_invalid_output",
        }
    }

    /// Provider details are for explicit UI display only, never logging.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Provider { detail, .. }
            | Self::Config(ConfigError::OptionsRejected { detail, .. }) => Some(detail),
            _ => None,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())?;
        match self {
            Self::Provider { status, .. }
            | Self::Config(ConfigError::OptionsRejected { status, .. }) => {
                write!(f, " (status: {status})")
            }
            Self::Input(InputError::TooLong {
                field,
                limit,
                actual,
            }) => write!(f, " ({field}: {actual}, limit: {limit})"),
            Self::InvalidOutput(reason) => write!(f, " ({reason:?})"),
            Self::Config(ConfigError::InvalidOptions(reason)) => write!(f, " ({reason})"),
            Self::Transport(reason) => write!(f, " ({reason})"),
            _ => Ok(()),
        }
    }
}

impl StdError for LlmError {}

pub struct ChatRequest<'a> {
    pub provider: &'a PostProcessProvider,
    pub api_key: &'a str,
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub schema: Option<(&'static str, Value)>,
    pub disable_reasoning: bool,
    pub options: &'a LlmRequestOptions,
}

pub struct ChatReply {
    pub content: Option<String>,
    pub finish_reason: Option<String>,
    pub had_reasoning: bool,
}

// Intentionally no Debug/Display: the cache identity contains credentials.
#[derive(PartialEq)]
struct ClientKey {
    provider_id: String,
    base_url: String,
    api_key: String,
    connect_timeout_secs: u32,
}

#[derive(Default)]
pub(crate) struct ClientCache(Mutex<Option<(ClientKey, reqwest::Client)>>);

impl ClientCache {
    fn client(
        &self,
        provider: &PostProcessProvider,
        api_key: &str,
        options: &LlmRequestOptions,
    ) -> Result<reqwest::Client, LlmError> {
        let key = ClientKey {
            provider_id: provider.id.clone(),
            base_url: provider.base_url.trim_end_matches('/').into(),
            api_key: api_key.into(),
            connect_timeout_secs: options.connect_timeout_secs,
        };
        let mut slot = self
            .0
            .lock()
            .map_err(|_| LlmError::Transport("HTTP client cache unavailable".into()))?;
        if let Some((old, client)) = &*slot {
            if *old == key {
                return Ok(client.clone());
            }
        }
        let headers = build_headers(provider, api_key)
            .map_err(|_| LlmError::Config(ConfigError::InvalidHeader))?;
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(options.connect_timeout_secs.into()))
            .build()
            .map_err(|e| {
                LlmError::Transport(report_reqwest_error("Failed to build HTTP client", &e))
            })?;
        *slot = Some((key, client.clone()));
        Ok(client)
    }
}

static CLIENT_CACHE: ClientCache = ClientCache(Mutex::new(None));
pub(crate) const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_REQUEST: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY: usize = 64 * 1024;

pub(crate) struct Limits {
    pub deadline: Duration,
    pub max_body: usize,
}

impl Limits {
    fn from_options(options: &LlmRequestOptions) -> Self {
        Self {
            deadline: Duration::from_secs(options.timeout_secs.into()),
            max_body: MAX_BODY,
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    json_schema: JsonSchema,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<bool>,
}

/// Request fields used to ask an endpoint to skip reasoning/thinking.
/// Providers disagree on the field name and accepted values, so at most one of
/// these is set per request (see `reasoning_disable_params`).
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

impl ReasoningParams {
    fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.reasoning.is_none() && self.thinking.is_none()
    }
}

/// Pick the reasoning-disable request fields an endpoint understands.
/// Unknown endpoints get the common OpenAI-style field; if they reject it,
/// the request is retried without it (see `send_chat_completion_with_schema`).
fn reasoning_disable_params(provider: &PostProcessProvider) -> ReasoningParams {
    let base_url = provider.base_url.to_lowercase();
    if base_url.contains("api.deepseek.com") {
        // DeepSeek rejects reasoning_effort "none" and uses its own field:
        // https://api-docs.deepseek.com/guides/thinking_mode
        ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
    } else if provider.id == "openrouter" {
        // OpenRouter nested object; exclude:true also keeps reasoning text out
        // of the response so it can't pollute structured-output JSON parsing
        ReasoningParams {
            reasoning: Some(ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
            ..Default::default()
        }
    } else {
        ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
    }
}

/// Endpoints (base_url|model) that rejected the reasoning-disable fields with a
/// 4xx. Remembered for the lifetime of the process so every dictation after the
/// first skips the doomed attempt and goes straight to a plain request.
fn reasoning_rejections() -> &'static Mutex<HashSet<String>> {
    static REJECTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REJECTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn endpoint_key(provider: &PostProcessProvider, model: &str) -> String {
    format!("{}|{}", provider.base_url.trim_end_matches('/'), model)
}

fn is_known_rejected(key: &str) -> bool {
    reasoning_rejections()
        .lock()
        .map(|set| set.contains(key))
        .unwrap_or(false)
}

fn remember_rejection(key: String) {
    if let Ok(mut set) = reasoning_rejections().lock() {
        set.insert(key);
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(flatten)]
    reasoning: ReasoningParams,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
    reasoning_content: Option<Value>,
    reasoning: Option<Value>,
}

/// Build headers for API requests based on provider type
fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    // Common headers
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/cjpais/Handy"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Handy/1.0 (+https://github.com/cjpais/Handy)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("Handy"));

    // Provider-specific auth headers
    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key)
                    .map_err(|e| format!("Invalid API key header value: {}", e))?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .map_err(|e| format!("Invalid authorization header value: {}", e))?,
            );
        }
    }

    Ok(headers)
}

/// Format a bounded error source chain.
///
/// `reqwest::Error`'s Display implementation intentionally gives only a short
/// summary. Nested causes contain the useful transport details, such as a
/// certificate validation failure, an HTTP/2 error, or a connection reset.
/// Callers must skip source types whose Display text can quote payload data.
fn error_source_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut causes = Vec::new();
    let mut source = error.source();

    // Defensive cap in case a third-party error exposes a cyclic source chain.
    for _ in 0..16 {
        let Some(cause) = source else {
            break;
        };
        causes.push(cause.to_string());
        source = cause.source();
    }

    causes
}

fn reqwest_error_kinds(error: &reqwest::Error) -> String {
    let mut kinds = Vec::new();

    if error.is_builder() {
        kinds.push("builder");
    }
    if error.is_connect() {
        kinds.push("connect");
    }
    if error.is_request() {
        kinds.push("request");
    }
    if error.is_redirect() {
        kinds.push("redirect");
    }
    if error.is_timeout() {
        kinds.push("timeout");
    }
    if error.is_status() {
        kinds.push("status");
    }
    if error.is_body() {
        kinds.push("body");
    }
    if error.is_decode() {
        kinds.push("decode");
    }
    if error.is_upgrade() {
        kinds.push("upgrade");
    }

    if kinds.is_empty() {
        "unknown".to_string()
    } else {
        kinds.join(", ")
    }
}

fn sanitized_url(url: &reqwest::Url) -> String {
    let mut url = url.clone();

    // Custom endpoints should not contain credentials or query-string tokens,
    // but omit them from diagnostics in case one does.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);

    url.to_string()
}

fn sanitized_url_for_log(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| sanitized_url(&url))
        // Do not echo an invalid URL: the parse failure might have been caused
        // by sensitive data entered in the custom endpoint field.
        .unwrap_or_else(|_| "<invalid URL>".to_string())
}

fn report_reqwest_error(context: &str, error: &reqwest::Error) -> String {
    let kinds = reqwest_error_kinds(error);
    let url = error
        .url()
        .map(sanitized_url)
        .map(|url| format!(", url: {url}"))
        .unwrap_or_default();

    // serde_json's error text can quote values from a malformed response. That
    // response may contain transcription content, so retain the useful decode
    // classification but never put its nested source in logs or UI errors.
    let causes = if error.is_decode() {
        Vec::new()
    } else {
        error_source_chain(error)
    };
    let cause_details = if !causes.is_empty() {
        format!(": caused by: {}", causes.join(" -> "))
    } else if error.url().is_none() {
        // Reqwest's short Display text is safe when it cannot append a raw URL.
        format!(": {error}")
    } else {
        // The sanitized URL is already included above. Avoid formatting the
        // original error because its Display implementation includes the raw URL.
        String::new()
    };

    let details = format!("{context} (kind: {kinds}{url}){cause_details}");
    error!("{details}");
    details
}

async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
    truncate: bool,
) -> Result<Vec<u8>, LlmError> {
    if !truncate && response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(LlmError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| LlmError::Transport(report_reqwest_error("HTTP body read failed", &e)))?
    {
        let remaining = limit.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            if !truncate {
                return Err(LlmError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
        bytes.extend_from_slice(&chunk);
        if truncate && bytes.len() == limit {
            break;
        }
    }
    Ok(bytes)
}

async fn provider_error(response: reqwest::Response, options_sent: bool) -> LlmError {
    let status = response.status().as_u16();
    let detail = match bounded_body(response, MAX_ERROR_BODY, true).await {
        Ok(bytes) => {
            if context_exceeded(status, &bytes) {
                return LlmError::ContextExceeded;
            }
            String::from_utf8_lossy(&bytes).chars().take(1024).collect()
        }
        // The HTTP status is already known; detail is best-effort UI context.
        Err(_) => String::new(),
    };
    if options_sent && matches!(status, 400 | 422) {
        LlmError::Config(ConfigError::OptionsRejected { status, detail })
    } else {
        LlmError::Provider { status, detail }
    }
}

fn context_exceeded(status: u16, body: &[u8]) -> bool {
    if !matches!(status, 400 | 422) {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let error = &value["error"];
    let kind = error["type"].as_str().unwrap_or_default();
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        kind,
        "exceed_context_size_error" | "context_length_exceeded"
    ) || code == "context_length_exceeded"
        || (message.contains("context") && message.contains("exceed"))
}

/// One generation, with at most one reasoning compatibility retry in the same deadline.
pub async fn chat_completion(req: ChatRequest<'_>) -> Result<ChatReply, LlmError> {
    let limits = Limits::from_options(req.options);
    chat_completion_with_limits(req, &CLIENT_CACHE, limits).await
}

pub(crate) async fn chat_completion_with_limits(
    req: ChatRequest<'_>,
    cache: &ClientCache,
    limits: Limits,
) -> Result<ChatReply, LlmError> {
    req.options
        .validate()
        .map_err(|e| LlmError::Config(ConfigError::InvalidOptions(e)))?;
    let started = std::time::Instant::now();
    tokio::time::timeout(limits.deadline, async {
        let client = cache.client(req.provider, req.api_key, req.options)?;
        let url = format!(
            "{}/chat/completions",
            req.provider.base_url.trim_end_matches('/')
        );
        let key = endpoint_key(req.provider, req.model);
        let reasoning = if req.disable_reasoning && !is_known_rejected(&key) {
            reasoning_disable_params(req.provider)
        } else {
            ReasoningParams::default()
        };
        let mut body = ChatCompletionRequest {
            model: req.model.into(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: req.system.into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: req.user.into(),
                },
            ],
            stream: false,
            temperature: req.options.temperature,
            max_tokens: req.options.max_output_tokens,
            response_format: req.schema.map(|(name, schema)| ResponseFormat {
                format_type: "json_schema".into(),
                json_schema: JsonSchema {
                    name: name.into(),
                    strict: true,
                    schema,
                },
            }),
            reasoning,
        };
        let encode = |body: &ChatCompletionRequest| -> Result<Vec<u8>, LlmError> {
            let bytes = serde_json::to_vec(body).map_err(|_| {
                LlmError::Config(ConfigError::InvalidOptions(
                    "Request encoding failed".into(),
                ))
            })?;
            if bytes.len() > MAX_REQUEST {
                return Err(LlmError::Input(InputError::TooLong {
                    field: "request_bytes",
                    limit: MAX_REQUEST,
                    actual: bytes.len(),
                }));
            }
            Ok(bytes)
        };
        let mut response = client
            .post(&url)
            .body(encode(&body)?)
            .send()
            .await
            .map_err(|e| LlmError::Transport(report_reqwest_error("HTTP request failed", &e)))?;
        if matches!(response.status().as_u16(), 400 | 422) && !body.reasoning.is_empty() {
            // Context overflow is not a reasoning compatibility rejection.
            // Never repeat an oversized request or weaken its input.
            let rejection = provider_error(response, false).await;
            if matches!(rejection, LlmError::ContextExceeded) {
                return Err(rejection);
            }
            info!(
                concat!(
                    "Endpoint rejected reasoning options (status {}, url: {}); ",
                    "retrying without reasoning fields"
                ),
                rejection,
                sanitized_url_for_log(&url)
            );
            body.reasoning = ReasoningParams::default();
            response = client
                .post(&url)
                .body(encode(&body)?)
                .send()
                .await
                .map_err(|e| LlmError::Transport(report_reqwest_error("HTTP retry failed", &e)))?;
            if response.status().is_success() {
                remember_rejection(key);
            }
        }
        if !response.status().is_success() {
            return Err(provider_error(
                response,
                body.temperature.is_some() || body.max_tokens.is_some(),
            )
            .await);
        }
        let bytes = bounded_body(response, limits.max_body, false).await?;
        let envelope: ChatCompletionResponse = serde_json::from_slice(&bytes)
            .map_err(|_| LlmError::InvalidOutput(OutputError::MalformedResponse))?;
        let choice = envelope
            .choices
            .into_iter()
            .next()
            .ok_or(LlmError::InvalidOutput(OutputError::Empty))?;
        // Synchronous bounded decoding cannot yield to Tokio's timer. Never return
        // a successful reply if encoding/decoding exhausted the same budget.
        if started.elapsed() >= limits.deadline {
            return Err(LlmError::Timeout);
        }
        Ok(ChatReply {
            content: choice.message.content,
            finish_reason: choice.finish_reason,
            had_reasoning: choice
                .message
                .reasoning_content
                .is_some_and(|v| !v.is_null())
                || choice.message.reasoning.is_some_and(|v| !v.is_null()),
        })
    })
    .await
    .map_err(|_| LlmError::Timeout)?
}

/// Model discovery shares the connection cache and bounded response deadline.
pub async fn fetch_models(
    provider: &PostProcessProvider,
    api_key: String,
    options: &LlmRequestOptions,
) -> Result<Vec<String>, String> {
    fetch_models_with_limits(
        provider,
        &api_key,
        options,
        &CLIENT_CACHE,
        Limits::from_options(options),
    )
    .await
    .map_err(|e| e.to_string())
}

pub(crate) async fn fetch_models_with_limits(
    provider: &PostProcessProvider,
    api_key: &str,
    options: &LlmRequestOptions,
    cache: &ClientCache,
    limits: Limits,
) -> Result<Vec<String>, LlmError> {
    options
        .validate()
        .map_err(|e| LlmError::Config(ConfigError::InvalidOptions(e)))?;
    let started = std::time::Instant::now();
    tokio::time::timeout(limits.deadline, async {
        let client = cache.client(provider, api_key, options)?;
        let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
        debug!("Fetching models from: {}", sanitized_url_for_log(&url));
        let response =
            client.get(&url).send().await.map_err(|e| {
                LlmError::Transport(report_reqwest_error("Failed to fetch models", &e))
            })?;
        if !response.status().is_success() {
            return Err(provider_error(response, false).await);
        }
        let bytes = bounded_body(response, limits.max_body, false).await?;
        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|_| LlmError::InvalidOutput(OutputError::MalformedResponse))?;
        let mut models = Vec::new();
        if let Some(data) = parsed.get("data").and_then(Value::as_array) {
            for entry in data {
                if let Some(id) = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| entry.get("name").and_then(Value::as_str))
                {
                    models.push(id.into());
                }
            }
        } else if let Some(array) = parsed.as_array() {
            models.extend(array.iter().filter_map(Value::as_str).map(String::from));
        }
        if started.elapsed() >= limits.deadline {
            return Err(LlmError::Timeout);
        }
        Ok(models)
    })
    .await
    .map_err(|_| LlmError::Timeout)?
}

#[cfg(test)]
pub(crate) mod test_http {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    pub enum Step {
        Respond(u16, String),
        DelayedRespond(Duration, u16, String),
        Chunked(String),
        StallBeforeHeaders,
        StallMidBody(usize, String),
        BrokenBody(u16),
    }

    pub struct FakeServer {
        pub url: String,
        pub requests: Arc<Mutex<Vec<Value>>>,
        pub accepts: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeServer {
        pub async fn new(steps: Vec<Step>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let accepts = Arc::new(AtomicUsize::new(0));
            let steps = Arc::new(Mutex::new(VecDeque::from(steps)));
            let recorded = requests.clone();
            let accepted = accepts.clone();
            let task = tokio::spawn(async move {
                let mut workers = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        socket = listener.accept() => {
                            let (stream, _) = socket.unwrap();
                            accepted.fetch_add(1, Ordering::SeqCst);
                            let steps = steps.clone();
                            let recorded = recorded.clone();
                            workers.spawn(async move {
                                let mut stream = BufReader::new(stream);
                                loop {
                                    let mut line = String::new();
                                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 { break; }
                                    let mut length = 0;
                                    loop {
                                        line.clear();
                                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 { return; }
                                        if line == "\r\n" { break; }
                                        if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                            length = n.trim().parse::<usize>().unwrap();
                                        }
                                    }
                                    let mut body = vec![0; length];
                                    if stream.read_exact(&mut body).await.is_err() { return; }
                                    recorded.lock().unwrap().push(if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() });
                                    let step = steps.lock().unwrap().pop_front();
                                    let Some(step) = step else { return; };
                                    let (status, body) = match step {
                                        Step::Respond(status, body) => (status, body),
                                        Step::DelayedRespond(delay, status, body) => {
                                            tokio::time::sleep(delay).await;
                                            (status, body)
                                        }
                                        Step::StallBeforeHeaders => {
                                            std::future::pending::<()>().await;
                                            return;
                                        }
                                        Step::StallMidBody(length, prefix) => {
                                            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n{prefix}");
                                            let _ = stream.get_mut().write_all(header.as_bytes()).await;
                                            std::future::pending::<()>().await;
                                            return;
                                        }
                                        Step::BrokenBody(status) => {
                                            let header = format!("HTTP/1.1 {status} Test\r\nContent-Length: 100\r\nConnection: close\r\n\r\nprivate partial detail");
                                            let _ = stream.get_mut().write_all(header.as_bytes()).await;
                                            return;
                                        }
                                        Step::Chunked(body) => {
                                            let header = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body);
                                            if stream.get_mut().write_all(header.as_bytes()).await.is_err() { return; }
                                            continue;
                                        }
                                    };
                                    let response = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}", body.len());
                                    if stream.get_mut().write_all(response.as_bytes()).await.is_err() { return; }
                                }
                            });
                        }
                        _ = workers.join_next(), if !workers.is_empty() => {}
                    }
                }
            });
            Self {
                url,
                requests,
                accepts,
                task,
            }
        }
    }

    pub fn provider(url: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: "custom".into(),
            label: "Fake".into(),
            base_url: url.into(),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: true,
        }
    }

    pub fn completion(content: Option<&str>, finish_reason: &str) -> String {
        serde_json::json!({ "choices": [{ "message": { "content": content }, "finish_reason": finish_reason }] }).to_string()
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn context_exceeded_http_maps_to_too_long_without_retry() {
        // Captured from the running llama.cpp qwen3.5:9b server, ctx 16384.
        let body = r#"{"error":{"code":400,"message":"request (18012 tokens) exceeds the available context size (16384 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":18012,"n_ctx":16384}}"#;
        for temperature in [None, Some(0.0)] {
            let server = FakeServer::new(vec![Step::Respond(400, body.into())]).await;
            let options = LlmRequestOptions {
                temperature,
                ..Default::default()
            };
            let error = call(
                &server,
                &options,
                true,
                MAX_BODY,
                Duration::from_secs(2),
                &ClientCache::default(),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.code(), "llm_context_exceeded");
            assert_eq!(error.reason(), "context_exceeded");
            assert_eq!(server.accepts.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(!format!("{error:?}").contains("18012"));
        }
    }
    use super::*;
    use std::fmt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<Box<TestError>>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    fn provider(id: &str, base_url: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: base_url.to_string(),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
        }
    }

    fn request_json(reasoning: ReasoningParams) -> Value {
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            response_format: None,
            temperature: None,
            max_tokens: None,
            reasoning,
        };
        serde_json::to_value(&request).unwrap()
    }

    async fn serve_one_response(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        format!("http://{address}")
    }

    #[test]
    fn error_source_chain_includes_all_nested_causes() {
        let error = TestError {
            message: "request failed",
            source: Some(Box::new(TestError {
                message: "TLS handshake failed",
                source: Some(Box::new(TestError {
                    message: "unknown certificate authority",
                    source: None,
                })),
            })),
        };

        assert_eq!(
            error_source_chain(&error),
            vec!["TLS handshake failed", "unknown certificate authority"]
        );
    }

    #[test]
    fn log_url_sanitization_removes_credentials_and_tokens() {
        let url = "https://user:password@example.com/v1/models?api_key=secret#private";
        assert_eq!(sanitized_url_for_log(url), "https://example.com/v1/models");
    }

    #[test]
    fn invalid_log_urls_are_not_echoed() {
        assert_eq!(
            sanitized_url_for_log("not a URL containing secret"),
            "<invalid URL>"
        );
    }

    #[tokio::test]
    async fn decode_error_does_not_echo_response_values() {
        let base_url =
            serve_one_response("200 OK", r#"{"choices":"PRIVATE TRANSCRIPTION CONTENT"}"#).await;
        let error = reqwest::get(base_url)
            .await
            .unwrap()
            .json::<ChatCompletionResponse>()
            .await
            .unwrap_err();

        let details = report_reqwest_error("Failed to parse API response", &error);
        assert!(details.contains("kind: decode"));
        assert!(!details.contains("PRIVATE TRANSCRIPTION CONTENT"));
    }

    #[tokio::test]
    async fn raw_error_url_is_not_reintroduced_without_a_source() {
        let base_url = serve_one_response("400 Bad Request", "bad request").await;
        let error = reqwest::get(format!(
            "{base_url}/private?api_key=SECRET_QUERY_TOKEN#private"
        ))
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();

        let details = report_reqwest_error("Request failed", &error);
        assert!(details.contains(&format!("url: {base_url}/private")));
        assert!(!details.contains("SECRET_QUERY_TOKEN"));
        assert!(!details.contains("#private"));
    }

    #[test]
    fn requests_explicitly_disable_streaming() {
        let json = request_json(ReasoningParams::default());
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn default_reasoning_params_serialize_to_no_fields() {
        let json = request_json(ReasoningParams::default());
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn custom_provider_uses_top_level_reasoning_effort() {
        let params = reasoning_disable_params(&provider("custom", "http://localhost:11434/v1"));
        let json = request_json(params);
        assert_eq!(json["reasoning_effort"], "none");
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn openrouter_uses_nested_reasoning_object() {
        let params =
            reasoning_disable_params(&provider("openrouter", "https://openrouter.ai/api/v1"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert_eq!(json["reasoning"]["effort"], "none");
        assert_eq!(json["reasoning"]["exclude"], true);
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn deepseek_base_url_uses_thinking_disabled() {
        let params = reasoning_disable_params(&provider("custom", "https://api.deepseek.com"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert_eq!(json["thinking"]["type"], "disabled");
    }

    #[test]
    fn reasoning_params_is_empty_tracks_all_fields() {
        assert!(ReasoningParams::default().is_empty());
        assert!(!ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn rejection_memo_is_keyed_by_base_url_and_model() {
        let deepseek = provider("custom", "https://api.deepseek.com/");
        let key = endpoint_key(&deepseek, "deepseek-chat");
        assert_eq!(key, "https://api.deepseek.com|deepseek-chat");
        assert!(!is_known_rejected(&key));
        remember_rejection(key.clone());
        assert!(is_known_rejected(&key));
        // A different model on the same endpoint is tracked separately
        assert!(!is_known_rejected(&endpoint_key(&deepseek, "other-model")));
    }

    use super::test_http::{completion, FakeServer, Step};

    async fn call(
        server: &FakeServer,
        options: &LlmRequestOptions,
        reasoning: bool,
        limit: usize,
        deadline: Duration,
        cache: &ClientCache,
    ) -> Result<ChatReply, LlmError> {
        let provider = test_http::provider(&server.url);
        chat_completion_with_limits(
            ChatRequest {
                provider: &provider,
                api_key: "",
                model: "fake-model",
                system: "trusted",
                user: "untrusted",
                schema: None,
                disable_reasoning: reasoning,
                options,
            },
            cache,
            Limits {
                deadline,
                max_body: limit,
            },
        )
        .await
    }

    fn test_options() -> LlmRequestOptions {
        LlmRequestOptions {
            temperature: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn request_sends_system_then_user_and_no_tools() {
        let server =
            FakeServer::new(vec![Step::Respond(200, completion(Some("ok"), "stop"))]).await;
        call(
            &server,
            &test_options(),
            false,
            MAX_BODY,
            Duration::from_secs(2),
            &ClientCache::default(),
        )
        .await
        .unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests[0]["messages"],
            serde_json::json!([
                {"role":"system", "content":"trusted"}, {"role":"user", "content":"untrusted"}
            ])
        );
        for key in [
            "tools",
            "tool_choice",
            "functions",
            "response_format",
            "temperature",
            "max_tokens",
        ] {
            assert!(requests[0].get(key).is_none(), "{key}");
        }
    }

    #[tokio::test]
    async fn configured_temperature_tokens_and_named_strict_schema_are_sent() {
        let server =
            FakeServer::new(vec![Step::Respond(200, completion(Some("ok"), "stop"))]).await;
        let provider = test_http::provider(&server.url);
        let options = LlmRequestOptions {
            temperature: Some(0.7),
            max_output_tokens: Some(2048),
            ..Default::default()
        };
        chat_completion_with_limits(
            ChatRequest {
                provider: &provider,
                api_key: "",
                model: "fake",
                system: "system",
                user: "user",
                schema: Some(("assistant_output", serde_json::json!({"type": "object"}))),
                disable_reasoning: false,
                options: &options,
            },
            &ClientCache::default(),
            Limits {
                deadline: Duration::from_secs(2),
                max_body: MAX_BODY,
            },
        )
        .await
        .unwrap();
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0]["temperature"], 0.7);
        assert_eq!(requests[0]["max_tokens"], 2048);
        assert_eq!(
            requests[0]["response_format"]["json_schema"]["name"],
            "assistant_output"
        );
        assert_eq!(
            requests[0]["response_format"]["json_schema"]["strict"],
            true
        );
    }

    #[tokio::test]
    async fn deadline_covers_stalled_headers_and_body() {
        for step in [
            Step::StallBeforeHeaders,
            Step::StallMidBody(100, "x".into()),
        ] {
            let server = FakeServer::new(vec![step]).await;
            let start = std::time::Instant::now();
            let result = call(
                &server,
                &test_options(),
                false,
                MAX_BODY,
                Duration::from_millis(80),
                &ClientCache::default(),
            )
            .await;
            assert!(matches!(result, Err(LlmError::Timeout)));
            assert!(start.elapsed() < Duration::from_millis(500));
        }
    }

    #[tokio::test]
    async fn deadline_covers_reasoning_retry() {
        let server = FakeServer::new(vec![
            Step::DelayedRespond(Duration::from_millis(150), 400, "rejected".into()),
            Step::StallBeforeHeaders,
        ])
        .await;
        let start = std::time::Instant::now();
        let result = call(
            &server,
            &test_options(),
            true,
            MAX_BODY,
            Duration::from_millis(250),
            &ClientCache::default(),
        )
        .await;
        assert!(matches!(result, Err(LlmError::Timeout)));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert!(
            start.elapsed() < Duration::from_millis(370),
            "retry received a separate deadline"
        );
    }

    #[tokio::test]
    async fn reasoning_rejection_retries_once_without_fields_and_is_memoized() {
        let server = FakeServer::new(vec![
            Step::Respond(422, "private rejection".into()),
            Step::Respond(200, completion(Some("ok"), "stop")),
            Step::Respond(200, completion(Some("ok"), "stop")),
        ])
        .await;
        let cache = ClientCache::default();
        for _ in 0..2 {
            call(
                &server,
                &test_options(),
                true,
                MAX_BODY,
                Duration::from_secs(2),
                &cache,
            )
            .await
            .unwrap();
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["reasoning_effort"], "none");
        assert!(requests[1].get("reasoning_effort").is_none());
        assert!(requests[2].get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn options_rejection_is_config_error_and_no_options_is_provider_error() {
        for status in [400, 422] {
            for temperature in [None, Some(0.0)] {
                let server =
                    FakeServer::new(vec![Step::Respond(status, "private".repeat(20000))]).await;
                let options = LlmRequestOptions {
                    temperature,
                    ..Default::default()
                };
                let error = call(
                    &server,
                    &options,
                    false,
                    MAX_BODY,
                    Duration::from_secs(2),
                    &ClientCache::default(),
                )
                .await
                .err()
                .unwrap();
                match (&error, temperature) {
                    (
                        LlmError::Config(ConfigError::OptionsRejected { status: actual, .. }),
                        Some(_),
                    )
                    | (LlmError::Provider { status: actual, .. }, None) => {
                        assert_eq!(*actual, status)
                    }
                    _ => panic!("unexpected safe error: {error}"),
                }
                assert_eq!(error.detail().unwrap().chars().count(), 1024);
                assert!(!error.to_string().contains("private"));
            }
        }
    }

    #[tokio::test]
    async fn content_length_and_streamed_body_over_limit_are_rejected() {
        for step in [
            Step::Respond(200, "x".repeat(100)),
            Step::Chunked("x".repeat(100)),
            Step::StallMidBody(100, "".into()),
        ] {
            let server = FakeServer::new(vec![step]).await;
            let result = call(
                &server,
                &test_options(),
                false,
                99,
                Duration::from_secs(2),
                &ClientCache::default(),
            )
            .await;
            assert!(matches!(result, Err(LlmError::ResponseTooLarge)));
        }
    }

    #[tokio::test]
    async fn body_within_limit_is_accepted_and_finish_reason_is_reported() {
        let body = completion(Some("ok"), "length");
        let server = FakeServer::new(vec![Step::Chunked(body.clone())]).await;
        let reply = call(
            &server,
            &test_options(),
            false,
            body.len(),
            Duration::from_secs(2),
            &ClientCache::default(),
        )
        .await
        .unwrap();
        assert_eq!(reply.content.as_deref(), Some("ok"));
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
    }

    #[tokio::test]
    async fn malformed_envelope_and_empty_choices_are_invalid_without_echo() {
        for (body, expected) in [
            (r#"{"choices":"PRIVATE"}"#, OutputError::MalformedResponse),
            (r#"{"choices":[]}"#, OutputError::Empty),
        ] {
            let server = FakeServer::new(vec![Step::Respond(200, body.into())]).await;
            let error = call(
                &server,
                &test_options(),
                false,
                MAX_BODY,
                Duration::from_secs(2),
                &ClientCache::default(),
            )
            .await
            .err()
            .unwrap();
            assert!(matches!(&error, LlmError::InvalidOutput(e) if *e == expected));
            assert!(!error.to_string().contains("PRIVATE"));
        }
    }

    #[tokio::test]
    async fn reasoning_fields_are_never_returned() {
        for field in ["reasoning_content", "reasoning"] {
            let body = serde_json::json!({"choices":[{"message":{"content":null, (field):"PRIVATE REASONING"}, "finish_reason":"stop"}]}).to_string();
            let server = FakeServer::new(vec![Step::Respond(200, body)]).await;
            let reply = call(
                &server,
                &test_options(),
                false,
                MAX_BODY,
                Duration::from_secs(2),
                &ClientCache::default(),
            )
            .await
            .unwrap();
            assert!(reply.had_reasoning);
            assert!(reply.content.is_none());
        }
    }

    #[test]
    fn error_debug_redacts_provider_detail() {
        let private = "PRIVATE provider detail with credential";
        let config = ConfigError::OptionsRejected {
            status: 422,
            detail: private.into(),
        };
        let config_debug = format!("{config:?}");
        assert!(config_debug.contains("OptionsRejected"));
        assert!(config_debug.contains("422"));
        assert!(!config_debug.contains(private));
        for error in [
            LlmError::Config(config),
            LlmError::Provider {
                status: 503,
                detail: private.into(),
            },
        ] {
            let debug = format!("{error:?}");
            assert!(
                !debug.contains(private),
                "provider detail leaked through Debug"
            );
            assert_eq!(error.detail(), Some(private));
        }
    }

    #[tokio::test]
    async fn unreadable_provider_body_preserves_status_classification() {
        for status in [400, 422, 503] {
            for options_sent in [false, true] {
                let server = FakeServer::new(vec![Step::BrokenBody(status)]).await;
                let mut options = test_options();
                options.temperature = options_sent.then_some(0.0);
                options.max_output_tokens = None;
                let error = call(
                    &server,
                    &options,
                    false,
                    MAX_BODY,
                    Duration::from_secs(2),
                    &ClientCache::default(),
                )
                .await
                .err()
                .unwrap();
                match &error {
                    LlmError::Config(ConfigError::OptionsRejected { status: actual, .. })
                        if options_sent && matches!(status, 400 | 422) =>
                    {
                        assert_eq!(*actual, status)
                    }
                    LlmError::Provider { status: actual, .. } if !options_sent || status == 503 => {
                        assert_eq!(*actual, status)
                    }
                    _ => panic!("HTTP classification lost: {error}"),
                }
                assert_eq!(error.detail(), Some(""));
            }
        }
    }

    #[tokio::test]
    async fn client_reuse_and_invalidation_cover_key_url_provider_and_connect_timeout() {
        use std::sync::atomic::Ordering;
        let server = FakeServer::new(
            (0..6)
                .map(|_| Step::Respond(200, completion(Some("ok"), "stop")))
                .collect(),
        )
        .await;
        let cache = ClientCache::default();
        let mut provider = test_http::provider(&server.url);
        let mut options = test_options();
        for (index, key) in [
            "secret", "secret", "changed", "changed", "changed", "changed",
        ]
        .iter()
        .enumerate()
        {
            if index == 3 {
                provider.id = "different".into();
            }
            if index == 4 {
                options.connect_timeout_secs = 6;
            }
            if index == 5 {
                provider.base_url.push_str("/v1");
            }
            chat_completion_with_limits(
                ChatRequest {
                    provider: &provider,
                    api_key: key,
                    model: "fake",
                    system: "sys",
                    user: "usr",
                    schema: None,
                    disable_reasoning: false,
                    options: &options,
                },
                &cache,
                Limits {
                    deadline: Duration::from_secs(2),
                    max_body: MAX_BODY,
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(server.accepts.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn model_discovery_uses_deadline_body_limit_and_shared_cache() {
        use std::sync::atomic::Ordering;
        let server = FakeServer::new(vec![
            Step::Respond(200, r#"{"data":[{"id":"one"},{"name":"two"}]}"#.into()),
            Step::Respond(200, completion(Some("ok"), "stop")),
        ])
        .await;
        let cache = ClientCache::default();
        let provider = test_http::provider(&server.url);
        let result = fetch_models_with_limits(
            &provider,
            "",
            &test_options(),
            &cache,
            Limits {
                deadline: Duration::from_secs(2),
                max_body: MAX_BODY,
            },
        )
        .await
        .unwrap();
        assert_eq!(result, ["one", "two"]);
        call(
            &server,
            &test_options(),
            false,
            MAX_BODY,
            Duration::from_secs(2),
            &cache,
        )
        .await
        .unwrap();
        assert_eq!(server.accepts.load(Ordering::SeqCst), 1);
        let server = FakeServer::new(vec![Step::StallBeforeHeaders]).await;
        let result = fetch_models_with_limits(
            &test_http::provider(&server.url),
            "",
            &test_options(),
            &ClientCache::default(),
            Limits {
                deadline: Duration::from_millis(80),
                max_body: MAX_BODY,
            },
        )
        .await;
        assert!(matches!(result, Err(LlmError::Timeout)));
        let server = FakeServer::new(vec![Step::Chunked("x".repeat(100))]).await;
        let result = fetch_models_with_limits(
            &test_http::provider(&server.url),
            "",
            &test_options(),
            &ClientCache::default(),
            Limits {
                deadline: Duration::from_secs(2),
                max_body: 99,
            },
        )
        .await;
        assert!(matches!(result, Err(LlmError::ResponseTooLarge)));
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_network_io() {
        let server = FakeServer::new(vec![]).await;
        let provider = test_http::provider(&server.url);
        let user = "x".repeat(MAX_REQUEST);
        let result = chat_completion_with_limits(
            ChatRequest {
                provider: &provider,
                api_key: "",
                model: "fake",
                system: "sys",
                user: &user,
                schema: None,
                disable_reasoning: false,
                options: &test_options(),
            },
            &ClientCache::default(),
            Limits {
                deadline: Duration::from_secs(2),
                max_body: MAX_BODY,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(LlmError::Input(InputError::TooLong {
                field: "request_bytes",
                ..
            }))
        ));
        assert!(server.requests.lock().unwrap().is_empty());
    }
}
