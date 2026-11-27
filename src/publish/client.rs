//! Registry publishing API: yamreg returns an artifact receipt; registries
//! with a moderation extension can return a submission instead.

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;
use std::time::{Duration, Instant};

pub const DEFAULT_UPLOAD_URL: &str = "https://repo.yse.wtf/api/v2/upload";

/// Metadata that identifies one artifact in one upload.
pub struct Publication<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub target: &'a str,
    pub sha256: &'a str,
    pub public_key: &'a str,
    pub signature: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationState {
    Pending,
    InReview,
    Approved,
    Published,
    Rejected,
    /// A state this client does not know. Treated as non-terminal so a newer
    /// registry can add intermediate states without breaking older clients.
    Other(String),
}

impl ModerationState {
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "pending" | "queued" => Self::Pending,
            "in_review" | "review" | "moderating" => Self::InReview,
            "approved" | "accepted" => Self::Approved,
            "published" | "live" => Self::Published,
            "rejected" | "refused" | "declined" => Self::Rejected,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Approved | Self::Published | Self::Rejected)
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Approved | Self::Published)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::InReview => "in_review",
            Self::Approved => "approved",
            Self::Published => "published",
            Self::Rejected => "rejected",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub id: String,
    pub state: ModerationState,
    /// Moderator explanation. Must be shown to the author verbatim.
    pub reason: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug)]
pub enum UploadOutcome {
    /// Registry accepted the upload and tracks it as a moderation submission.
    Queued(Submission),
    /// Registry accepted the upload but this API version does not report
    /// moderation. The artifact is live under the registry's publication rules.
    Accepted,
}

/// Derive the API base (`…/api/v2`) from the upload endpoint. The upload URL is
/// the only address the author configures; everything else is relative to it.
pub fn api_base(upload_url: &str) -> Result<String> {
    let url = reqwest::Url::parse(upload_url)
        .with_context(|| format!("invalid registry upload URL {upload_url:?}"))?;
    if url.scheme() != "https" {
        bail!("registry upload URL must use HTTPS")
    }
    let mut base = url.clone();
    let path = base.path().trim_end_matches('/').to_string();
    let path = path.strip_suffix("/upload").unwrap_or(&path).to_string();
    if path.is_empty() {
        bail!("registry upload URL has no API prefix, expected …/api/v2/upload")
    }
    base.set_path(&path);
    base.set_query(None);
    base.set_fragment(None);
    Ok(base.as_str().trim_end_matches('/').to_string())
}

pub fn submission_url(upload_url: &str, id: &str) -> Result<String> {
    if matches!(id, "" | "." | "..")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        bail!("invalid submission id {id:?}")
    }
    Ok(format!("{}/submissions/{id}", api_base(upload_url)?))
}

fn client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("yam-publish/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("create HTTP client")
}

/// Account information returned by `GET /api/v2/whoami`. The endpoint is
/// optional: a registry without it still accepts uploads, and `auth login`
/// saves the token without verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub login: Option<String>,
    pub namespaces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhoAmI {
    Known(Account),
    Unsupported,
}

pub fn whoami(upload_url: &str, token: &str) -> Result<WhoAmI> {
    let url = format!("{}/whoami", api_base(upload_url)?);
    let response = client()?
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .send()
        .with_context(|| format!("request {url}"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(WhoAmI::Unsupported);
    }
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        bail!(
            "registry rejected the publish token ({status}): {}",
            body.trim()
        )
    }
    if !status.is_success() {
        bail!(
            "registry account request failed ({status}): {}",
            body.trim()
        )
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("registry returned a non-JSON account document at {url}"))?;
    Ok(WhoAmI::Known(parse_account(&value)))
}

fn parse_account(value: &Value) -> Account {
    let account = value
        .get("user")
        .filter(|value| value.is_object())
        .unwrap_or(value);
    let login = string_at(account, &["login", "username", "name"]);
    let namespaces = account
        .get("namespaces")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    Value::String(name) => Some(name.clone()),
                    Value::Object(_) => string_at(entry, &["name", "id", "namespace"]),
                    _ => None,
                })
                .collect()
        })
        .or_else(|| {
            string_at(account, &["namespace"])
                .or_else(|| string_at(value, &["namespace"]))
                .map(|namespace| vec![namespace])
        })
        .unwrap_or_default();
    Account { login, namespaces }
}

