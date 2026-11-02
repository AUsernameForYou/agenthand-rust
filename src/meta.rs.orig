//! The Meta Model API client and the identify contract — port of the
//! live-client half of the Python `meta_client.py`.
//!
//! Transport: reqwest (blocking, rustls), retries DISABLED (reqwest
//! does not retry by default) — an SDK-level retry would re-run a
//! billable call behind the gate's back. Every function here performs
//! network I/O and must only run inside `billing::billed_call`.
//!
//! Timeouts: every Meta call runs under a per-call timeout of
//! [`DEFAULT_HTTP_TIMEOUT_S`] (180 s) covering the whole request
//! (connect, send, the provider's think time, and the response
//! read), plus a connect timeout of [`HTTP_CONNECT_TIMEOUT_S`]
//! (20 s). reqwest's blocking client otherwise applies its own
//! 30 s default, and a live identify call measured 28.3 s
//! (1.4 s of headroom), so the default would cut off healthy,
//! already-billable calls. `AGENTHAND_HTTP_TIMEOUT_S` (a whole
//! number of seconds, > 0) overrides the per-call timeout; an empty,
//! unparseable or zero value keeps the 180 s default. The value is
//! read when the client is built (`build_client`), and applies to
//! the direct client and the proxy fallback alike.

use crate::config;
use crate::image::decode_image;
use crate::pricing::{self, Usage};
use base64::Engine;
use serde_json::{json, Value};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

/// Base URL of the Meta Model API.
pub const META_BASE_URL: &str = "https://api.meta.ai/v1";
/// Output-token budget for one identify call.
pub const IDENTIFY_MAX_TOKENS: i64 = 2048;
/// Bounded retry budget for identify (body failures only).
pub const IDENTIFY_MAX_ATTEMPTS: usize = 3;
/// Bounded attempt budget for one cut_out call.
pub const SAM_MAX_ATTEMPTS: usize = 2;

const IDENTIFY_PROMPT_TOKENS: i64 = 300;

/// Default per-call timeout for every Meta API request, in seconds
/// (see the module docs; overridable via [`HTTP_TIMEOUT_ENV`]).
pub const DEFAULT_HTTP_TIMEOUT_S: u64 = 180;
/// Connect timeout for every Meta API request, in seconds.
pub const HTTP_CONNECT_TIMEOUT_S: u64 = 20;
/// Environment variable that overrides [`DEFAULT_HTTP_TIMEOUT_S`].
pub const HTTP_TIMEOUT_ENV: &str = "AGENTHAND_HTTP_TIMEOUT_S";

/// Model behind the identify tool.
pub const IDENTIFY_MODEL: &str = "muse-spark-1.3";
/// Model behind the cut_out tool.
pub const CUT_OUT_MODEL: &str = "sam-3.1";
/// Model that generates draw's background plate.
pub const DRAW_BACKGROUND_MODEL: &str = "muse-image-1.0";

/// Raised when a real call is attempted with no META_API_KEY. Raised
/// BEFORE any reservation is written.
pub fn require_meta_api_key() -> Result<String, String> {
    config::get_meta_api_key().ok_or_else(|| {
        "META_API_KEY is not set (environment or .env); the Meta Model API is not configured, so no billable call was made."
            .to_string()
    })
}

/// The Meta API HTTP transport: a direct primary client plus an
/// optional proxy fallback, used as one unit by every Meta call.
///
/// Two environment classes broke the naive client:
///
/// 1. Intercepting proxies (2026-10-27): reqwest auto-detects proxy
///    env vars, and a TLS-intercepting proxy presents certificates
///    issued by its own local CA, which rustls (webpki roots)
///    rightly rejects as UnknownIssuer; that is how every live call
///    from this VM died at connect time during the port's first
///    live run. Direct egress presents the genuine chain, so the
///    PRIMARY client bypasses proxies. (Python's httpx stack never
///    hit this: it validates against the OS store.)
/// 2. Stub-resolver egress policies (2026-10-29,
///    M6-DISCRIMINATOR + M6-DNS-SCOPE in the hackathon record):
///    the musl static build's libc stub resolver sends DNS from an
///    UNCONNECTED UDP socket, and a class of sandboxed egress
///    policies refuses unconnected UDP sends outright (EPERM at
///    sendto), so the shipped Linux binary could not resolve
///    api.meta.ai at all while glibc/Python (connected-socket
///    stubs) worked. The primary client therefore does not depend
///    on the libc stub: it resolves through hickory against the
///    resolv.conf nameservers over TCP — a connected stream,
///    treated as ordinary egress — chained with the system stub
///    as a second chance inside the resolver.
///
/// FALLBACK: when the primary attempt fails with a connect-class
/// error — DNS failure, refused/reset connect, TLS handshake;
/// states in which no request ever reached the provider, so
/// nothing billable happened and a retry cannot double-settle —
/// the request is sent ONCE through a second client that follows
/// the environment proxy and verifies TLS against webpki roots
/// PLUS the operating system's CA bundle (the narration client's
/// proven pattern in roomrun.rs; the same trust set Python and
/// every OS tool use — verification is widened to the OS store,
/// never skipped). Mid-response failures never retry: the request
/// may already be billable at that point.
pub struct MetaClient {
    direct: reqwest::blocking::Client,
    fallback: Option<reqwest::blocking::Client>,
    timeout: std::time::Duration,
}

/// Resolve the per-call timeout from a raw `AGENTHAND_HTTP_TIMEOUT_S`
/// value: a whole number of seconds greater than zero, else the
/// [`DEFAULT_HTTP_TIMEOUT_S`] default. Pure, so the rule is testable
/// without touching the process environment.
pub fn http_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_S);
    std::time::Duration::from_secs(secs)
}

/// The per-call timeout `build_client` applies, read from the
/// environment (`AGENTHAND_HTTP_TIMEOUT_S`) at call time.
pub fn http_timeout() -> std::time::Duration {
    http_timeout_from(std::env::var(HTTP_TIMEOUT_ENV).ok().as_deref())
}

impl MetaClient {
    /// The per-call timeout this client was built with.
    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    /// Send via the direct client; on a connect-class error, retry
    /// once via the fallback client when one was built.
    fn execute(
        &self,
        send: impl Fn(&reqwest::blocking::Client) -> Result<reqwest::blocking::Response, reqwest::Error>,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        match send(&self.direct) {
            Ok(response) => Ok(response),
            Err(err) => {
                if err.is_connect() {
                    if let Some(fallback) = &self.fallback {
                        return send(fallback);
                    }
                }
                Err(err)
            }
        }
    }
}

/// Render an error with its complete source chain ("top: cause:
/// root cause"). reqwest's top level is nearly opaque ("error
/// sending request for url ..."); the actionable fact lives in the
/// chain — flattening it cost a day of diagnosis in the 2026-10-29
/// DNS incident, so send/read failures in this module render it.
pub fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// A DNS resolver for reqwest that transmits only over connected
/// streams: hickory against the resolv.conf nameservers with the
/// protocol forced to TCP, chained with the system stub as a
/// second chance (for networks where UDP is the only DNS path).
/// None when no system nameserver configuration can be read; the
/// caller then keeps reqwest's default resolution.
struct TcpDnsResolver {
    resolver: hickory_resolver::TokioResolver,
}

