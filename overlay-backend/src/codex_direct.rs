//! Direct ChatGPT-backed Codex transport.
//!
//! This is intentionally additive to `codex_subscription`: users can keep the
//! official local app-server path or opt into direct OAuth + Codex Responses.
//! OAuth credentials are stored only in the protected credential store.

use crate::ai::{AiEndpoint, AiEvent, ChatMessage, ContentPart, MessageContent, TokenUsage};
use crate::codex_subscription::{AccountState, CodexModel, LoginEvent, ProviderSnapshot};
use crate::credentials::{self, SecretSlot};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
// Keep model discovery aligned with the current stable Codex client protocol.
// This is intentionally independent from Suflyor's own package version.
const CODEX_CLIENT_VERSION: &str = "0.160.1";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_MARGIN: u64 = 60;

static LOGIN_GENERATION: AtomicU64 = AtomicU64::new(0);
static TOKEN_REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Copy)]
pub struct LoginAttempt(u64);

#[derive(Clone)]
struct Credential {
    access: String,
    account_id: String,
    plan: Option<String>,
}

#[derive(Debug, Clone)]
struct DeviceAuth {
    id: String,
    code: String,
    interval: Duration,
}

#[must_use]
pub fn begin_device_login() -> LoginAttempt {
    LoginAttempt(LOGIN_GENERATION.fetch_add(1, Ordering::SeqCst) + 1)
}

pub fn cancel_pending_login() {
    LOGIN_GENERATION.fetch_add(1, Ordering::SeqCst);
}

fn login_is_current(generation: u64) -> bool {
    LOGIN_GENERATION.load(Ordering::SeqCst) == generation
}

/// Device-code login compatible with the ChatGPT Codex OAuth flow used by Pi.
/// No external Codex executable or app-server is involved.
pub fn device_login(attempt: LoginAttempt, mut notify: impl FnMut(LoginEvent)) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(runtime) = runtime else {
        notify(LoginEvent::Error);
        return;
    };
    let result = runtime.block_on(device_login_async(attempt.0, &mut notify));
    if let Err(error) = result {
        if error.to_string() != "login cancelled" {
            log::warn!("[suflyor-codex-direct] OAuth login failed");
            notify(LoginEvent::Error);
        }
    }
}

async fn device_login_async(
    generation: u64,
    notify: &mut impl FnMut(LoginEvent),
) -> Result<()> {
    if !login_is_current(generation) {
        return Err(anyhow!("login cancelled"));
    }
    if let Ok(Some(credential)) = valid_credential().await {
        notify(LoginEvent::SignedIn {
            plan: credential.plan,
        });
        return Ok(());
    }

    let client = crate::ai::control::http_client();
    let response = client
        .post(DEVICE_USER_CODE_URL)
        .timeout(Duration::from_secs(20))
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .context("start direct Codex device login")?;
    if !response.status().is_success() {
        return Err(anyhow!("device login unavailable"));
    }
    let value: Value = response.json().await.context("parse device login")?;
    let interval = value
        .get("interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(5)
        .clamp(1, 30);
    let device = DeviceAuth {
        id: required_string(&value, "device_auth_id")?,
        code: required_string(&value, "user_code")?,
        interval: Duration::from_secs(interval),
    };
    notify(LoginEvent::AwaitingUser {
        verification_url: DEVICE_VERIFICATION_URL.to_string(),
        user_code: device.code.clone(),
    });

    let deadline = std::time::Instant::now() + LOGIN_TIMEOUT;
    let (authorization_code, code_verifier) = loop {
        if !login_is_current(generation) {
            return Err(anyhow!("login cancelled"));
        }
        if std::time::Instant::now() >= deadline {
            notify(LoginEvent::SignInRequired);
            return Ok(());
        }
        tokio::time::sleep(device.interval).await;
        if !login_is_current(generation) {
            return Err(anyhow!("login cancelled"));
        }
        let response = client
            .post(DEVICE_TOKEN_URL)
            .timeout(Duration::from_secs(20))
            .json(&json!({
                "device_auth_id": device.id,
                "user_code": device.code,
            }))
            .send()
            .await
            .context("poll direct Codex device login")?;
        if response.status().is_success() {
            let value: Value = response.json().await.context("parse device token")?;
            break (
                required_string(&value, "authorization_code")?,
                required_string(&value, "code_verifier")?,
            );
        }
        let status = response.status().as_u16();
        if status == 403 || status == 404 {
            continue;
        }
        let body = response.text().await.unwrap_or_default();
        let error_code = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value.get("error").and_then(|error| {
                    error
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| error.get("code").and_then(Value::as_str).map(str::to_string))
                })
            });
        if matches!(error_code.as_deref(), Some("deviceauth_authorization_pending")) {
            continue;
        }
        if matches!(error_code.as_deref(), Some("slow_down")) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        return Err(anyhow!("device authorization failed"));
    };

    let response = client
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", authorization_code.as_str()),
            ("code_verifier", code_verifier.as_str()),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ])
        .send()
        .await
        .context("exchange direct Codex authorization code")?;
    let credential = parse_and_store_token_response(response).await?;
    if !login_is_current(generation) {
        return Err(anyhow!("login cancelled"));
    }
    notify(LoginEvent::SignedIn {
        plan: credential.plan,
    });
    Ok(())
}