/// Upload one artifact. The body is the archive itself; identity travels in
/// headers so the registry can authenticate before buffering the body.
pub fn upload(
    upload_url: &str,
    token: &str,
    publication: &Publication<'_>,
    artifact: &[u8],
) -> Result<UploadOutcome> {
    if artifact.is_empty() {
        bail!("artifact is empty")
    }
    api_base(upload_url)?;
    let http = client()?;
    let request = http
        .post(upload_url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/octet-stream")
        .header("x-yam-name", header(publication.name)?)
        .header("x-yam-version", header(publication.version)?)
        .header("x-yam-target", header(publication.target)?)
        .header("x-yam-sha256", header(publication.sha256)?)
        .header("x-yam-author-key", header(publication.public_key)?)
        .header("x-yam-author-signature", header(publication.signature)?)
        .body(artifact.to_vec());
    let mut attempt = 1;
    let response = loop {
        let result = request.try_clone().context("cannot replay upload")?.send();
        let delay = match &result {
            Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(attempt),
            Err(e) if e.is_connect() => attempt,
            Err(_) => {
                break result.context(
                    "upload result unknown; check the signed package index before retrying",
                )?
            }
            Ok(_) => break result?,
        };
        if attempt >= 3 || delay > 30 {
            break result.context("send artifact to registry")?;
        }
        std::thread::sleep(Duration::from_secs(delay));
        attempt += 1;
    };
    let status = response.status();
    let body = response
        .text()
        .context("read upload response; check the signed package index before retrying")?;
    if status.is_server_error() {
        bail!(
            "upload result unknown ({status}); check the signed package index before retrying: {}",
            body.trim()
        )
    }
    if !status.is_success() {
        bail!("registry rejected publication ({status}): {}", body.trim())
    }
    upload_acknowledgement(&body, publication)
}

fn upload_acknowledgement(body: &str, publication: &Publication<'_>) -> Result<UploadOutcome> {
    if let Some(submission) = parse_submission(body) {
        return Ok(UploadOutcome::Queued(submission));
    }
    if body.trim() == "uploaded" {
        return Ok(UploadOutcome::Accepted);
    }
    let receipt: Value =
        serde_json::from_str(body).context("unrecognized registry upload acknowledgement")?;
    for (field, expected) in [
        ("name", publication.name),
        ("version", publication.version),
        ("target", publication.target),
        ("sha256", publication.sha256),
    ] {
        if receipt.get(field).and_then(Value::as_str) != Some(expected) {
            bail!("registry upload receipt does not match {field}")
        }
    }
    Ok(UploadOutcome::Accepted)
}

/// Fetch the current moderation state of a submission.
pub fn status(upload_url: &str, token: &str, id: &str) -> Result<Submission, StatusError> {
    let url = submission_url(upload_url, id).map_err(StatusError::permanent)?;
    let response = client()
        .map_err(StatusError::retryable)?
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .send()
        .map_err(|error| {
            StatusError::retryable(anyhow::Error::new(error).context(format!("request {url}")))
        })?;
    let http = response.status();
    let body = response.text().unwrap_or_default();
    if http == reqwest::StatusCode::NOT_FOUND {
        return Err(StatusError::permanent(anyhow::anyhow!(
            "registry does not expose moderation status for submission {id} (404 at {url}); \
             the server must implement GET /api/v2/submissions/<id> as documented in docs/REGISTRY-API.md"
        )));
    }
    if !http.is_success() {
        let error = anyhow::anyhow!("registry status request failed ({http}): {}", body.trim());
        return Err(
            if http.is_server_error() || http == reqwest::StatusCode::TOO_MANY_REQUESTS {
                StatusError::retryable(error)
            } else {
                StatusError::permanent(error)
            },
        );
    }
    parse_submission(&body)
        .filter(|s| s.id == id)
        .ok_or_else(|| {
            StatusError::permanent(anyhow::anyhow!(
                "registry returned an invalid or mismatched submission at {url}"
            ))
        })
}

/// A status request failure. Authentication and contract errors must stop the
/// watcher immediately; transport errors and 5xx responses are worth retrying.
#[derive(Debug)]
pub struct StatusError {
    error: anyhow::Error,
    retryable: bool,
}

impl StatusError {
    fn retryable(error: anyhow::Error) -> Self {
        Self {
            error,
            retryable: true,
        }
    }

    fn permanent(error: anyhow::Error) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for StatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// Parse both the upload acknowledgement and the status response. Accepts a
/// nested `submission` object and flat top-level fields so a server can evolve
/// the envelope without breaking authors.
pub fn parse_submission(body: &str) -> Option<Submission> {
    let value: Value = serde_json::from_str(body).ok()?;
    let candidate = value
        .get("submission")
        .filter(|value| value.is_object())
        .unwrap_or(&value);
    let id = string_at(candidate, &["id", "submission_id", "submissionId"])
        .or_else(|| string_at(&value, &["submission_id", "submissionId"]))?;
    submission_url(DEFAULT_UPLOAD_URL, &id).ok()?;
    let moderation = candidate
        .get("moderation")
        .filter(|value| value.is_object());
    let state = moderation
        .and_then(|value| string_at(value, &["state", "status"]))
        .or_else(|| string_at(candidate, &["status", "state"]))
        .or_else(|| string_at(&value, &["status", "state"]))
        .filter(|v| !v.trim().is_empty())?;
    let reason = moderation
        .and_then(|value| string_at(value, &["reason", "message"]))
        .or_else(|| string_at(candidate, &["reason", "message"]))
        .filter(|reason| !reason.trim().is_empty());
    let updated_at = string_at(candidate, &["updated_at", "updatedAt"])
        .or_else(|| moderation.and_then(|value| string_at(value, &["updated_at", "updatedAt"])));
    Some(Submission {
        id,
        state: ModerationState::parse(&state),
        reason,
        updated_at,
    })
}

fn string_at(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(key))
        .find_map(|value| value.as_str())
        .map(str::to_string)
}

fn header(value: &str) -> Result<reqwest::header::HeaderValue> {
    reqwest::header::HeaderValue::from_str(value)
        .context("metadata contains an invalid HTTP header value")
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Delay between status requests.
    pub interval: Duration,
    /// Give up after this long. `None` waits forever; `Some(ZERO)` checks once.
    pub timeout: Option<Duration>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            timeout: Some(Duration::from_secs(900)),
        }
    }
}