fn build_tcp_dns_resolver() -> Option<TcpDnsResolver> {
    let (system, mut opts) = hickory_resolver::system_conf::read_system_conf().ok()?;
    let mut config = hickory_resolver::config::ResolverConfig::new();
    if let Some(domain) = system.domain() {
        config.set_domain(domain.clone());
    }
    for search in system.search() {
        config.add_search(search.clone());
    }
    let mut any = false;
    for ns in system.name_servers() {
        config.add_name_server(hickory_resolver::config::NameServerConfig::new(
            ns.socket_addr,
            hickory_resolver::proto::xfer::Protocol::Tcp,
        ));
        any = true;
    }
    if !any {
        return None;
    }
    // One short attempt per nameserver: this resolver is the first
    // of two chances, and a black-holed TCP/53 should not stall the
    // caller for hickory's default retry budget.
    opts.timeout = std::time::Duration::from_secs(3);
    opts.attempts = 1;
    let mut builder = hickory_resolver::TokioResolver::builder_with_config(
        config,
        hickory_resolver::name_server::TokioConnectionProvider::default(),
    );
    *builder.options_mut() = opts;
    Some(TcpDnsResolver {
        resolver: builder.build(),
    })
}

impl reqwest::dns::Resolve for TcpDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let resolver = self.resolver.clone();
        Box::pin(async move {
            match resolver.lookup_ip(host.as_str()).await {
                Ok(lookup) => {
                    let addrs: Vec<SocketAddr> =
                        lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
                    if addrs.is_empty() {
                        return Err(format!("DNS lookup for {host} returned no addresses").into());
                    }
                    Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
                }
                Err(tcp_err) => {
                    // Second chance: the system stub, consulted
                    // synchronously on the runtime thread. Reached
                    // only when TCP DNS failed, which on a healthy
                    // network is never; the stub answers fast where
                    // it works, and where it cannot it costs one
                    // retry schedule, no worse than pre-fix.
                    match (host.as_str(), 0).to_socket_addrs() {
                        Ok(iter) => {
                            let addrs: Vec<SocketAddr> = iter.collect();
                            if addrs.is_empty() {
                                Err(format!(
                                    "DNS lookup for {host} failed: TCP resolver: {tcp_err}; \
                                     system resolver returned no addresses"
                                )
                                .into())
                            } else {
                                Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
                            }
                        }
                        Err(stub_err) => Err(format!(
                            "DNS lookup for {host} failed: TCP resolver: {tcp_err}; \
                             system resolver: {stub_err}"
                        )
                        .into()),
                    }
                }
            }
        })
    }
}

/// The fallback client: environment proxy (auto-detected, NOT
/// bypassed) plus the OS CA bundle added to reqwest's roots — the
/// narration client's bundle-loading pattern from roomrun.rs.
/// None when no proxy is configured in the environment: without
/// one, a second client would just repeat the direct attempt.
fn build_fallback_client(timeout: std::time::Duration) -> Option<reqwest::blocking::Client> {
    const PROXY_VARS: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    if !PROXY_VARS
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
    {
        return None;
    }
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(std::time::Duration::from_secs(HTTP_CONNECT_TIMEOUT_S));
    // Load the operating-system CA bundle so a TLS endpoint (or an
    // intercepting proxy) chaining to a locally trusted root
    // verifies. Failure to read the bundle is not fatal: webpki
    // roots still apply.
    let bundle_path = std::env::var("SSL_CERT_FILE")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/etc/ssl/certs/ca-certificates.crt".to_string());
    if let Ok(pem_text) = std::fs::read_to_string(&bundle_path) {
        for chunk in pem_text.split("-----END CERTIFICATE-----") {
            let pem = format!("{chunk}-----END CERTIFICATE-----");
            if !pem.contains("-----BEGIN CERTIFICATE-----") {
                continue;
            }
            if let Ok(cert) = reqwest::Certificate::from_pem(pem.as_bytes()) {
                builder = builder.add_root_certificate(cert);
            }
        }
    }
    builder.build().ok()
}

/// Build the shared Meta API transport (see [`MetaClient`]).
/// The per-call timeout is [`http_timeout`] (180 s unless
/// `AGENTHAND_HTTP_TIMEOUT_S` overrides it) with a 20 s connect
/// timeout, on the direct client and the fallback alike: reqwest's
/// own 30 s default left a 28.3 s identify call 1.4 s of headroom.
pub fn build_client() -> Result<MetaClient, String> {
    let timeout = http_timeout();
    let mut direct_builder = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(std::time::Duration::from_secs(HTTP_CONNECT_TIMEOUT_S));
    if let Some(resolver) = build_tcp_dns_resolver() {
        direct_builder = direct_builder.dns_resolver(Arc::new(resolver));
    }
    let direct = direct_builder
        .build()
        .map_err(|e| format!("cannot build HTTP client: {}", error_chain(&e)))?;
    Ok(MetaClient {
        direct,
        fallback: build_fallback_client(timeout),
        timeout,
    })
}

fn post_json(client: &MetaClient, key: &str, path: &str, body: &Value) -> Result<Value, String> {
    let response = client
        .execute(|c| {
            c.post(format!("{META_BASE_URL}{path}"))
                .bearer_auth(key)
                .json(body)
                .send()
        })
        .map_err(|e| format!("Meta API request to {path} failed: {}", error_chain(&e)))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("Meta API response read failed: {}", error_chain(&e)))?;
    if !status.is_success() {
        return Err(format!(
            "Meta API {path} returned HTTP {status}: {}",
            text.chars().take(500).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("Meta API {path} returned non-JSON: {e}"))
}

/// POST a prebuilt body (multipart for the edit endpoint); same
/// transport, error shape and JSON parsing as `post_json`.
fn post_body(
    client: &MetaClient,
    key: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Result<Value, String> {
    let response = client
        .execute(|c| {
            c.post(format!("{META_BASE_URL}{path}"))
                .bearer_auth(key)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.to_vec())
                .send()
        })
        .map_err(|e| format!("Meta API request to {path} failed: {}", error_chain(&e)))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("Meta API response read failed: {}", error_chain(&e)))?;
    if !status.is_success() {
        return Err(format!(
            "Meta API {path} returned HTTP {status}: {}",
            text.chars().take(500).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("Meta API {path} returned non-JSON: {e}"))
}

// ---------------------------------------------------------------------------
// identify: schema, instructions, parsing, phrase hygiene, repair.
// ---------------------------------------------------------------------------

fn identify_schema() -> Value {
    json!({
        "name": "identify_result",
        "strict": true,
        "schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "description": {"type": "string"},
                "concept": {"type": "string"},
                "alternates": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 2
                },
                "suggested_title": {"type": "string"},
                "listing_bullets": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 3,
                    "maxItems": 5
                },
                "condition_notes": {"type": "string"}
            },
            "required": [
                "description", "concept", "alternates", "suggested_title",
                "listing_bullets", "condition_notes"
            ]
        }
    })
}

