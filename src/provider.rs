//! Provider wire formats (spec section 2.3): request building, response
//! parsing, and one blocking HTTPS POST.

use crate::config::Config;
use crate::error::{Error, Result, config_err};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::time::Duration;

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Anthropic,
    OpenAi,
    Gemini,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "anthropic" => Some(Kind::Anthropic),
            "openai" => Some(Kind::OpenAi),
            "gemini" => Some(Kind::Gemini),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
            Kind::Gemini => "gemini",
        }
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Kind::Anthropic => "https://api.anthropic.com",
            Kind::OpenAi => "https://api.openai.com/v1",
            Kind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Kind::Anthropic => "claude-haiku-4-5",
            Kind::OpenAi => "gpt-4.1-nano",
            Kind::Gemini => "gemini-2.5-flash",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub kind: Kind,
    pub url: String,
    pub model: String,
}

/// Resolve `name` to a concrete provider. The three wire types double as
/// built-in provider names, so `hey` works with only a key set.
pub fn resolve(cfg: &Config, name: &str, model_override: Option<&str>) -> Result<Provider> {
    let builtin = Kind::parse(name);
    let kind = match cfg.provider_field(name, "type") {
        Some(t) => Kind::parse(&t).ok_or_else(|| {
            crate::error::Error::Config(format!(
                "provider '{name}' has unknown type '{t}' (expected anthropic, openai or gemini)"
            ))
        })?,
        None => match builtin {
            Some(k) => k,
            None => {
                return config_err(format!(
                    "provider '{name}' is not configured. Run: hey config init"
                ));
            }
        },
    };
    let url = cfg
        .provider_field(name, "url")
        .unwrap_or_else(|| kind.default_url().to_string());
    let model = match model_override
        .map(String::from)
        .or_else(|| cfg.provider_field(name, "model"))
    {
        Some(m) => m,
        None if builtin == Some(kind) => kind.default_model().to_string(),
        None => {
            return config_err(format!(
                "provider '{name}' has no model. Run: hey config set provider.{name}.model <model>"
            ));
        }
    };
    Ok(Provider {
        name: name.to_string(),
        kind,
        url: url.trim_end_matches('/').to_string(),
        model,
    })
}

#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Request {
    /// The request as shown by `--dry-run`. Auth values are already
    /// placeholders when the caller passes one, but mask again to be safe.
    pub fn dry_run_text(&self) -> String {
        let mut out = format!("POST {}\n", self.url);
        for (k, v) in &self.headers {
            let secret = matches!(
                k.to_ascii_lowercase().as_str(),
                "authorization" | "x-api-key" | "x-goog-api-key"
            );
            out.push_str(&format!("{k}: {}\n", if secret { "<redacted>" } else { v }));
        }
        out.push('\n');
        out.push_str(&serde_json::to_string_pretty(&self.body).unwrap_or_default());
        out.push('\n');
        out
    }
}

pub struct Params<'a> {
    pub system: &'a str,
    pub user: &'a str,
    pub max_tokens: u64,
    pub stream: bool,
}

pub fn build_request(p: &Provider, key: Option<&str>, q: &Params) -> Request {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    let (url, body) = match p.kind {
        Kind::Anthropic => {
            headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
            if let Some(k) = key {
                headers.push(("x-api-key".into(), k.into()));
            }
            let mut body = json!({
                "model": p.model,
                "max_tokens": q.max_tokens,
                "system": q.system,
                "messages": [{ "role": "user", "content": q.user }],
            });
            if q.stream {
                body["stream"] = json!(true);
            }
            (format!("{}/v1/messages", p.url), body)
        }
        Kind::OpenAi => {
            if let Some(k) = key {
                headers.push(("authorization".into(), format!("Bearer {k}")));
            }
            // OpenAI's newer models reject `max_tokens`; every OpenAI-compatible
            // server still understands it.
            let limit = if p.url == Kind::OpenAi.default_url() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            let mut body = json!({
                "model": p.model,
                "messages": [
                    { "role": "system", "content": q.system },
                    { "role": "user", "content": q.user },
                ],
            });
            body[limit] = json!(q.max_tokens);
            if q.stream {
                body["stream"] = json!(true);
            }
            (format!("{}/chat/completions", p.url), body)
        }
        Kind::Gemini => {
            if let Some(k) = key {
                headers.push(("x-goog-api-key".into(), k.into()));
            }
            let model = p.model.strip_prefix("models/").unwrap_or(&p.model);
            let action = if q.stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            let body = json!({
                "systemInstruction": { "parts": [{ "text": q.system }] },
                "contents": [{ "role": "user", "parts": [{ "text": q.user }] }],
                "generationConfig": { "maxOutputTokens": q.max_tokens },
            });
            (format!("{}/models/{model}:{action}", p.url), body)
        }
    };
    Request { url, headers, body }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
}