#[must_use]
pub fn provider_snapshot() -> ProviderSnapshot {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(runtime) = runtime else {
        return error_snapshot();
    };
    runtime
        .block_on(provider_snapshot_async())
        .unwrap_or_else(|_| error_snapshot())
}

async fn provider_snapshot_async() -> Result<ProviderSnapshot> {
    let Some(credential) = valid_credential().await? else {
        return Ok(ProviderSnapshot {
            account: AccountState::SignedOut,
            models: Vec::new(),
            rate_limits: None,
        });
    };
    let models = fetch_models(&credential).await?;
    Ok(ProviderSnapshot {
        account: AccountState::SignedIn {
            plan: credential.plan,
        },
        models,
        rate_limits: None,
    })
}

fn error_snapshot() -> ProviderSnapshot {
    ProviderSnapshot {
        account: AccountState::Error,
        models: Vec::new(),
        rate_limits: None,
    }
}

#[must_use]
pub fn disconnect() -> AccountState {
    cancel_pending_login();
    let access = credentials::delete(SecretSlot::CodexDirectAccess);
    let refresh = credentials::delete(SecretSlot::CodexDirectRefresh);
    if access.is_ok() && refresh.is_ok() {
        AccountState::SignedOut
    } else {
        AccountState::Error
    }
}

pub async fn complete_turn(
    endpoint: &AiEndpoint,
    messages: &[ChatMessage],
) -> Result<(String, TokenUsage)> {
    let started = std::time::Instant::now();
    let credential = valid_credential()
        .await?
        .ok_or_else(|| anyhow!("ChatGPT sign-in is required"))?;
    let mut response = send_response_request(endpoint, messages, &credential).await?;
    let mut decoder = SseDecoder::default();
    let mut text = String::new();
    while let Some(chunk) = response.chunk().await.context("read direct Codex stream")? {
        for value in decoder.push(&chunk) {
            match codex_event(&value) {
                DirectEvent::Start(_) | DirectEvent::Ignore => {}
                DirectEvent::Delta(delta) => text.push_str(&delta),
                DirectEvent::Done {
                    reason,
                    input_tokens,
                    output_tokens,
                } => {
                    return Ok((
                        text,
                        TokenUsage {
                            input: input_tokens,
                            output: output_tokens,
                            tok_per_sec: if started.elapsed().as_secs_f64() > 0.0 && output_tokens > 0 {
                                output_tokens as f64 / started.elapsed().as_secs_f64()
                            } else {
                                0.0
                            },
                            finish_reason: reason,
                        },
                    ));
                }
                DirectEvent::Failed => return Err(anyhow!("Codex answer unavailable")),
            }
        }
    }
    Err(anyhow!("Codex stream ended before a terminal response"))
}