const IDENTIFY_INSTRUCTIONS: &str = "You are the planning step of a listing-production pipeline. Look at the product photo and return the structured result. Be brief in every field; the length limits are hard. description: a short factual description of the product, at most 40 words. concept: ONE short noun phrase naming the product, suitable for text-prompted image segmentation (e.g. 'brown leather wallet') — the product only, no background, no adjectives about the scene. Name the physical object itself, never the screen, photo, or frame it appears in. alternates: at least 2 different fallback noun phrases for the same product, for use if the first phrase segments poorly. suggested_title: a marketplace listing title. listing_bullets: 3 to 5 short selling points a buyer can verify from the photo, each at most 12 words. condition_notes: visible wear, damage, or defects, in at most 25 words; say so plainly, or state that none are visible.";

/// An identify failure. `raw_response` is Some when the failure was an
/// unparseable response body (the retryable kind); `body_failure()`
/// mirrors `is_identify_body_failure`.
#[derive(Debug)]
pub struct IdentifyError {
    pub message: String,
    pub raw_response: Option<String>,
    pub debug_path: Option<String>,
}

impl IdentifyError {
    pub fn new(message: &str) -> Self {
        IdentifyError {
            message: message.to_string(),
            raw_response: None,
            debug_path: None,
        }
    }
    pub fn body_failure(&self) -> bool {
        self.raw_response.is_some()
    }
}

impl std::fmt::Display for IdentifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

// -- phrase hygiene (port of the deterministic sanitizer) -------------------

const FRAME_HEAD_NOUNS: [&str; 11] = [
    "screen",
    "display",
    "monitor",
    "photo",
    "photograph",
    "image",
    "picture",
    "background",
    "scene",
    "frame",
    "screenshot",
];

fn phrase_tokens(phrase: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let lower = phrase.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_ascii_alphanumeric() {
            current.push(*c);
        } else if *c == '\''
            && i + 1 < chars.len()
            && chars[i + 1].is_ascii_alphabetic()
            && !current.is_empty()
        {
            current.push(*c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_frame_phrase(phrase: &str) -> bool {
    let tokens = phrase_tokens(phrase);
    if tokens.is_empty() {
        return true;
    }
    let mut head = tokens[tokens.len() - 1].as_str();
    if head.ends_with('s') && FRAME_HEAD_NOUNS.contains(&&head[..head.len() - 1]) {
        head = &head[..head.len() - 1];
    }
    FRAME_HEAD_NOUNS.contains(&head)
}

fn rewrite_frame_phrase(phrase: &str) -> Option<String> {
    let mut tokens = phrase_tokens(phrase);
    while !tokens.is_empty() && is_frame_phrase(&tokens.join(" ")) {
        tokens.pop();
    }
    while !tokens.is_empty() && ["the", "a", "an"].contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

const DESCRIPTION_CUT_TOKENS: [&str; 15] = [
    "on",
    "in",
    "against",
    "with",
    "beside",
    "near",
    "showing",
    "displaying",
    "featuring",
    "sits",
    "sitting",
    "placed",
    "lying",
    "standing",
    "resting",
];

fn phrase_from_description(description: &str) -> Option<String> {
    let head_text = description.split(['.', ',', ';', ':']).next().unwrap_or("");
    let mut tokens = phrase_tokens(head_text);
    while !tokens.is_empty() && ["the", "a", "an"].contains(&tokens[0].as_str()) {
        tokens.remove(0);
    }
    let mut kept: Vec<String> = Vec::new();
    for token in tokens {
        if DESCRIPTION_CUT_TOKENS.contains(&token.as_str()) || token == "of" {
            break;
        }
        if FRAME_HEAD_NOUNS.contains(&token.as_str())
            || (token.ends_with('s') && FRAME_HEAD_NOUNS.contains(&&token[..token.len() - 1]))
        {
            continue;
        }
        kept.push(token);
    }
    if kept.is_empty() || is_frame_phrase(&kept.join(" ")) {
        return None;
    }
    Some(kept.join(" "))
}

/// Returns (concept, alternates); concept None when nothing usable
/// survives (the repairable shortfall).
fn sanitize_plan_phrases(
    concept: &str,
    alternates: &[String],
    description: &str,
) -> (Option<String>, Vec<String>) {
    let mut pool: Vec<String> = Vec::new();
    let add = |pool: &mut Vec<String>, phrase: Option<String>| {
        if let Some(p) = phrase {
            if !pool.iter().any(|q| q.to_lowercase() == p.to_lowercase()) {
                pool.push(p);
            }
        }
    };
    let all: Vec<&str> = std::iter::once(concept)
        .chain(alternates.iter().map(|s| s.as_str()))
        .collect();
    for phrase in &all {
        if !is_frame_phrase(phrase) {
            add(&mut pool, Some(phrase.trim().to_string()));
        }
    }
    for phrase in &all {
        if is_frame_phrase(phrase) {
            let rewritten = rewrite_frame_phrase(phrase);
            add(&mut pool, rewritten);
        }
    }
    if pool.is_empty() {
        let derived = phrase_from_description(description);
        add(&mut pool, derived);
    }
    if pool.is_empty() {
        return (None, Vec::new());
    }
    let concept_out = pool.remove(0);
    (Some(concept_out), pool)
}

/// Parse and contract-check one identify response body.
pub fn parse_identify_payload(content: &str) -> Result<Value, IdentifyError> {
    let payload: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            return Err(IdentifyError {
                message: format!("Identify response is not valid JSON: {e}"),
                raw_response: Some(content.to_string()),
                debug_path: None,
            })
        }
    };
    if !payload.is_object() {
        return Err(IdentifyError::new(
            "Identify response is not a JSON object.",
        ));
    }
    for field in [
        "description",
        "concept",
        "suggested_title",
        "condition_notes",
    ] {
        match payload.get(field).and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => {}
            _ => {
                return Err(IdentifyError::new(&format!(
                    "Identify response field {field:?} is missing or not a non-empty string."
                )))
            }
        }
    }
    let bullets = payload.get("listing_bullets").and_then(|v| v.as_array());
    let bullets_ok = bullets
        .map(|b| {
            (3..=5).contains(&b.len())
                && b.iter()
                    .all(|x| x.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
        })
        .unwrap_or(false);
    if !bullets_ok {
        return Err(IdentifyError::new(
            "Identify response field 'listing_bullets' must be 3 to 5 non-empty strings.",
        ));
    }
    let concept = payload["concept"].as_str().unwrap().trim().to_string();
    let raw_alternates = payload.get("alternates").and_then(|v| v.as_array());
    let Some(raw_alternates) = raw_alternates else {
        return Err(IdentifyError::new(
            "ALTERNATES_SHORTFALL: 'alternates' is not a list.",
        ));
    };
    let mut cleaned: Vec<String> = Vec::new();
    for item in raw_alternates {
        if let Some(s) = item.as_str() {
            let phrase = s.trim().to_string();
            if !phrase.is_empty()
                && phrase.to_lowercase() != concept.to_lowercase()
                && !cleaned.contains(&phrase)
            {
                cleaned.push(phrase);
            }
        }
    }
    let description = payload["description"].as_str().unwrap();
    let (concept_out, alternates) = sanitize_plan_phrases(&concept, &cleaned, description);
    let Some(concept_out) = concept_out else {
        return Err(IdentifyError::new(&format!(
            "ALTERNATES_SHORTFALL: no segmentable object phrase in the plan — concept and alternates all name the frame or the medium (concept was {concept:?})."
        )));
    };
    if alternates.len() < 2 {
        return Err(IdentifyError::new(&format!(
            "ALTERNATES_SHORTFALL: fewer than 2 usable alternates after cleaning (got {}).",
            alternates.len()
        )));
    }
    Ok(json!({
        "description": payload["description"].as_str().unwrap().trim(),
        "concept": concept_out,
        "alternates": alternates,
        "suggested_title": payload["suggested_title"].as_str().unwrap().trim(),
        "listing_bullets": bullets
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap().trim().to_string())
            .collect::<Vec<_>>(),
        "condition_notes": payload["condition_notes"].as_str().unwrap().trim(),
    }))
}

fn usage_of_chat(response: &Value) -> Usage {
    let usage = response.get("usage");
    let cached = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Usage {
        input_tokens: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        cached_input_tokens: cached,
        images: 0,
    }
}

fn capture_raw_response(content: &str, call_id: &str) -> Option<String> {
    let dir = config::get_data_dir().join("debug");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("identify-{call_id}.txt"));
    std::fs::write(&path, content).ok()?;
    Some(path.display().to_string())
}