impl Usage {
    fn merge(&mut self, other: Usage) {
        self.input = other.input.or(self.input);
        self.output = other.output.or(self.output);
    }
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    path.iter().try_fold(v, |v, k| v.get(*k))?.as_u64()
}

/// Text and usage from a complete (non-streaming) response body.
pub fn parse_response(kind: Kind, body: &str) -> Result<(String, Usage)> {
    let v: Value = serde_json::from_str(body).map_err(|_| {
        Error::Provider(format!("unexpected response (not JSON): {}", snippet(body)))
    })?;
    if let Some(msg) = error_message(&v) {
        return Err(Error::Provider(msg));
    }
    let (text, usage) = match kind {
        Kind::Anthropic => {
            let text = v["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p["type"] == "text")
                        .filter_map(|p| p["text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default();
            let usage = Usage {
                input: u64_at(&v, &["usage", "input_tokens"]),
                output: u64_at(&v, &["usage", "output_tokens"]),
            };
            (text, usage)
        }
        Kind::OpenAi => {
            let content = &v["choices"][0]["message"]["content"];
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
                _ => String::new(),
            };
            let usage = Usage {
                input: u64_at(&v, &["usage", "prompt_tokens"]),
                output: u64_at(&v, &["usage", "completion_tokens"]),
            };
            (text, usage)
        }
        Kind::Gemini => {
            if let Some(reason) = v["promptFeedback"]["blockReason"].as_str() {
                return Err(Error::Provider(format!("prompt blocked by provider: {reason}")));
            }
            let text = v["candidates"][0]["content"]["parts"]
                .as_array()
                .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<String>())
                .unwrap_or_default();
            let usage = Usage {
                input: u64_at(&v, &["usageMetadata", "promptTokenCount"]),
                output: u64_at(&v, &["usageMetadata", "candidatesTokenCount"]),
            };
            (text, usage)
        }
    };
    Ok((text, usage))
}

/// Provider error text from the common JSON error shapes.
fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error").or_else(|| v.get(0).and_then(|x| x.get("error")))?;
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| e.as_str())
        .map(String::from)
        .unwrap_or_else(|| e.to_string());
    Some(msg)
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        out.push_str("...");
    }
    out
}

pub enum StreamItem {
    Text(String),
    Usage(Usage),
    Done,
}

/// Interpret one SSE `data:` payload.
pub fn parse_stream_event(kind: Kind, data: &str) -> Result<Vec<StreamItem>> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(vec![StreamItem::Done]);
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Ok(Vec::new()); // keep-alives and comments
    };
    if let Some(msg) = error_message(&v) {
        return Err(Error::Provider(msg));
    }
    let mut items = Vec::new();
    match kind {
        Kind::Anthropic => match v["type"].as_str() {
            Some("content_block_delta") => {
                if let Some(t) = v["delta"]["text"].as_str() {
                    items.push(StreamItem::Text(t.to_string()));
                }
            }
            Some("message_start") => items.push(StreamItem::Usage(Usage {
                input: u64_at(&v, &["message", "usage", "input_tokens"]),
                output: u64_at(&v, &["message", "usage", "output_tokens"]),
            })),
            Some("message_delta") => items.push(StreamItem::Usage(Usage {
                input: u64_at(&v, &["usage", "input_tokens"]),
                output: u64_at(&v, &["usage", "output_tokens"]),
            })),
            Some("message_stop") => items.push(StreamItem::Done),
            _ => {}
        },
        Kind::OpenAi => {
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str()
                && !t.is_empty()
            {
                items.push(StreamItem::Text(t.to_string()));
            }
        }
        Kind::Gemini => {
            if let Some(reason) = v["promptFeedback"]["blockReason"].as_str() {
                return Err(Error::Provider(format!("prompt blocked by provider: {reason}")));
            }
            if let Some(parts) = v["candidates"][0]["content"]["parts"].as_array() {
                for t in parts.iter().filter_map(|p| p["text"].as_str()) {
                    items.push(StreamItem::Text(t.to_string()));
                }
            }
            items.push(StreamItem::Usage(Usage {
                input: u64_at(&v, &["usageMetadata", "promptTokenCount"]),
                output: u64_at(&v, &["usageMetadata", "candidatesTokenCount"]),
            }));
        }
    }
    Ok(items)
}

