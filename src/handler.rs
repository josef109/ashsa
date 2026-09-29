use crate::config::Config;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Retry behaviour for failures while *establishing* the connection to the
/// downstream endpoint.
///
/// Only connect errors are retried. Once a request may have reached Home
/// Assistant (read timeout, error status, ...) it is never sent again, so a
/// command such as "turn on" cannot be executed twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total number of attempts including the first one (minimum 1).
    pub max_attempts: u32,
    /// Pause between two attempts.
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            delay: Duration::from_millis(200),
        }
    }
}

#[derive(Clone)]
pub struct App {
    config: Config,
    client: Client,
    retry: RetryPolicy,
}

#[derive(Debug, PartialEq, Eq)]
enum RequestError {
    MissingDirective,
    UnsupportedPayloadVersion(String),
    MissingScope,
    UnsupportedScopeType(String),
    MissingToken,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingDirective => write!(f, "request missing required directive field"),
            Self::UnsupportedPayloadVersion(version) => {
                write!(f, "only payloadVersion 3 is supported, got {version}")
            }
            Self::MissingScope => write!(f, "request missing scope in endpoint or payload"),
            Self::UnsupportedScopeType(scope_type) => {
                write!(f, "only BearerToken scope is supported, got {scope_type}")
            }
            Self::MissingToken => write!(f, "authentication token is required"),
        }
    }
}

impl std::error::Error for RequestError {}

impl App {
    pub fn new(config: Config, client: Client) -> Self {
        Self {
            config,
            client,
            retry: RetryPolicy::default(),
        }
    }

    /// Overrides the default connect-retry policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    async fn send_with_retry(
        &self,
        url: &str,
        token: &str,
        event: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let max_attempts = self.retry.max_attempts.max(1);
        let mut attempt = 1;

        loop {
            match self
                .client
                .post(url)
                .bearer_auth(token)
                .json(event)
                .send()
                .await
            {
                Ok(response) => return Ok(response),
                Err(err) if err.is_connect() && attempt < max_attempts => {
                    warn!(
                        attempt,
                        max_attempts,
                        error = %err,
                        "connecting to downstream endpoint failed, retrying"
                    );
                    tokio::time::sleep(self.retry.delay).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn handle_event(&self, event: Value) -> Value {
        info!("processing Alexa request");

        if self.config.debug {
            debug!(event = %sanitize_json(&event), "received Alexa event");
        }

        match self.handle_event_inner(event).await {
            Ok(response) => response,
            Err(HandlerError::InvalidRequest(err)) => {
                error!(error = %err, "invalid request");
                alexa_error("INVALID_REQUEST", err.to_string())
            }
            Err(HandlerError::Authorization(message)) => {
                error!(message, "downstream authorization failure");
                alexa_error("INVALID_AUTHORIZATION_CREDENTIAL", message)
            }
            Err(HandlerError::Internal(message)) => {
                error!(message, "internal handler error");
                alexa_error("INTERNAL_ERROR", message)
            }
        }
    }

    async fn handle_event_inner(&self, event: Value) -> Result<Value, HandlerError> {
        validate_payload_version(&event).map_err(HandlerError::InvalidRequest)?;
        let token = extract_token(&event, &self.config).map_err(HandlerError::InvalidRequest)?;
        let url = format!("{}/api/alexa/smart_home", self.config.base_url);

        let response = self
            .send_with_retry(&url, token, &event)
            .await
            .map_err(|err| {
                HandlerError::Internal(format!("failed to call downstream endpoint: {err}"))
            })?;

        let status = response.status();
        let body = response.text().await.map_err(|err| {
            HandlerError::Internal(format!("failed to read downstream response: {err}"))
        })?;

        debug!(status = status.as_u16(), "received downstream response");

        if status.is_success() {
            serde_json::from_str(&body).map_err(|err| {
                HandlerError::Internal(format!("downstream returned invalid JSON: {err}"))
            })
        } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            Err(HandlerError::Authorization(body))
        } else {
            Err(HandlerError::Internal(format!(
                "downstream endpoint returned {}: {}",
                status.as_u16(),
                body
            )))
        }
    }
}

enum HandlerError {
    InvalidRequest(RequestError),
    Authorization(String),
    Internal(String),
}

fn validate_payload_version(event: &Value) -> Result<(), RequestError> {
    let directive = event
        .get("directive")
        .ok_or(RequestError::MissingDirective)?;

    let payload_version = directive
        .get("header")
        .and_then(Value::as_object)
        .and_then(|header| header.get("payloadVersion"))
        .and_then(Value::as_str)
        .ok_or_else(|| RequestError::UnsupportedPayloadVersion("missing".to_owned()))?;

    if payload_version == "3" {
        Ok(())
    } else {
        Err(RequestError::UnsupportedPayloadVersion(
            payload_version.to_owned(),
        ))
    }
}

fn extract_token<'a>(event: &'a Value, config: &'a Config) -> Result<&'a str, RequestError> {
    let scope = validated_scope(event)?;

    if let Some(token) = config.fallback_bearer_token.as_deref() {
        info!("Using long lived token");
        return Ok(token);
    }

    if let Some(token) = scope.get("token").and_then(Value::as_str) {
        info!("Using token from event");
        return Ok(token);
    }

    Err(RequestError::MissingToken)
}

fn validated_scope<'a>(event: &'a Value) -> Result<&'a Value, RequestError> {
    let directive = event
        .get("directive")
        .ok_or(RequestError::MissingDirective)?;