/// One real identify call. `call_id` names the debug capture of an
/// unparseable body (the Python passes the billed call's id via a
/// context var; here it is an explicit parameter).
pub fn vision_identify(
    client: &MetaClient,
    key: &str,
    png_bytes: &[u8],
    model: &str,
    call_id: &str,
) -> Result<(Value, Usage), IdentifyError> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png_bytes)
    );
    let body = json!({
        "model": model,
        "max_tokens": IDENTIFY_MAX_TOKENS,
        "response_format": {"type": "json_schema", "json_schema": identify_schema()},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": IDENTIFY_INSTRUCTIONS},
                {"type": "image_url", "image_url": {"url": data_uri}}
            ]
        }]
    });
    let response =
        post_json(client, key, "/chat/completions", &body).map_err(|e| IdentifyError::new(&e))?;
    let mut usage = usage_of_chat(&response);
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let plan = match parse_identify_payload(&content) {
        Ok(plan) => return Ok((plan, usage)),
        Err(e) => e,
    };
    if plan.raw_response.is_some() {
        let mut e = plan;
        e.debug_path = capture_raw_response(&content, call_id);
        return Err(e);
    }
    if !plan.message.starts_with("ALTERNATES_SHORTFALL") {
        return Err(plan);
    }

    // Repair path: one text-only follow-up for replacement phrases.
    let partial: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
    let concept = partial
        .get("concept")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let repair_body = json!({
        "model": model,
        "max_tokens": 256,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "identify_alternates_repair",
                "strict": true,
                "schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "alternates": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 2
                        }
                    },
                    "required": ["alternates"]
                }
            }
        },
        "messages": [{
            "role": "user",
            "content": format!(
                "A segmentation plan for a product photo uses the noun phrase {concept:?}. Give at least 2 different fallback noun phrases for the same product, each short and naming the concrete physical object itself — never the screen, display, photo, image, screenshot, background, scene, or frame."
            )
        }]
    });
    let repair = post_json(client, key, "/chat/completions", &repair_body)
        .map_err(|e| IdentifyError::new(&e))?;
    usage = usage.merge(&usage_of_chat(&repair));
    let repair_content = repair
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repair_payload: Result<Value, _> = serde_json::from_str(&repair_content);
    let extra = repair_payload
        .as_ref()
        .ok()
        .and_then(|v| v.get("alternates"))
        .and_then(|v| v.as_array())
        .cloned();
    let Some(extra) = extra else {
        let mut e = IdentifyError {
            message: "Alternates repair response is unusable: 'alternates' missing or not a list"
                .to_string(),
            raw_response: Some(repair_content.clone()),
            debug_path: None,
        };
        e.debug_path = capture_raw_response(&repair_content, call_id);
        return Err(e);
    };
    let mut merged = partial.clone();
    let mut alts: Vec<Value> = partial
        .get("alternates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    alts.extend(extra);
    merged["alternates"] = Value::Array(alts);
    let merged_text = serde_json::to_string(&merged).unwrap();
    match parse_identify_payload(&merged_text) {
        Ok(plan) => Ok((plan, usage)),
        Err(mut e) => {
            if e.raw_response.is_some() && e.debug_path.is_none() {
                e.debug_path = capture_raw_response(&repair_content, call_id);
            }
            Err(e)
        }
    }
}

/// Estimate image input tokens from pixel dimensions (32 px patches + 256);
/// 4096 when the dimensions are unknown.
pub fn estimate_image_input_tokens(width: Option<u32>, height: Option<u32>) -> i64 {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let patches = ((w as f64) / 32.0).ceil() as i64 * ((h as f64) / 32.0).ceil() as i64;
            patches + 256
        }
        _ => 4096,
    }
}

/// Worst-case projected cost of one identify call; feeds the spend-cap gate.
pub fn project_identify_cost_usd(
    model: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<f64, String> {
    let input_tokens = estimate_image_input_tokens(width, height) + IDENTIFY_PROMPT_TOKENS;
    let one_call = pricing::estimate_cost_usd(
        model,
        &Usage {
            input_tokens,
            output_tokens: IDENTIFY_MAX_TOKENS,
            ..Default::default()
        },
    )?;
    Ok(crate::round6(2.0 * one_call))
}

/// Worst-case projected cost of one cut_out call at the given attempt budget.
pub fn project_cut_out_cost_usd(model: &str, max_attempts: usize) -> Result<f64, String> {
    pricing::estimate_cost_usd(
        model,
        &Usage {
            images: max_attempts as i64,
            ..Default::default()
        },
    )
}

/// Worst-case projected cost of one draw call (a single generated image).
pub fn project_draw_cost_usd(model: &str) -> Result<f64, String> {
    pricing::estimate_cost_usd(
        model,
        &Usage {
            images: 1,
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// SAM segmentation (Responses API transport).
// ---------------------------------------------------------------------------

fn responses_output_text(response: &Value) -> String {
    if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    let mut parts = String::new();
    if let Some(items) = response.get("output").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(contents) = item.get("content").and_then(|v| v.as_array()) {
                for content in contents {
                    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
                        parts.push_str(text);
                    }
                }
            }
        }
    }
    parts
}

/// One real SAM segmentation call; (output_text, usage).
pub fn segment_image(
    client: &MetaClient,
    key: &str,
    png_bytes: &[u8],
    phrase: &str,
    model: &str,
) -> Result<(String, Usage), String> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png_bytes)
    );
    let body = json!({
        "model": model,
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": phrase},
                {"type": "input_image", "image_url": data_uri}
            ]
        }]
    });
    let response = post_json(client, key, "/responses", &body)?;
    let mut usage = Usage {
        images: 1,
        ..Default::default()
    };
    if let Some(raw) = response.get("usage") {
        usage.input_tokens = raw
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        usage.output_tokens = raw
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
    }
    Ok((responses_output_text(&response), usage))
}