/// Remove secrets from text about to be shown to the user.
pub fn scrub(text: &str, key: Option<&str>) -> String {
    match key {
        Some(k) if k.len() >= 4 => text.replace(k, "<redacted>"),
        _ => text.to_string(),
    }
}

pub struct Sent {
    pub text: String,
    pub raw: String,
    pub usage: Usage,
}

fn agent(timeout: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout)))
        .http_status_as_error(false)
        .user_agent(format!("hey/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn transport_error(e: ureq::Error, req: &Request, timeout: u64, key: Option<&str>) -> Error {
    let host = req.url.split('/').nth(2).unwrap_or(&req.url);
    let msg = match e {
        ureq::Error::Timeout(_) => format!("request to {host} timed out after {timeout}s"),
        other => format!("cannot reach {host}: {other}"),
    };
    Error::Provider(scrub(&msg, key))
}

fn http_error(status: u16, body: &str, key: Option<&str>) -> Error {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| error_message(&v))
        .unwrap_or_else(|| snippet(body));
    let detail = scrub(&detail, key);
    if detail.is_empty() {
        Error::Provider(format!("provider returned HTTP {status}"))
    } else {
        Error::Provider(format!("provider returned HTTP {status}: {detail}"))
    }
}

fn post(
    req: &Request,
    timeout: u64,
    key: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let agent = agent(timeout);
    let mut r = agent.post(&req.url);
    for (k, v) in &req.headers {
        r = r.header(k.as_str(), v.as_str());
    }
    let payload = req.body.to_string();
    r.send(payload.as_str())
        .map_err(|e| transport_error(e, req, timeout, key))
}

/// One complete request/response round trip.
pub fn send(kind: Kind, req: &Request, timeout: u64, key: Option<&str>) -> Result<Sent> {
    let mut resp = post(req, timeout, key)?;
    let status = resp.status().as_u16();
    let raw = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| transport_error(e, req, timeout, key))?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &raw, key));
    }
    let (text, usage) = parse_response(kind, &raw).map_err(|e| match e {
        Error::Provider(m) => Error::Provider(scrub(&m, key)),
        other => other,
    })?;
    Ok(Sent { text, raw, usage })
}

