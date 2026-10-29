//! Explicit cloud HTTP routing: system, direct->proxy, proxy->direct,
//! direct-only, and proxy-only. Only retry when a transport/path is failing;
//! never retry successful requests or authentication (401) failures.
use crate::config::Config;
use anyhow::{anyhow, Result};
use std::sync::{OnceLock, RwLock, Mutex};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Default)]
struct RouteSettings {
    mode: i32,
    url: String,
}

static ROUTING: OnceLock<RwLock<RouteSettings>> = OnceLock::new();
// Reuse TLS sessions and sockets; building a new reqwest Client on every
// STT fragment would add substantial latency.
static CLIENTS: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();

fn settings() -> RouteSettings {
    let lock = ROUTING.get_or_init(|| {
        let cfg = crate::config::load();
        RwLock::new(RouteSettings { mode: cfg.network_proxy_mode, url: cfg.network_proxy_url })
    });
    lock.read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn update_if_initialized(cfg: &Config) {
    if let Some(lock) = ROUTING.get() {
        let mut value = lock.write().unwrap_or_else(|e| e.into_inner());
        if value.mode != cfg.network_proxy_mode || value.url != cfg.network_proxy_url {
            *value = RouteSettings {
                mode: cfg.network_proxy_mode,
                url: cfg.network_proxy_url.clone(),
            };
            if let Some(cache) = CLIENTS.get() {
                cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
        }
    }
}

pub fn validate(mode: i32, url: &str) -> Result<()> {
    if !(0..=4).contains(&mode) { return Err(anyhow!("Invalid proxy mode")); }
    if mode == 0 || mode == 3 { return Ok(()); }
    if url.trim().is_empty() { return Err(anyhow!("Enter a proxy URL first")); }
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|_| anyhow!("Invalid proxy URL"))?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h")
        || parsed.host_str().is_none() {
        return Err(anyhow!("Proxy must use http://, https:// or socks5://"));
    }
    reqwest::Proxy::all(url.trim()).map_err(|_| anyhow!("Invalid proxy address"))?;
    Ok(())
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Route { System, Direct, Proxy }

fn is_private_target(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return true };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost")
       || host.ends_with(".local") { return true; }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v) => {
                v.is_private() || v.is_loopback() || v.is_link_local()
                || (v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
            },
            std::net::IpAddr::V6(v) => v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local(),
        };
    }
    false
}

fn routes(url: &reqwest::Url, config: &RouteSettings) -> Vec<Route> {
    if is_private_target(url) { return vec![Route::Direct]; }
    match config.mode {
        1 => vec![Route::Direct, Route::Proxy],
        2 => vec![Route::Proxy, Route::Direct],
        3 => vec![Route::Direct],
        4 => vec![Route::Proxy],
        _ => vec![Route::System],
    }
}

fn client(route: Route, config: &RouteSettings) -> Result<reqwest::Client> {
    let key = match route {
        Route::System => "system".to_owned(),
        Route::Direct => "direct".to_owned(),
        Route::Proxy => format!("proxy:{}", config.url),
    };
    let cache = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut clients = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(client) = clients.get(&key) { return Ok(client.clone()); }
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(7))
        .timeout(Duration::from_secs(210));
    let builder = match route {
        Route::System => builder,
        Route::Direct => builder.no_proxy(),
        Route::Proxy => builder.proxy(reqwest::Proxy::all(config.url.trim())
            .map_err(|_| anyhow!("Invalid proxy configuration"))?),
    };
    let built = builder.build().map_err(|_| anyhow!("Cannot initialize HTTP route"))?;
    clients.insert(key, built.clone());
    Ok(built)
}

fn retry_route(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 403 | 407 | 502 | 503 | 504)
}

fn retry_transport(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || err.is_request()
}

/// Requests with re-creatable bodies (including multipart WAV upload).
/// Closure is invoked again only when a route fails before a usable HTTP reply.
/// Provider errors such as 400/401/429 do not change the route.
pub async fn send_rebuildable<F>(url: &str, mut build: F) -> Result<reqwest::Response>
where F: FnMut(&reqwest::Client) -> Result<reqwest::RequestBuilder> {
    let target = reqwest::Url::parse(url).map_err(|_| anyhow!("Invalid request URL"))?;
    let config = settings();
    let candidates = routes(&target, &config);
    let count = candidates.len();
    for (index, route) in candidates.into_iter().enumerate() {
        let client = client(route, &config)?;
        let started = std::time::Instant::now();
        match build(&client)?.send().await {
            Ok(response) => {
                log::info!("[route] service={} path={route:?} attempt={}/{} http={} elapsed_ms={}",
                    provider_label(&target), index + 1, count,
                    response.status().as_u16(), started.elapsed().as_millis());
                if index + 1 < count && retry_route(response.status()) {
                    log::warn!("Network route {:?} returned retryable status {}; trying fallback",
                        route, response.status().as_u16());
                    continue;
                }
                if index > 0 { log::info!("Network fallback route {:?} succeeded", route); }
                return Ok(response);
            }
            Err(error) => {
                log::warn!("[route] service={} path={route:?} attempt={}/{} error={} elapsed_ms={}",
                    provider_label(&target), index + 1, count,
                    if error.is_timeout() { "timeout" } else if error.is_connect() { "connect" } else { "transport" },
                    started.elapsed().as_millis());
                if index + 1 < count && retry_transport(&error) {
                    log::warn!("Network route {:?} failed; trying fallback", route);
                    continue;
                }
                return Err(anyhow!("Network request failed ({})", if error.is_timeout() {"timeout"} else {"transport"}));
            }
        }
    }
    Err(anyhow!("No available network route"))
}