pub async fn stream_turn(
    endpoint: &AiEndpoint,
    messages: &[ChatMessage],
    tx: &mpsc::Sender<AiEvent>,
) -> Result<()> {
    let credential = valid_credential()
        .await?
        .ok_or_else(|| anyhow!("ChatGPT sign-in is required"))?;
    let mut response = send_response_request(endpoint, messages, &credential).await?;
    let mut decoder = SseDecoder::default();
    let mut started = false;
    while let Some(chunk) = response.chunk().await.context("read direct Codex stream")? {
        for value in decoder.push(&chunk) {
            match codex_event(&value) {
                DirectEvent::Start(id) => {
                    started = true;
                    if tx.send(AiEvent::Start { id }).await.is_err() {
                        return Ok(());
                    }
                }
                DirectEvent::Delta(text) => {
                    if !started {
                        started = true;
                        if tx
                            .send(AiEvent::Start {
                                id: "codex-direct".into(),
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    if tx.send(AiEvent::Delta { text }).await.is_err() {
                        return Ok(());
                    }
                }
                DirectEvent::Done { reason, .. } => {
                    let _ = tx.send(AiEvent::Done { reason }).await;
                    return Ok(());
                }
                DirectEvent::Failed => return Err(anyhow!("Codex answer unavailable")),
                DirectEvent::Ignore => {}
            }
        }
    }
    Err(anyhow!("Codex stream ended before a terminal response"))
}

async fn send_response_request(
    endpoint: &AiEndpoint,
    messages: &[ChatMessage],
    credential: &Credential,
) -> Result<reqwest::Response> {
    if endpoint.model.trim().is_empty() {
        return Err(anyhow!("Select an available Codex model"));
    }
    let body = response_body(endpoint, messages)?;
    let url = format!("{CODEX_BASE_URL}/codex/responses");
    if crate::deep_lock::endpoint_blocked(crate::deep_lock::deep_lock_active(), CODEX_BASE_URL) {
        return Err(anyhow!(crate::deep_lock::BLOCKED_ERROR));
    }
    let response = crate::network::send(crate::ai::control::http_client()
        .post(url)
        .timeout(Duration::from_secs(180))
        .bearer_auth(&credential.access)
        .header("chatgpt-account-id", &credential.account_id)
        .header("originator", "suflyor")
        .header("User-Agent", concat!("suflyor/", env!("CARGO_PKG_VERSION")))
        .header("OpenAI-Beta", "responses=experimental")
        .header("Accept", "text/event-stream")
        .json(&body)
        )
        .await
        .context("send direct Codex request")?;
    if !response.status().is_success() {
        let status = response.status();
        let body_len = response.text().await.unwrap_or_default().len();
        log::warn!(
            "{}",
            crate::http_log::http_error_line("Codex direct", status.as_u16(), body_len)
        );
        return Err(anyhow!("HTTP {status}"));
    }
    Ok(response)
}

fn response_body(endpoint: &AiEndpoint, messages: &[ChatMessage]) -> Result<Value> {
    let instructions = messages
        .iter()
        .filter(|message| message.role == "system")
        .filter_map(|message| match &message.content {
            MessageContent::Text(text) => Some(text.as_str()),
            MessageContent::Parts(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = messages
        .iter()
        .filter(|message| message.role != "system")
        .enumerate()
        .map(|(index, message)| direct_input_message(index, message))
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": endpoint.model,
        "store": false,
        "stream": true,
        "instructions": if instructions.is_empty() { "You are a helpful assistant." } else { instructions.as_str() },
        "input": input,
        "text": {"verbosity": "low"},
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(effort) = endpoint
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !matches!(effort, "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra") {
            return Err(anyhow!("Unsupported Codex reasoning effort"));
        }
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    if let Some(tier) = endpoint.service_tier.as_deref() {
        if !matches!(tier, "priority" | "fast") {
            return Err(anyhow!("Unsupported Codex service tier"));
        }
        body["service_tier"] = Value::String("priority".into());
    }
    Ok(body)
}

fn direct_input_message(index: usize, message: &ChatMessage) -> Value {
    if message.role == "assistant" {
        let text = match &message.content {
            MessageContent::Text(text) => text.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        return json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type":"output_text","text":text,"annotations":[]}],
            "status": "completed",
            "id": format!("msg_suflyor_{index}"),
        });
    }
    json!({
        "role": if message.role == "developer" { "developer" } else { "user" },
        "content": direct_input_content(&message.content),
    })
}

fn direct_input_content(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => {
            json!([{"type":"input_text","text":text}])
        }
        MessageContent::Parts(parts) => Value::Array(
            parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => json!({"type":"input_text","text":text}),
                    ContentPart::ImageUrl { image_url } => {
                        json!({"type":"input_image","detail":"auto","image_url":image_url.url})
                    }
                })
                .collect(),
        ),
    }
}

async fn fetch_models(credential: &Credential) -> Result<Vec<CodexModel>> {
    let url = format!(
        "{CODEX_BASE_URL}/codex/models?client_version={CODEX_CLIENT_VERSION}"
    );
    let response = crate::network::send(crate::ai::control::http_client()
        .get(url)
        .timeout(Duration::from_secs(20))
        .bearer_auth(&credential.access)
        .header("chatgpt-account-id", &credential.account_id)
        .header("originator", "suflyor")
        .header("User-Agent", concat!("suflyor/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/json")
        )
        .await
        .context("fetch direct Codex model catalog")?;
    if !response.status().is_success() {
        return Err(anyhow!("Codex model catalog unavailable"));
    }
    let value: Value = response.json().await.context("parse Codex model catalog")?;
    let rows = value
        .get("models")
        .or_else(|| value.get("data"))
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| anyhow!("Codex model catalog has an unsupported shape"))?;
    let mut models = Vec::new();
    for row in rows {
        if row.get("supported_in_api").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if matches!(row.get("visibility").and_then(Value::as_str), Some("hide" | "hidden")) {
            continue;
        }
        let Some(id) = row.get("slug").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
            continue;
        };
        let display_name = row
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(id)
            .to_string();
        let reasoning_efforts = row
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        level
                            .get("effort")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let input_modalities = row
            .get("input_modalities")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| vec!["text".into()]);
        let supports_fast = row
            .get("service_tiers")
            .and_then(Value::as_array)
            .is_some_and(|tiers| {
                tiers.iter().any(|tier| {
                    tier.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| matches!(id, "priority" | "fast"))
                })
            })
            || row
                .get("additional_speed_tiers")
                .and_then(Value::as_array)
                .is_some_and(|tiers| tiers.iter().any(|tier| tier.as_str() == Some("fast")));
        models.push(CodexModel {
            id: id.to_string(),
            display_name,
            is_default: row.get("is_default").and_then(Value::as_bool).unwrap_or(false),
            default_reasoning_effort: row
                .get("default_reasoning_level")
                .and_then(Value::as_str)
                .map(str::to_string),
            reasoning_efforts,
            input_modalities,
            supports_fast,
        });
    }
    Ok(models)
}