/// Streaming round trip: `on_text` is called with each delta as it arrives.
pub fn send_stream(
    kind: Kind,
    req: &Request,
    timeout: u64,
    key: Option<&str>,
    mut on_text: impl FnMut(&str),
) -> Result<Usage> {
    let mut resp = post(req, timeout, key)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let raw = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(http_error(status, &raw, key));
    }
    let mut usage = Usage::default();
    let reader = BufReader::new(resp.body_mut().as_reader());
    for line in reader.split(b'\n') {
        let line = line.map_err(|e| {
            Error::Provider(scrub(&format!("stream interrupted: {e}"), key))
        })?;
        let line = String::from_utf8_lossy(&line);
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let items = parse_stream_event(kind, data).map_err(|e| match e {
            Error::Provider(m) => Error::Provider(scrub(&m, key)),
            other => other,
        })?;
        for item in items {
            match item {
                StreamItem::Text(t) => on_text(&t),
                StreamItem::Usage(u) => usage.merge(u),
                StreamItem::Done => return Ok(usage),
            }
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(kind: Kind, url: &str) -> Provider {
        Provider { name: "p".into(), kind, url: url.into(), model: "m1".into() }
    }

    fn params(stream: bool) -> Params<'static> {
        Params { system: "SYS", user: "USER", max_tokens: 123, stream }
    }

    #[test]
    fn anthropic_request_shape() {
        let r = build_request(&prov(Kind::Anthropic, "https://api.anthropic.com"), Some("K"), &params(false));
        assert_eq!(r.url, "https://api.anthropic.com/v1/messages");
        assert!(r.headers.contains(&("x-api-key".into(), "K".into())));
        assert!(r.headers.contains(&("anthropic-version".into(), "2023-06-01".into())));
        assert_eq!(r.body["system"], "SYS");
        assert_eq!(r.body["max_tokens"], 123);
        assert_eq!(r.body["messages"][0]["content"], "USER");
        assert!(r.body.get("stream").is_none());
    }

    #[test]
    fn openai_request_shape_and_token_param() {
        let r = build_request(&prov(Kind::OpenAi, "https://api.openai.com/v1"), Some("K"), &params(true));
        assert_eq!(r.url, "https://api.openai.com/v1/chat/completions");
        assert!(r.headers.contains(&("authorization".into(), "Bearer K".into())));
        assert_eq!(r.body["messages"][0]["role"], "system");
        assert_eq!(r.body["max_completion_tokens"], 123);
        assert!(r.body.get("max_tokens").is_none());
        assert_eq!(r.body["stream"], true);

        let r = build_request(&prov(Kind::OpenAi, "http://localhost:11434/v1"), None, &params(false));
        assert_eq!(r.body["max_tokens"], 123);
        assert!(r.headers.iter().all(|(k, _)| k != "authorization"));
    }

    #[test]
    fn gemini_request_shape() {
        let r = build_request(&prov(Kind::Gemini, "https://g.example/v1beta"), Some("K"), &params(false));
        assert_eq!(r.url, "https://g.example/v1beta/models/m1:generateContent");
        assert!(r.headers.contains(&("x-goog-api-key".into(), "K".into())));
        assert_eq!(r.body["systemInstruction"]["parts"][0]["text"], "SYS");
        assert_eq!(r.body["generationConfig"]["maxOutputTokens"], 123);
        let s = build_request(&prov(Kind::Gemini, "https://g.example/v1beta"), None, &params(true));
        assert!(s.url.ends_with("models/m1:streamGenerateContent?alt=sse"));
    }

    #[test]
    fn dry_run_never_shows_a_key() {
        let r = build_request(&prov(Kind::OpenAi, "https://api.openai.com/v1"), Some("sk-secret-123"), &params(false));
        let text = r.dry_run_text();
        assert!(!text.contains("sk-secret-123"), "{text}");
        assert!(text.contains("authorization: <redacted>"));
    }

    #[test]
    fn parses_anthropic_text_blocks_only() {
        let body = r#"{"content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"a"},{"type":"text","text":"b"}],
                       "usage":{"input_tokens":5,"output_tokens":7}}"#;
        let (t, u) = parse_response(Kind::Anthropic, body).unwrap();
        assert_eq!(t, "ab");
        assert_eq!(u, Usage { input: Some(5), output: Some(7) });
    }

    #[test]
    fn parses_openai_and_gemini() {
        let (t, u) = parse_response(
            Kind::OpenAi,
            r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#,
        )
        .unwrap();
        assert_eq!((t.as_str(), u.output), ("hi", Some(2)));

        let (t, u) = parse_response(
            Kind::Gemini,
            r#"{"candidates":[{"content":{"parts":[{"text":"x"},{"text":"y"}]}}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4}}"#,
        )
        .unwrap();
        assert_eq!((t.as_str(), u.input), ("xy", Some(3)));
    }

    #[test]
    fn provider_error_bodies_become_errors() {
        let e = parse_response(Kind::Anthropic, r#"{"type":"error","error":{"type":"x","message":"bad key"}}"#)
            .unwrap_err();
        assert_eq!(e.to_string(), "bad key");
        assert!(parse_response(Kind::OpenAi, "<html>").is_err());
        let e = parse_response(Kind::Gemini, r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#).unwrap_err();
        assert!(e.to_string().contains("SAFETY"));
    }

    #[test]
    fn stream_events() {
        let t = |k, d: &str| -> String {
            parse_stream_event(k, d)
                .unwrap()
                .into_iter()
                .filter_map(|i| if let StreamItem::Text(t) = i { Some(t) } else { None })
                .collect()
        };
        assert_eq!(
            t(Kind::Anthropic, r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#),
            "hi"
        );
        assert_eq!(t(Kind::OpenAi, r#"{"choices":[{"delta":{"content":"yo"}}]}"#), "yo");
        assert_eq!(t(Kind::Gemini, r#"{"candidates":[{"content":{"parts":[{"text":"g"}]}}]}"#), "g");
        assert!(matches!(
            parse_stream_event(Kind::OpenAi, "[DONE]").unwrap()[0],
            StreamItem::Done
        ));
        assert!(parse_stream_event(Kind::OpenAi, "not json").unwrap().is_empty());
        assert!(parse_stream_event(Kind::Anthropic, r#"{"type":"error","error":{"message":"overloaded"}}"#).is_err());
    }

    #[test]
    fn scrub_removes_keys_from_messages() {
        assert_eq!(scrub("bad key sk-abc-123456 here", Some("sk-abc-123456")), "bad key <redacted> here");
        assert_eq!(scrub("nothing", None), "nothing");
    }

    #[test]
    fn http_errors_are_scrubbed_and_readable() {
        let e = http_error(401, r#"{"error":{"message":"Incorrect API key: sk-live-9999"}}"#, Some("sk-live-9999"));
        let s = e.to_string();
        assert!(s.contains("HTTP 401") && !s.contains("sk-live-9999"), "{s}");
    }
}