// ---------------------------------------------------------------------------
// Muse Image background plates (images API transport).
// ---------------------------------------------------------------------------

const IMAGE_SIZE_CANDIDATES: [(u32, u32); 5] = [
    (1024, 1024),
    (1536, 1024),
    (1024, 1536),
    (1792, 1024),
    (1024, 1792),
];

const PLATE_INSTRUCTION: &str = "An empty scene photographed straight-on: the setting described below, with a clear, unobstructed flat surface in the center foreground where an object will be placed later. The scene is EMPTY: no product, no featured object on the central surface, no people, no animals, no text, no logos, no watermark. Scene: ";

/// Wrap a scene description in the empty-plate instruction draw sends
/// to the image model.
pub fn wrap_plate_prompt(scene_prompt: &str) -> String {
    format!("{PLATE_INSTRUCTION}{scene_prompt}")
}

/// Nearest supported generation size for the given dimensions;
/// `1024x1024` when the dimensions are unknown.
pub fn closest_image_size(width: Option<u32>, height: Option<u32>) -> String {
    let (Some(w), Some(h)) = (width, height) else {
        return "1024x1024".to_string();
    };
    if w == 0 || h == 0 {
        return "1024x1024".to_string();
    }
    let target = (w as f64 / h as f64).ln();
    let mut best = IMAGE_SIZE_CANDIDATES[0];
    let mut best_score = (f64::INFINITY, i64::MIN);
    for candidate in IMAGE_SIZE_CANDIDATES {
        let score = (
            ((candidate.0 as f64 / candidate.1 as f64).ln() - target).abs(),
            -((candidate.0 as i64) * (candidate.1 as i64)),
        );
        if score < best_score {
            best_score = score;
            best = candidate;
        }
    }
    format!("{}x{}", best.0, best.1)
}

/// Plate-call failure with a Python-compatible error code.
#[derive(Debug)]
pub struct ImageError {
    pub code: String,
    pub message: String,
}

fn plate_bytes_from_response(client: &MetaClient, response: &Value) -> Result<Vec<u8>, ImageError> {
    let data = response.get("data").and_then(|v| v.as_array());
    let Some(data) = data.filter(|d| !d.is_empty()) else {
        if let Some(refusal) = response
            .get("refusal")
            .or_else(|| response.get("error"))
            .filter(|v| !v.is_null())
        {
            return Err(ImageError {
                code: "IMAGE_REFUSED".to_string(),
                message: format!("The image model refused the plate prompt: {refusal}"),
            });
        }
        return Err(ImageError {
            code: "BAD_IMAGE_RESPONSE".to_string(),
            message: "The image model returned no image data for the plate prompt.".to_string(),
        });
    };
    let item = &data[0];
    if let Some(refusal) = item.get("refusal").filter(|v| !v.is_null()) {
        return Err(ImageError {
            code: "IMAGE_REFUSED".to_string(),
            message: format!("The image model refused the plate prompt: {refusal}"),
        });
    }
    let mut raw: Option<Vec<u8>> = None;
    if let Some(b64) = item.get("b64_json").and_then(|v| v.as_str()) {
        if !b64.is_empty() {
            raw = Some(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| ImageError {
                        code: "BAD_IMAGE_RESPONSE".to_string(),
                        message: format!("The plate image payload is not valid base64: {e}"),
                    })?,
            );
        }
    }
    if raw.is_none() {
        if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
            if !url.is_empty() {
                let resp = client
                    .execute(|c| c.get(url).send())
                    .map_err(|e| ImageError {
                        code: "IMAGE_CALL_FAILED".to_string(),
                        message: format!(
                            "Fetching the generated plate image failed: {}",
                            error_chain(&e)
                        ),
                    })?;
                raw = Some(
                    resp.bytes()
                        .map_err(|e| ImageError {
                            code: "IMAGE_CALL_FAILED".to_string(),
                            message: format!(
                                "Fetching the generated plate image failed: {}",
                                error_chain(&e)
                            ),
                        })?
                        .to_vec(),
                );
            }
        }
    }
    let Some(raw) = raw else {
        return Err(ImageError {
            code: "BAD_IMAGE_RESPONSE".to_string(),
            message: "The image model returned an item with neither b64_json nor url.".to_string(),
        });
    };
    // Normalize to RGBA PNG bytes.
    let decoded = decode_image(&raw).map_err(|e| ImageError {
        code: "BAD_IMAGE_RESPONSE".to_string(),
        message: format!("The plate payload does not decode as an image: {e}"),
    })?;
    let rgba = decoded.into_rgba();
    Ok(crate::image::encode_png_rgba(&rgba))
}

/// One real muse-image call; (plate PNG bytes, usage).
pub fn request_plate_image(
    client: &MetaClient,
    key: &str,
    prompt: &str,
    width: Option<u32>,
    height: Option<u32>,
    model: &str,
) -> Result<(Vec<u8>, Usage), ImageError> {
    let size = closest_image_size(width, height);
    let body = json!({
        "model": model,
        "prompt": wrap_plate_prompt(prompt),
        "size": size,
        "n": 1,
        // muse-image-1.0 otherwise returns WebP (extended VP8X), which
        // the hand-rolled plate decoder does not accept. A live probe on
        // 2026-10-27 confirmed the endpoint honors this parameter and
        // returns a PNG payload.
        "output_format": "png"
    });
    let response = post_json(client, key, "/images/generations", &body).map_err(|e| {
        if e.contains("refus") {
            ImageError {
                code: "IMAGE_REFUSED".to_string(),
                message: e,
            }
        } else {
            ImageError {
                code: "IMAGE_CALL_FAILED".to_string(),
                message: format!("The muse-image plate call failed: {e}"),
            }
        }
    })?;
    let png_bytes = plate_bytes_from_response(client, &response)?;
    let mut usage = Usage {
        images: 1,
        ..Default::default()
    };
    if let Some(raw) = response.get("usage") {
        usage.input_tokens = raw
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        usage.output_tokens = raw
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
    }
    Ok((png_bytes, usage))
}