/// Immutable JSON/GET requests can be cloned and submitted on another route.
/// An uncloneable body must instead use `send_rebuildable` above.
pub async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let (original, request) = request.build_split();
    let request = request.map_err(|_| anyhow!("Cannot construct HTTP request"))?;
    let config = settings();
    let candidates = routes(request.url(), &config);
    let count = candidates.len();
    let mut next = Some(request);
    for (index, route) in candidates.into_iter().enumerate() {
        let request = next.take().ok_or_else(|| anyhow!("HTTP body cannot be retried"))?;
        let backup = if index + 1 < count { request.try_clone() } else { None };
        let client = if route == Route::System { original.clone() } else { client(route, &config)? };
        let service = provider_label(request.url());
        let started = std::time::Instant::now();
        match client.execute(request).await {
            Ok(resp) => {
                log::info!("[route] service={} path={route:?} attempt={}/{} http={} elapsed_ms={}",
                    service, index + 1, count, resp.status().as_u16(), started.elapsed().as_millis());
                if index + 1 < count && retry_route(resp.status()) && backup.is_some() {
                    log::warn!("Network route {:?}: retryable HTTP {}; trying fallback", route, resp.status().as_u16());
                    next = backup;
                    continue;
                }
                if index > 0 { log::info!("Network fallback route {:?} succeeded", route); }
                return Ok(resp);
            },
            Err(err) => {
                log::warn!("[route] service={} path={route:?} attempt={}/{} error={} elapsed_ms={}",
                    service, index + 1, count,
                    if err.is_timeout() { "timeout" } else if err.is_connect() { "connect" } else { "transport" },
                    started.elapsed().as_millis());
                if index + 1 < count && retry_transport(&err) && backup.is_some() {
                    log::warn!("Network route {:?} failed; trying fallback", route);
                    next = backup;
                    continue;
                }
                return Err(anyhow!("Network request failed ({})", if err.is_timeout() {"timeout"} else {"transport"}));
            }
        }
    }
    Err(anyhow!("No available network route"))
}

/// Non-billable route probe used only by the Diagnostics tab. An HTTP response
/// (including 401/404/405) proves reachability, NOT API key/model correctness.
/// Does not send a user's credentials or text. URLs/hosts never enter the result.
pub async fn diagnostic_probe(url: &str, use_proxy: bool) -> Result<u16> {
    let target = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid endpoint"))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(anyhow!("unsupported endpoint protocol"));
    }
    if use_proxy && is_private_target(&target) {
        return Err(anyhow!("local endpoint: proxy bypassed"));
    }
    let cfg = settings();
    if use_proxy && cfg.url.trim().is_empty() {
        return Err(anyhow!("proxy not configured"));
    }
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(7));
    builder = if use_proxy {
        builder.proxy(reqwest::Proxy::all(cfg.url.trim())
            .map_err(|_| anyhow!("invalid proxy configuration"))?)
    } else {
        builder.no_proxy()
    };
    let client = builder.build().map_err(|_| anyhow!("client initialization failed"))?;
    let response = client.head(target).send().await.map_err(|err| {
        anyhow!(if err.is_timeout() { "timeout" }
            else if err.is_connect() { "connection failed" }
            else { "transport failure" })
    })?;
    Ok(response.status().as_u16())
}

/// Model/service names for logs must not contain URL credentials or custom
/// hostnames. This labels only known providers; others become `custom`.
fn provider_label(url: &reqwest::Url) -> &'static str {
    match url.host_str().unwrap_or("") {
        "api.groq.com" => "groq",
        "api.elevenlabs.io" => "elevenlabs",
        "api.deepgram.com" => "deepgram",
        "eu1.asr.api.speechmatics.com" => "speechmatics",
        "api.openai.com" => "openai",
        "api.anthropic.com" => "anthropic",
        _ => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valid_modes_and_urls() {
        assert!(validate(1, "http://127.0.0.1:8080").is_ok());
        assert!(validate(2, "socks5://127.0.0.1:1080").is_ok());
        assert!(validate(1, "").is_err());
        assert!(validate(4, "file:///etc/passwd").is_err());
        assert!(validate(0, "").is_ok());
    }
    #[test]
    fn private_targets_bypass_proxy() {
        let url = reqwest::Url::parse("http://192.168.1.10:33335/mcp").unwrap();
        assert_eq!(routes(&url, &RouteSettings {mode:2,url:"http://proxy:8080".into()}), vec![Route::Direct]);
    }
    #[test]
    fn diagnostic_provider_labels_are_not_urls() {
        let public = reqwest::Url::parse("https://api.groq.com/openai/v1/audio/transcriptions").unwrap();
        assert_eq!(provider_label(&public), "groq");
        let custom = reqwest::Url::parse("https://alice:password@private.example/v1").unwrap();
        assert_eq!(provider_label(&custom), "custom");
    }
    #[test]
    fn explicit_route_includes_only_configured_candidates() {
        let url = reqwest::Url::parse("https://api.groq.com/openai/v1/models").unwrap();
        let conf = RouteSettings { mode: 1, url: "http://127.0.0.1:7890".into() };
        assert_eq!(routes(&url, &conf), vec![Route::Direct, Route::Proxy]);
    }
    #[test]
    fn route_order() {
        let url = reqwest::Url::parse("https://api.groq.com/v1/models").unwrap();
        let s = RouteSettings { mode:1, url:"http://localhost:8080".into() };
        assert_eq!(routes(&url, &s), vec![Route::Direct, Route::Proxy]);
    }
}