/// Follow a submission until it reaches a terminal state.
///
/// `on_change` is called for the first state and for every transition, never
/// twice for the same state. Transient failures are retried a few times; the
/// author sees the reason instead of a generic "connection reset".
pub fn watch_submission(
    upload_url: &str,
    token: &str,
    id: &str,
    options: &WatchOptions,
    on_change: &mut dyn FnMut(&Submission),
) -> Result<Submission> {
    watch_with(
        id,
        options,
        |id| status(upload_url, token, id).map_err(anyhow::Error::new),
        on_change,
    )
}

fn watch_with<F>(
    id: &str,
    options: &WatchOptions,
    mut fetch: F,
    on_change: &mut dyn FnMut(&Submission),
) -> Result<Submission>
where
    F: FnMut(&str) -> Result<Submission>,
{
    let started = Instant::now();
    let mut last: Option<Submission> = None;
    let mut consecutive_errors = 0u32;
    loop {
        match fetch(id) {
            Ok(submission) => {
                consecutive_errors = 0;
                if last.as_ref() != Some(&submission) {
                    on_change(&submission);
                    last = Some(submission.clone());
                }
                if submission.state.is_terminal() {
                    return Ok(submission);
                }
            }
            Err(error) => {
                let retryable = error
                    .downcast_ref::<StatusError>()
                    .is_some_and(|error| error.retryable);
                if !retryable {
                    return Err(error);
                }
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(error.context(format!(
                        "giving up on submission {id} after {consecutive_errors} failed status requests"
                    )));
                }
            }
        }
        if let Some(timeout) = options.timeout {
            if started.elapsed() >= timeout {
                return last.map_or_else(
                    || {
                        bail!(
                            "timed out after {}s waiting for submission {id}",
                            timeout.as_secs()
                        )
                    },
                    |submission| {
                        if submission.state.is_terminal() {
                            Ok(submission)
                        } else {
                            bail!(
                                "timed out after {}s with submission {id} still {}",
                                timeout.as_secs(),
                                submission.state.as_str()
                            )
                        }
                    },
                );
            }
        }
        if options.interval.is_zero() {
            std::thread::yield_now();
        } else {
            std::thread::sleep(options.interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_is_derived_from_the_upload_endpoint() {
        assert_eq!(
            api_base("https://repo.yse.wtf/api/v2/upload").unwrap(),
            "https://repo.yse.wtf/api/v2"
        );
        assert_eq!(
            api_base("https://registry.example/api/v2/upload/").unwrap(),
            "https://registry.example/api/v2"
        );
    }

    #[test]
    fn submission_url_rejects_path_injection() {
        assert!(submission_url("https://r.example/api/v2/upload", "../upload").is_err());
        assert_eq!(
            submission_url("https://r.example/api/v2/upload", "abc-123").unwrap(),
            "https://r.example/api/v2/submissions/abc-123"
        );
    }

    #[test]
    fn rejects_insecure_endpoints_and_incomplete_submissions() {
        assert!(api_base("http://registry.example/api/v2/upload").is_err());
        assert!(parse_submission(r#"{"id":"s1"}"#).is_none());
        assert!(submission_url(DEFAULT_UPLOAD_URL, "..").is_err());
    }

    #[test]
    fn nested_and_flat_submissions_parse() {
        let nested = parse_submission(
            r#"{"submission":{"id":"s1","status":"in_review","moderation":{"reason":"checking"}}}"#,
        )
        .unwrap();
        assert_eq!(nested.id, "s1");
        assert_eq!(nested.state, ModerationState::InReview);
        assert_eq!(nested.reason.as_deref(), Some("checking"));

        let flat = parse_submission(r#"{"submission_id":"s2","status":"published"}"#).unwrap();
        assert_eq!(flat.id, "s2");
        assert_eq!(flat.state, ModerationState::Published);
        assert!(flat.state.is_success());
    }

    #[test]
    fn upload_receipt_must_match_the_sent_artifact() {
        let p = Publication {
            name: "demo/player",
            version: "1.0.0",
            target: "any",
            sha256: "ab",
            public_key: "",
            signature: "",
        };
        let receipt = serde_json::json!({"name":p.name,"version":p.version,"target":p.target,"sha256":p.sha256});
        assert!(matches!(
            upload_acknowledgement(&receipt.to_string(), &p),
            Ok(UploadOutcome::Accepted)
        ));
        for field in ["name", "version", "target", "sha256"] {
            let mut bad = receipt.clone();
            bad[field] = Value::String("unexpected".into());
            assert!(upload_acknowledgement(&bad.to_string(), &p).is_err());
            bad.as_object_mut().unwrap().remove(field);
            assert!(upload_acknowledgement(&bad.to_string(), &p).is_err());
        }
        assert!(matches!(
            upload_acknowledgement("uploaded\n", &p),
            Ok(UploadOutcome::Accepted)
        ));
        assert!(upload_acknowledgement("<html>Login</html>", &p).is_err());
        assert!(upload_acknowledgement("", &p).is_err());
        assert!(parse_submission(r#"{"ok":true}"#).is_none());
    }

    #[test]
    fn account_documents_parse_in_both_shapes() {
        let account = parse_account(&serde_json::json!({
            "login": "author",
            "namespaces": ["author", {"name": "team"}]
        }));
        assert_eq!(account.login.as_deref(), Some("author"));
        assert_eq!(account.namespaces, vec!["author", "team"]);

        let account = parse_account(&serde_json::json!({
            "user": {"username": "author"},
            "namespace": "author"
        }));
        assert_eq!(account.login.as_deref(), Some("author"));
        assert_eq!(account.namespaces, vec!["author"]);
    }

    #[test]
    fn an_unknown_state_is_not_terminal() {
        let submission = parse_submission(r#"{"id":"s","status":"manual_hold"}"#).unwrap();
        assert!(!submission.state.is_terminal());
        assert_eq!(submission.state.as_str(), "manual_hold");
    }

    #[test]
    fn watch_reports_transitions_and_stops_at_a_terminal_state() {
        let states = [
            "pending",
            "pending",
            "in_review",
            "approved",
            "approved",
            "rejected",
        ];
        let mut index = 0;
        let mut seen = Vec::new();
        let submission = watch_with(
            "s",
            &WatchOptions {
                interval: Duration::ZERO,
                timeout: None,
            },
            |_| {
                let state = states[index.min(states.len() - 1)];
                index += 1;
                Ok(Submission {
                    id: "s".into(),
                    state: ModerationState::parse(state),
                    reason: None,
                    updated_at: None,
                })
            },
            &mut |submission| seen.push(submission.state.clone()),
        )
        .unwrap();
        assert_eq!(submission.state, ModerationState::Approved);
        assert_eq!(index, 4);
        assert_eq!(
            seen,
            vec![
                ModerationState::Pending,
                ModerationState::InReview,
                ModerationState::Approved
            ]
        );
    }

    #[test]
    fn watch_times_out_while_a_submission_is_still_pending() {
        let result = watch_with(
            "s",
            &WatchOptions {
                interval: Duration::ZERO,
                timeout: Some(Duration::ZERO),
            },
            |_| {
                Ok(Submission {
                    id: "s".into(),
                    state: ModerationState::Pending,
                    reason: None,
                    updated_at: None,
                })
            },
            &mut |_| {},
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("still pending"), "{message}");
    }

    #[test]
    fn watch_does_not_retry_a_rejected_token() {
        let mut calls = 0;
        let result = watch_with(
            "s",
            &WatchOptions {
                interval: Duration::ZERO,
                timeout: None,
            },
            |_| {
                calls += 1;
                Err(anyhow::Error::new(StatusError::permanent(anyhow::anyhow!(
                    "registry status request failed (401 Unauthorized)"
                ))))
            },
            &mut |_| {},
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}