// ---------------------------------------------------------------------------
// Muse Image edit plates (images/edits): the plate is the ORIGINAL photo
// with the product region greyed out, edited in its own perspective.
// ---------------------------------------------------------------------------

const PLATE_EDIT_OBJECT: &str = "Edit @Image1. It is a real photo in which one region has been filled flat grey. Fill the grey region with the continuous background that would be behind it: the same ground, floor, wall and surfaces, continuing their lines, textures and lighting. Keep the camera angle, framing, ground plane, horizon line and light direction exactly as in @Image1; do not move, zoom, rotate or re-frame anything. Make the scene clean, uncluttered and listing-ready: tidy, evenly lit, nothing new added. Do not place any object in the grey region. No people, animals, text, logos or watermark.";

const PLATE_EDIT_SURFACE: &str = "Edit @Image1. It is a real photo in which the main surface has been filled flat grey. Continue the surroundings: keep everything outside the grey region where it is, in the same camera angle, framing, ground plane, horizon line and light direction, and make it clean, uncluttered and listing-ready. Fill the grey region with a plain, even continuation of what borders it. Do not move, zoom, rotate or re-frame anything. No people, animals, text, logos or watermark.";

/// Tidy default styling: the styling text an object plate gets when
/// the caller sends no scene text, so a bare draw still comes back as
/// a clean, bright listing photo of the same place. Surface plates
/// (fills_frame) keep the plain "continue the surroundings" wording.
pub const PLATE_EDIT_DEFAULT_STYLE: &str = "a clean, tidy, bright version of the same place: surfaces fresh and uncluttered, no moss or debris, soft even daylight, same camera height and same ground plane, listing photo quality";

/// Angle lock (owner's ruling, 2026-11-01): appended to EVERY
/// source_edit plate prompt, with or without caller scene text, so a
/// caller never has to ask for the angle to match.
pub const ANGLE_LOCK_CLAUSE: &str = "Keep the exact same camera angle and perspective as the original photo, same horizon position, same framing.";
/// Shadow ruling (owner, 2026-11-01): appended right after the angle
/// lock on every source_edit plate prompt. Best effort only; the
/// composite's shadow carry-over is the guarantee.
pub const SHADOW_CLAUSE: &str = "Keep the object's cast shadow on the ground as in the original photo, same direction, length and softness.";
/// Shot-angle phrases, one per `staging::ShotAngle` bucket.
pub const ANGLE_PHRASE_STEEP_DOWN: &str =
    "shot from standing height looking down at the surface at a steep angle";
pub const ANGLE_PHRASE_EYE_LEVEL: &str = "eye-level shot";
pub const ANGLE_PHRASE_LOW_UP: &str = "shot from a low angle looking slightly up";

pub fn angle_phrase(angle: crate::staging::ShotAngle) -> &'static str {
    use crate::staging::ShotAngle;
    match angle {
        ShotAngle::SteepDown => ANGLE_PHRASE_STEEP_DOWN,
        ShotAngle::EyeLevel => ANGLE_PHRASE_EYE_LEVEL,
        ShotAngle::LowUp => ANGLE_PHRASE_LOW_UP,
    }
}

/// The angle-lock text a source_edit prompt ends with: the base clause,
/// plus "The original photo was <phrase>." only when the geometry
/// states an angle. The unstated eye-level default adds nothing: an
/// "eye-level" claim on a downward photo the estimator missed would
/// pull the edit away from the photo it must match.
pub fn angle_lock_clause(estimate: Option<&crate::staging::AngleEstimate>) -> String {
    match estimate {
        Some(e) if e.stated => format!(
            "{ANGLE_LOCK_CLAUSE} The original photo was {}.",
            angle_phrase(e.angle)
        ),
        _ => ANGLE_LOCK_CLAUSE.to_string(),
    }
}

/// Legacy `generated` mode (no reference photo): the angle lock becomes
/// steering words from the estimated angle, the eye-level default
/// included, since a text-only plate must be told some angle. Weaker by
/// nature: the model never sees the photo.
pub fn generated_angle_steer(estimate: &crate::staging::AngleEstimate) -> String {
    format!(
        "Camera angle and perspective: {}, with the horizon and ground plane placed to match a product photographed that way.",
        angle_phrase(estimate.angle)
    )
}

/// The scene text for a legacy generated plate: the caller's scene,
/// then the angle steering words.
pub fn generated_plate_scene(
    scene_prompt: &str,
    estimate: &crate::staging::AngleEstimate,
) -> String {
    let scene = scene_prompt.trim();
    let steer = generated_angle_steer(estimate);
    if scene.is_empty() {
        steer
    } else {
        format!("{}{} {steer}", scene, sentence_end(scene))
    }
}

/// "." when `text` does not already end a sentence, so an appended
/// clause never runs on from caller text.
fn sentence_end(text: &str) -> &'static str {
    if text.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    }
}

/// The geometry-aware edit prompt (the Mac probe's wording plus the
/// angle lock). `fills_frame` switches to the "continue the
/// surroundings" wording for surfaces and subjects cut by the frame.
/// The caller's scene text rides as styling only: the geometry is the
/// photo's. The base angle-lock clause comes after any scene text, so a
/// scene change cannot pull the camera; only `SHADOW_CLAUSE` follows it.
pub fn plate_edit_prompt(scene_prompt: &str, fills_frame: bool) -> String {
    plate_edit_prompt_locked(scene_prompt, fills_frame, None)
}

/// `plate_edit_prompt` with the geometry's angle estimate: adds the
/// specific shot-angle phrase when the estimate is stated.
pub fn plate_edit_prompt_locked(
    scene_prompt: &str,
    fills_frame: bool,
    estimate: Option<&crate::staging::AngleEstimate>,
) -> String {
    let base = if fills_frame {
        PLATE_EDIT_SURFACE
    } else {
        PLATE_EDIT_OBJECT
    };
    // Shadow ruling (docs/SHADOW-CARRY.md): the cast-shadow clause rides
    // right after the angle lock (best effort; the composite's shadow
    // carry-over is the guarantee).
    let lock = format!("{} {SHADOW_CLAUSE}", angle_lock_clause(estimate));
    let scene = scene_prompt.trim();
    // No scene text on an object plate: the tidy default styling.
    let styling = if scene.is_empty() && !fills_frame {
        PLATE_EDIT_DEFAULT_STYLE
    } else {
        scene
    };
    if styling.is_empty() {
        format!("{base} {lock}")
    } else {
        format!(
            "{base} Styling only (keep the geometry): {styling}{} {lock}",
            sentence_end(styling)
        )
    }
}