async fn valid_credential() -> Result<Option<Credential>> {
    let access = credentials::read(SecretSlot::CodexDirectAccess)?;
    if let Some(access) = access.filter(|value| token_is_fresh(value)) {
        return credential_from_access(access).map(Some);
    }
    if credentials::read(SecretSlot::CodexDirectRefresh)?.is_none() {
        return Ok(None);
    }

    // Multiple tiles can cross the expiry boundary at the same time. Serialize
    // refresh-token rotation, then re-read because another request may already
    // have refreshed the credential while this one waited.
    let _refresh_guard = TOKEN_REFRESH_LOCK.lock().await;
    if let Some(access) = credentials::read(SecretSlot::CodexDirectAccess)?
        .filter(|value| token_is_fresh(value))
    {
        return credential_from_access(access).map(Some);
    }
    let Some(refresh) = credentials::read(SecretSlot::CodexDirectRefresh)? else {
        return Ok(None);
    };
    refresh_credential(&refresh).await.map(Some)
}

async fn refresh_credential(refresh: &str) -> Result<Credential> {
    let response = crate::network::send(crate::ai::control::http_client()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ])
        )
        .await
        .context("refresh direct Codex token")?;
    parse_and_store_token_response(response).await
}

async fn parse_and_store_token_response(response: reqwest::Response) -> Result<Credential> {
    if !response.status().is_success() {
        return Err(anyhow!("ChatGPT token request failed"));
    }
    let value: Value = response.json().await.context("parse ChatGPT token response")?;
    let access = required_string(&value, "access_token")?;
    let refresh = required_string(&value, "refresh_token")?;
    let credential = credential_from_access(access.clone())?;
    // Store the refresh token first so a partial write can still recover.
    credentials::write(SecretSlot::CodexDirectRefresh, &refresh)?;
    credentials::write(SecretSlot::CodexDirectAccess, &access)?;
    Ok(credential)
}