    let scope = directive
        .get("endpoint")
        .and_then(|endpoint| endpoint.get("scope"))
        .or_else(|| {
            directive
                .get("payload")
                .and_then(|payload| payload.get("grantee"))
        })
        .or_else(|| {
            directive
                .get("payload")
                .and_then(|payload| payload.get("scope"))
        })
        .ok_or(RequestError::MissingScope)?;

    let scope_type = scope
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| RequestError::UnsupportedScopeType("missing".to_owned()))?;

    if scope_type != "BearerToken" {
        return Err(RequestError::UnsupportedScopeType(scope_type.to_owned()));
    }

    Ok(scope)
}

fn alexa_error(error_type: &str, message: String) -> Value {
    json!({
        "event": {
            "payload": {
                "type": error_type,
                "message": message,
            }
        }
    })
}

fn sanitize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sanitized = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if key.eq_ignore_ascii_case("token") {
                    sanitized.insert(key.clone(), Value::String("<redacted>".to_owned()));
                } else {
                    sanitized.insert(key.clone(), sanitize_json(value));
                }
            }
            Value::Object(sanitized)
        }
        Value::Array(values) => Value::Array(values.iter().map(sanitize_json).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, RequestError, RetryPolicy, alexa_error, extract_token, validate_payload_version,
    };
    use crate::client::build_http_client;
    use crate::config::Config;
    use mockito::{Matcher, Server};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config(base_url: String) -> Config {
        Config {
            base_url,
            aws_default_region: Some("us-east-1".to_owned()),
            debug: false,
            insecure_skip_tls_verify: false,
            fallback_bearer_token: None,
        }
    }

    fn sample_event(scope: Value) -> Value {
        json!({
            "directive": {
                "header": {
                    "payloadVersion": "3"
                },
                "endpoint": {
                    "scope": scope
                }
            }
        })
    }

    #[test]
    fn payload_version_must_be_three() {
        let err = validate_payload_version(&json!({
            "directive": {
                "header": {
                    "payloadVersion": "2"
                }
            }
        }))
        .unwrap_err();

        assert_eq!(err, RequestError::UnsupportedPayloadVersion("2".to_owned()));
    }

    #[test]
    fn extracts_endpoint_scope_token() {
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "endpoint-token"
        }));

        let cfg = config("https://example.com".to_owned());
        let token = extract_token(&event, &cfg).unwrap();
        assert_eq!(token, "endpoint-token");
    }

    #[test]
    fn long_lived_token_takes_precedence_over_event_token() {
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "endpoint-token"
        }));

        let mut cfg = config("https://example.com".to_owned());
        cfg.fallback_bearer_token = Some("fallback-token".to_owned());

        let token = extract_token(&event, &cfg).unwrap();
        assert_eq!(token, "fallback-token");
    }

    #[test]
    fn extracts_linking_grantee_token() {
        let event = json!({
            "directive": {
                "header": {
                    "payloadVersion": "3"
                },
                "payload": {
                    "grantee": {
                        "type": "BearerToken",
                        "token": "grantee-token"
                    }
                }
            }
        });

        let cfg = config("https://example.com".to_owned());
        let token = extract_token(&event, &cfg).unwrap();
        assert_eq!(token, "grantee-token");
    }

    #[test]
    fn extracts_discovery_scope_token() {
        let event = json!({
            "directive": {
                "header": {
                    "payloadVersion": "3"
                },
                "payload": {
                    "scope": {
                        "type": "BearerToken",
                        "token": "discovery-token"
                    }
                }
            }
        });

        let cfg = config("https://example.com".to_owned());
        let token = extract_token(&event, &cfg).unwrap();
        assert_eq!(token, "discovery-token");
    }

    #[test]
    fn missing_directive_is_rejected() {
        let err = validate_payload_version(&json!({})).unwrap_err();
        assert_eq!(err, RequestError::MissingDirective);
    }

    #[test]
    fn unsupported_scope_type_is_rejected() {
        let event = sample_event(json!({
            "type": "AccessToken",
            "token": "bad-token"
        }));

        let err = extract_token(&event, &config("https://example.com".to_owned())).unwrap_err();
        assert_eq!(
            err,
            RequestError::UnsupportedScopeType("AccessToken".to_owned())
        );
    }

    #[test]
    fn fallback_token_is_used_even_when_debug_is_disabled() {
        let event = sample_event(json!({
            "type": "BearerToken"
        }));
        let mut cfg = config("https://example.com".to_owned());
        cfg.fallback_bearer_token = Some("fallback-token".to_owned());

        let token = extract_token(&event, &cfg).unwrap();
        assert_eq!(token, "fallback-token");
    }

    #[test]
    fn fallback_token_still_requires_bearer_scope_type() {
        let event = sample_event(json!({
            "type": "AccessToken"
        }));
        let mut cfg = config("https://example.com".to_owned());
        cfg.fallback_bearer_token = Some("fallback-token".to_owned());

        let err = extract_token(&event, &cfg).unwrap_err();
        assert_eq!(
            err,
            RequestError::UnsupportedScopeType("AccessToken".to_owned())
        );
    }

    #[tokio::test]
    async fn successful_downstream_json_is_passed_through() {
        let mut server = Server::new_async().await;
        let response_body = json!({"event": {"header": {"name": "Response"}}});
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .match_header("authorization", "Bearer test-token")
            .match_header(
                "content-type",
                Matcher::Regex("application/json.*".to_owned()),
            )
            .match_header("user-agent", "Alexa Smart Home Skill Adapter - us-east-1")
            .with_status(200)
            .with_body(response_body.to_string())
            .create_async()
            .await;

        let cfg = config(server.url());
        let client = build_http_client(&cfg).unwrap();
        let app = App::new(cfg, client);
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(actual, response_body);
    }

    #[tokio::test]
    async fn downstream_request_uses_long_lived_token_when_configured() {
        let mut server = Server::new_async().await;
        let response_body = json!({"event": {"header": {"name": "Response"}}});
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "event-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .match_header("authorization", "Bearer fallback-token")
            .with_status(200)
            .with_body(response_body.to_string())
            .create_async()
            .await;

        let mut cfg = config(server.url());
        cfg.fallback_bearer_token = Some("fallback-token".to_owned());
        let client = build_http_client(&cfg).unwrap();
        let app = App::new(cfg, client);
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(actual, response_body);
    }

    #[tokio::test]
    async fn downstream_auth_failure_maps_to_alexa_error() {
        let mut server = Server::new_async().await;
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "bad-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .with_status(401)
            .with_body("denied")
            .create_async()
            .await;

        let app = App::new(config(server.url()), Client::new());
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(
            actual,
            alexa_error("INVALID_AUTHORIZATION_CREDENTIAL", "denied".to_owned())
        );
    }

    #[tokio::test]
    async fn downstream_server_error_maps_to_internal_error() {
        let mut server = Server::new_async().await;
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .with_status(500)
            .with_body("boom")
            .create_async()
            .await;

        let app = App::new(config(server.url()), Client::new());
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(
            actual,
            alexa_error(
                "INTERNAL_ERROR",
                "downstream endpoint returned 500: boom".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn invalid_downstream_json_maps_to_internal_error() {
        let mut server = Server::new_async().await;
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .with_status(200)
            .with_body("not-json")
            .create_async()
            .await;

        let app = App::new(config(server.url()), Client::new());
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert!(
            actual["event"]["payload"]["message"]
                .as_str()
                .unwrap()
                .contains("downstream returned invalid JSON")
        );
    }

    #[tokio::test]
    async fn transport_failure_maps_to_internal_error() {
        let app = App::new(config("http://127.0.0.1:9".to_owned()), Client::new());
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let actual = app.handle_event(event).await;

        assert_eq!(actual["event"]["payload"]["type"], "INTERNAL_ERROR");
        assert!(
            actual["event"]["payload"]["message"]
                .as_str()
                .unwrap()
                .contains("failed to call downstream endpoint")
        );
    }

    /// Returns an address on which nothing is listening (connection refused).
    fn unused_addr() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    /// Starts a one-shot HTTP server on `addr` after `start_delay` and answers
    /// the first request with `body` as JSON.
    fn spawn_late_server(
        addr: SocketAddr,
        start_delay: Duration,
        body: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tokio::time::sleep(start_delay).await;
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();

            // Read the complete request (headers + body) before answering.
            let mut received = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                received.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&received).to_ascii_lowercase();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if received.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        })
    }

    #[tokio::test]
    async fn connect_failure_is_retried_until_server_is_up() {
        let addr = unused_addr();
        let response_body = json!({"event": {"header": {"name": "Response"}}});
        let server = spawn_late_server(addr, Duration::from_millis(150), response_body.to_string());
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let app =
            App::new(config(format!("http://{addr}")), Client::new()).with_retry(RetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(100),
            });
        let actual = app.handle_event(event).await;

        server.await.unwrap();
        assert_eq!(actual, response_body);
    }

    #[tokio::test]
    async fn connect_failure_gives_up_after_max_attempts() {
        let addr = unused_addr();
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let app =
            App::new(config(format!("http://{addr}")), Client::new()).with_retry(RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(50),
            });
        let started = Instant::now();
        let actual = app.handle_event(event).await;

        // 3 attempts => 2 pauses of 50 ms.
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(actual["event"]["payload"]["type"], "INTERNAL_ERROR");
        assert!(
            actual["event"]["payload"]["message"]
                .as_str()
                .unwrap()
                .contains("failed to call downstream endpoint")
        );
    }

    #[tokio::test]
    async fn single_attempt_policy_does_not_retry() {
        let addr = unused_addr();
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let app =
            App::new(config(format!("http://{addr}")), Client::new()).with_retry(RetryPolicy {
                max_attempts: 1,
                delay: Duration::from_secs(5),
            });
        let started = Instant::now();
        let actual = app.handle_event(event).await;

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(actual["event"]["payload"]["type"], "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn zero_max_attempts_still_sends_once() {
        let mut server = Server::new_async().await;
        let response_body = json!({"event": {"header": {"name": "Response"}}});
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .with_status(200)
            .with_body(response_body.to_string())
            .expect(1)
            .create_async()
            .await;

        let app = App::new(config(server.url()), Client::new()).with_retry(RetryPolicy {
            max_attempts: 0,
            delay: Duration::from_millis(10),
        });
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(actual, response_body);
    }

    #[tokio::test]
    async fn error_status_from_downstream_is_not_retried() {
        let mut server = Server::new_async().await;
        let event = sample_event(json!({
            "type": "BearerToken",
            "token": "test-token"
        }));

        let mock = server
            .mock("POST", "/api/alexa/smart_home")
            .with_status(500)
            .with_body("boom")
            .expect(1)
            .create_async()
            .await;

        let app = App::new(config(server.url()), Client::new()).with_retry(RetryPolicy {
            max_attempts: 3,
            delay: Duration::from_millis(10),
        });
        let actual = app.handle_event(event).await;

        mock.assert_async().await;
        assert_eq!(actual["event"]["payload"]["type"], "INTERNAL_ERROR");
    }
}