/// How the edit request is shaped on the wire. Every field can be set
/// from the environment, so the shape the Mac probe proves can be
/// adopted without a rebuild:
/// - `AGENTHAND_EDIT_PATH` (default `/images/edits`)
/// - `AGENTHAND_EDIT_TRANSPORT` = `multipart` (default) | `json`
/// - `AGENTHAND_EDIT_IMAGE_FIELD` (default `image`; e.g. `image[]`,
///   `images`, `reference_images`)
/// - `AGENTHAND_EDIT_IMAGE_ARRAY` = `1` to send JSON images as an array
/// - `AGENTHAND_EDIT_SIZE_FIELD` (default `size`; e.g. `aspect_ratio`)
/// - `AGENTHAND_EDIT_SIZE_STYLE` = `pixels` (default, `1536x1024`) |
///   `ratio` (`3:2`)
#[derive(Debug, Clone, PartialEq)]
pub struct PlateEditConfig {
    pub path: String,
    pub multipart: bool,
    pub image_field: String,
    pub image_as_array: bool,
    pub size_field: String,
    pub size_as_ratio: bool,
}

impl Default for PlateEditConfig {
    fn default() -> Self {
        PlateEditConfig {
            path: "/images/edits".to_string(),
            multipart: true,
            image_field: "image".to_string(),
            image_as_array: false,
            size_field: "size".to_string(),
            size_as_ratio: false,
        }
    }
}

impl PlateEditConfig {
    pub fn from_env() -> Self {
        let get = |name: &str| std::env::var(name).unwrap_or_default().trim().to_string();
        let mut cfg = PlateEditConfig::default();
        let path = get("AGENTHAND_EDIT_PATH");
        if path.starts_with('/') {
            cfg.path = path;
        }
        cfg.multipart = get("AGENTHAND_EDIT_TRANSPORT") != "json";
        let field = get("AGENTHAND_EDIT_IMAGE_FIELD");
        if !field.is_empty() {
            cfg.image_field = field;
        }
        cfg.image_as_array = matches!(
            get("AGENTHAND_EDIT_IMAGE_ARRAY").as_str(),
            "1" | "true" | "yes"
        );
        let size_field = get("AGENTHAND_EDIT_SIZE_FIELD");
        if !size_field.is_empty() {
            cfg.size_field = size_field;
        }
        cfg.size_as_ratio = get("AGENTHAND_EDIT_SIZE_STYLE") == "ratio";
        cfg
    }

    /// The size value for a pixel size string like `1536x1024`.
    pub fn size_value(&self, pixels: &str) -> String {
        if !self.size_as_ratio {
            return pixels.to_string();
        }
        match pixels {
            "1536x1024" => "3:2",
            "1024x1536" => "2:3",
            "1792x1024" => "16:9",
            "1024x1792" => "9:16",
            _ => "1:1",
        }
        .to_string()
    }
}

/// A multipart/form-data body: text fields, then one PNG file part.
/// Returns (content type, body).
pub fn multipart_body(
    boundary: &str,
    fields: &[(&str, String)],
    file_field: &str,
    png: &[u8],
) -> (String, Vec<u8>) {
    let mut body = Vec::with_capacity(png.len() + 1024);
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"image1.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// One real muse-image edit call over the item-masked reference;
/// (plate image bytes as RGBA PNG, usage). The reference must already
/// have the product region greyed out (`staging::masked_reference`).
pub fn request_plate_edit(
    client: &MetaClient,
    key: &str,
    reference_png: &[u8],
    prompt: &str,
    size: &str,
    model: &str,
    cfg: &PlateEditConfig,
) -> Result<(Vec<u8>, Usage), ImageError> {
    let size_value = cfg.size_value(size);
    let sent = if cfg.multipart {
        let fields = [
            ("model", model.to_string()),
            ("prompt", prompt.to_string()),
            (cfg.size_field.as_str(), size_value),
            ("n", "1".to_string()),
            ("output_format", "png".to_string()),
        ];
        let boundary = format!("agenthand-{}", uuid::Uuid::new_v4().simple());
        let (content_type, body) =
            multipart_body(&boundary, &fields, &cfg.image_field, reference_png);
        post_body(client, key, &cfg.path, &content_type, &body)
    } else {
        let data_uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(reference_png)
        );
        let mut body = json!({
            "model": model,
            "prompt": prompt,
            "n": 1,
            "output_format": "png"
        });
        body[cfg.size_field.as_str()] = json!(size_value);
        body[cfg.image_field.as_str()] = if cfg.image_as_array {
            json!([data_uri])
        } else {
            json!(data_uri)
        };
        post_json(client, key, &cfg.path, &body)
    };
    let response = sent.map_err(|e| ImageError {
        code: if e.contains("refus") {
            "IMAGE_REFUSED"
        } else {
            "IMAGE_CALL_FAILED"
        }
        .to_string(),
        message: format!("The muse-image edit call failed: {e}"),
    })?;
    let png_bytes = plate_bytes_from_response(client, &response)?;
    let mut usage = Usage {
        images: 1,
        ..Default::default()
    };
    if let Some(raw) = response.get("usage") {
        usage.input_tokens = raw
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        usage.output_tokens = raw
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
    }
    Ok((png_bytes, usage))
}

#[cfg(test)]
mod http_timeout_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// A loopback server that accepts one connection, reads the
    /// request, and answers only after `delay`. Loopback only: no
    /// network leaves the machine.
    fn slow_server(delay: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        format!("http://{addr}/")
    }

    /// F0: the per-call timeout comes from AGENTHAND_HTTP_TIMEOUT_S.
    /// A 1 s override must cut off a response that takes 4 s.
    #[test]
    fn build_client_honours_the_env_timeout() {
        std::env::set_var("AGENTHAND_HTTP_TIMEOUT_S", "1");
        let client = build_client().unwrap();
        assert_eq!(client.timeout(), Duration::from_secs(1));
        std::env::set_var("AGENTHAND_HTTP_TIMEOUT_S", "7");
        assert_eq!(build_client().unwrap().timeout(), Duration::from_secs(7));
        std::env::remove_var("AGENTHAND_HTTP_TIMEOUT_S");
        assert_eq!(build_client().unwrap().timeout(), Duration::from_secs(180));
        let url = slow_server(Duration::from_secs(4));
        let started = Instant::now();
        let result = client.execute(|c| c.get(&url).send());
        let elapsed = started.elapsed();
        let err = result.expect_err("a 1 s timeout must cut off a 4 s response");
        assert!(err.is_timeout(), "expected a timeout error, got: {err}");
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    }

    /// F0: unset, empty, unparseable and zero values all keep the
    /// 180 s default; a positive whole number of seconds wins.
    #[test]
    fn timeout_defaults_to_180_seconds() {
        assert_eq!(DEFAULT_HTTP_TIMEOUT_S, 180);
        assert_eq!(http_timeout_from(None), Duration::from_secs(180));
        for raw in ["", "  ", "abc", "0", "-5", "2.5"] {
            assert_eq!(http_timeout_from(Some(raw)), Duration::from_secs(180), "{raw:?}");
        }
        assert_eq!(http_timeout_from(Some("240")), Duration::from_secs(240));
        assert_eq!(http_timeout_from(Some(" 45 ")), Duration::from_secs(45));
        assert_eq!(HTTP_CONNECT_TIMEOUT_S, 20);
    }
}