fn credential_from_access(access: String) -> Result<Credential> {
    let payload = jwt_payload(&access)?;
    let auth = payload
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("ChatGPT account claim missing"))?;
    let account_id = auth
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("ChatGPT account id missing"))?
        .to_string();
    let plan = auth
        .get("chatgpt_plan_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(Credential {
        access,
        account_id,
        plan,
    })
}

fn token_is_fresh(access: &str) -> bool {
    let Ok(payload) = jwt_payload(access) else {
        return false;
    };
    let Some(exp) = payload.get("exp").and_then(Value::as_u64) else {
        return false;
    };
    exp > unix_seconds().saturating_add(REFRESH_MARGIN)
}

fn jwt_payload(token: &str) -> Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("invalid ChatGPT token"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .context("decode ChatGPT token")?;
    serde_json::from_slice(&decoded).context("parse ChatGPT token")
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing OAuth response field"))
}

#[derive(Default)]
struct SseDecoder {
    bytes: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.bytes.extend_from_slice(chunk);
        let mut values = Vec::new();
        loop {
            let lf = self.bytes.windows(2).position(|window| window == b"\n\n");
            let crlf = self
                .bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n");
            let (frame_end, consumed) = match (lf, crlf) {
                (Some(left), Some(right)) if right < left => (right, right + 4),
                (Some(left), _) => (left, left + 2),
                (None, Some(right)) => (right, right + 4),
                (None, None) => break,
            };
            let frame = String::from_utf8_lossy(&self.bytes[..frame_end]).replace("\r\n", "\n");
            self.bytes.drain(..consumed);
            for line in frame.lines() {
                let Some(payload) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str(payload) {
                    values.push(value);
                }
            }
        }
        values
    }
}

enum DirectEvent {
    Start(String),
    Delta(String),
    Done {
        reason: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    Failed,
    Ignore,
}

fn codex_event(value: &Value) -> DirectEvent {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "response.created" => value
            .pointer("/response/id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map_or(DirectEvent::Ignore, |id| DirectEvent::Start(id.to_string())),
        "response.output_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map_or(DirectEvent::Ignore, |text| DirectEvent::Delta(text.to_string())),
        "response.completed" | "response.done" | "response.incomplete" => {
            let response = value.get("response").unwrap_or(value);
            DirectEvent::Done {
                reason: if kind == "response.incomplete"
                    || response.get("status").and_then(Value::as_str) == Some("incomplete")
                {
                    response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("length")
                        .to_string()
                } else {
                    "stop".into()
                },
                input_tokens: response
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output_tokens: response
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            }
        }
        "response.failed" | "response.cancelled" | "error" => DirectEvent::Failed,
        _ => DirectEvent::Ignore,
    }
}

pub fn allowed_signin_url(value: &str) -> bool {
    value == DEVICE_VERIFICATION_URL || value.starts_with(&format!("{AUTH_BASE_URL}/"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn device_signin_url_is_restricted_to_openai_auth() {
        assert!(allowed_signin_url(DEVICE_VERIFICATION_URL));
        assert!(!allowed_signin_url("https://example.com/codex/device"));
    }

    #[test]
    fn sse_decoder_handles_crlf_frames() {
        let mut decoder = SseDecoder::default();
        let values = decoder.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\n\r\n");
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn direct_request_body_carries_reasoning_fast_and_replayed_assistant() {
        let endpoint = AiEndpoint {
            protocol: crate::ai::AiProtocol::CodexDirect,
            base_url: CODEX_BASE_URL.into(),
            bearer: String::new(),
            model: "gpt-test".into(),
            reasoning_effort: Some("high".into()),
            service_tier: Some("priority".into()),
            is_local: false,
        };
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: MessageContent::Text("system prompt".into()),
            },
            ChatMessage {
                role: "user".into(),
                content: MessageContent::Text("question".into()),
            },
            ChatMessage {
                role: "assistant".into(),
                content: MessageContent::Text("previous answer".into()),
            },
        ];

        let body = response_body(&endpoint, &messages).expect("request body");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
    }
}