// ---------------------------------------------------------------------------
// Generated-plate clauses (docs/LIGHT-AND-THIN.md): light direction,
// thin-structure plainness, scene exclusions. They ride after the angle
// steer in the legacy `generated` mode only; source_edit prompts and the
// identify instructions are unchanged.
// ---------------------------------------------------------------------------

/// Light phrases, one per `staging::LightSide` read from a cast shadow.
pub const LIGHT_PHRASE_FROM_LEFT: &str =
    "Sun low from camera-left; long soft shadows fall to camera-right across the ground.";
pub const LIGHT_PHRASE_FROM_RIGHT: &str =
    "Sun low from camera-right; long soft shadows fall to camera-left across the ground.";
pub const LIGHT_PHRASE_FROM_CAMERA: &str =
    "Light from behind the camera; soft shadows fall straight back, away from the camera.";
pub const LIGHT_PHRASE_FROM_BEHIND: &str =
    "Light from behind the scene, facing the camera; soft shadows fall toward the camera.";
pub const LIGHT_PHRASE_OVERHEAD: &str =
    "Soft high daylight from overhead; short soft shadows fall directly beneath objects.";
/// Gradient-only reads claim a side, not a low sun.
pub const LIGHT_PHRASE_GRADIENT_LEFT: &str =
    "Light from camera-left; soft shadows fall to camera-right.";
pub const LIGHT_PHRASE_GRADIENT_RIGHT: &str =
    "Light from camera-right; soft shadows fall to camera-left.";
/// Plainness clause for open-frame products (thin-structure trigger).
pub const THIN_PLAIN_CLAUSE: &str = "Keep the surface directly behind the product plain, smooth and evenly lit, with no dark blocks, doorways, vehicles, furniture or hard edges showing through the product's open frame.";
/// The fixed tail of the scene exclusions.
pub const SCENE_EXCLUSIONS_FIXED: [&str; 3] = ["bicycles or other vehicles", "people", "animals"];

/// The light phrase for an estimate, or None when nothing is claimed.
pub fn light_clause(estimate: &crate::staging::LightEstimate) -> Option<&'static str> {
    use crate::staging::LightSide;
    let side = estimate.side?;
    Some(match (side, estimate.method) {
        (LightSide::FromLeft, "gradient") => LIGHT_PHRASE_GRADIENT_LEFT,
        (LightSide::FromRight, "gradient") => LIGHT_PHRASE_GRADIENT_RIGHT,
        (LightSide::FromLeft, _) => LIGHT_PHRASE_FROM_LEFT,
        (LightSide::FromRight, _) => LIGHT_PHRASE_FROM_RIGHT,
        (LightSide::FromCamera, _) => LIGHT_PHRASE_FROM_CAMERA,
        (LightSide::FromBehind, _) => LIGHT_PHRASE_FROM_BEHIND,
        (LightSide::Overhead, _) => LIGHT_PHRASE_OVERHEAD,
    })
}

fn plural(noun: &str) -> String {
    let consonant_y = noun.len() > 1
        && noun.ends_with('y')
        && !noun[..noun.len() - 1].ends_with(['a', 'e', 'i', 'o', 'u']);
    if noun.ends_with('s') {
        noun.to_string()
    } else if consonant_y {
        format!("{}ies", &noun[..noun.len() - 1])
    } else if noun.ends_with("ch") || noun.ends_with("sh") || noun.ends_with(['x', 'z']) {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// What a generated plate must not contain, from the identify concept:
/// the concept itself, its head noun's plural, then the fixed list. Empty
/// when the concept is unknown (the prompt is then unchanged).
pub fn scene_exclusions(concept: Option<&str>) -> Vec<String> {
    let cleaned: String = concept
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '\'' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    // Keep the concept short (an identify phrase is a few words).
    words.truncate(8);
    if words.is_empty() {
        return Vec::new();
    }
    let phrase = words.join(" ");
    // Head noun: the last word before a qualifier ("bike with basket").
    let cut = words
        .iter()
        .position(|w| ["with", "on", "in", "for", "from", "against", "and", "near"].contains(w))
        .filter(|&p| p > 0)
        .unwrap_or(words.len());
    let head = plural(words[cut - 1]);
    let mut out = Vec::new();
    if words.len() > 1 {
        out.push(phrase);
    }
    if head != "bicycles" && !out.contains(&head) {
        out.push(head);
    }
    out.extend(SCENE_EXCLUSIONS_FIXED.iter().map(|s| s.to_string()));
    out
}

/// "Empty scene, no a, no b, ...; the product will be placed afterwards."
pub fn scene_exclusion_clause(exclusions: &[String]) -> Option<String> {
    if exclusions.is_empty() {
        return None;
    }
    let list: Vec<String> = exclusions.iter().map(|e| format!("no {e}")).collect();
    Some(format!(
        "Empty scene, {}; the product will be placed afterwards.",
        list.join(", ")
    ))
}

/// Append a clause as its own sentence.
pub fn append_plate_clause(text: &str, clause: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        clause.to_string()
    } else {
        format!("{text}{} {clause}", sentence_end(text))
    }
}

/// The generated plate's scene text: `generated_plate_scene` (caller's
/// scene, angle steer), then the light phrase when one is stated, the
/// plainness clause when the thin-structure guard triggered, and the
/// exclusion clause when the concept is known.
pub fn generated_plate_scene_dressed(
    scene_prompt: &str,
    estimate: &crate::staging::AngleEstimate,
    light: Option<&str>,
    thin: bool,
    exclusions: &[String],
) -> String {
    let mut text = generated_plate_scene(scene_prompt, estimate);
    if let Some(phrase) = light {
        text = append_plate_clause(&text, phrase);
    }
    if thin {
        text = append_plate_clause(&text, THIN_PLAIN_CLAUSE);
    }
    if let Some(clause) = scene_exclusion_clause(exclusions) {
        text = append_plate_clause(&text, &clause);
    }
    text
}

#[cfg(test)]
mod plate_default_style_tests {
    use super::*;

    #[test]
    fn bare_object_plate_gets_the_tidy_default_styling_then_the_lock() {
        let p = plate_edit_prompt_locked("", false, None);
        assert_eq!(
            p,
            format!(
                "{PLATE_EDIT_OBJECT} Styling only (keep the geometry): {PLATE_EDIT_DEFAULT_STYLE}. {ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}"
            )
        );
        assert_eq!(plate_edit_prompt("  ", false), p);
    }

    #[test]
    fn surface_plates_and_caller_scenes_do_not_get_the_default() {
        assert_eq!(
            plate_edit_prompt_locked("", true, None),
            format!("{PLATE_EDIT_SURFACE} {ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}")
        );
        let scene = plate_edit_prompt_locked("on a beach", false, None);
        assert!(!scene.contains(PLATE_EDIT_DEFAULT_STYLE));
        assert!(scene.ends_with(&format!("{ANGLE_LOCK_CLAUSE} {SHADOW_CLAUSE}")));
    }
}
