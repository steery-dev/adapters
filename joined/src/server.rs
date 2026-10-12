//! Steery's server as the joined machine reaches it: its URL and the machine's own key,
//! handed by the server as `STEERY_URL` and `STEERY_KEY`. Its words for a joined host (the
//! `/v1/join` paths, `JoinedHost`) stay in this file.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use steery_sdk::{Error, refusal};
use url::Url;

/// How long one request may take, inside the runtime's 60 s (PROTOCOL.md §9).
pub const CHANGE: Duration = Duration::from_secs(55);
/// How long reaching the server may take before it is out of reach.
const CONNECT: Duration = Duration::from_secs(10);
/// The longest line a refusal carries.
const LINE: usize = 300;

/// A host that joined the machine, as `GET /v1/join/hosts` answers it.
#[derive(Debug, Deserialize)]
pub struct JoinedHost {
    pub name: String,
    pub online: bool,
    pub joined_at: DateTime<Utc>,
}

/// A refusal's body from the server: `{"error": {"code", "message"}}`.
#[derive(Deserialize)]
struct Refused {
    error: Said,
}

#[derive(Deserialize)]
struct Said {
    message: String,
}

/// Why a call to the server did not succeed.
#[derive(Debug)]
pub enum Fail {
    /// An HTTP answer that is no success, and what it said.
    Http { status: u16, line: String },
    /// The server could not be reached.
    Offline(String),
    /// Anything else, in one line.
    Other(String),
}

/// The settings given in `initialize`, and the client that speaks to the server.
pub struct Server {
    base: Url,
    key: String,
    http: reqwest::Client,
}

impl Server {
    /// The server `STEERY_URL` and `STEERY_KEY` name; why not, in one line.
    pub fn new(settings: &BTreeMap<String, String>) -> Result<Self, String> {
        let raw = settings
            .get("STEERY_URL")
            .map(|raw| raw.trim())
            .unwrap_or_default();
        if raw.is_empty() {
            return Err(
                "STEERY_URL is not set: Steery's server hands it to a joined machine".into(),
            );
        }
        let mut base = Url::parse(raw).map_err(|_| format!("STEERY_URL is not a URL: {raw}"))?;
        if base.scheme() != "http" && base.scheme() != "https" {
            return Err(format!("STEERY_URL is not http or https: {raw}"));
        }
        base.set_query(None);
        base.set_fragment(None);
        let key = settings
            .get("STEERY_KEY")
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
            .ok_or("STEERY_KEY is not set: Steery's server hands it to a joined machine")?;
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT)
            .timeout(CHANGE)
            .build()
            .map_err(|e| format!("the HTTP client: {}", chain(&e)))?;
        Ok(Self { base, key, http })
    }

    /// `path` under the server's root, with `query`.
    pub fn url(&self, path: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().extend(path);
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    /// The `Authorization` header's value.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.key)
    }

    /// Every host that joined the machine.
    pub async fn hosts(&self) -> Result<Vec<JoinedHost>, Error> {
        let what = "list the joined hosts";
        let listed = async {
            let response = self
                .http
                .get(self.url(&["v1", "join", "hosts"], &[]))
                .header(reqwest::header::AUTHORIZATION, self.bearer())
                .send()
                .await
                .map_err(|e| sending(what, &e))?;
            let status = response.status();
            let body = response.bytes().await.map_err(|e| sending(what, &e))?;
            if !status.is_success() {
                return Err(Fail::Http {
                    status: status.as_u16(),
                    line: refused(what, status.as_u16(), &body),
                });
            }
            serde_json::from_slice(&body)
                .map_err(|e| Fail::Other(format!("{what}: the server's answer: {e}")))
        };
        listed.await.map_err(|fail| self.refuse(fail))
    }

    /// The refusal `fail` is. The machine's key refused is `refused`: it does not heal by
    /// waiting. The server down is a gateway answering for it as well as no answer at all,
    /// and a draining server's 503.
    pub fn refuse(&self, fail: Fail) -> Error {
        let (word, line) = match fail {
            Fail::Http { status: 404, line } => (refusal::NOT_FOUND, line),
            Fail::Http {
                status: 502..=504,
                line,
            }
            | Fail::Offline(line) => (refusal::OFFLINE, line),
            Fail::Http { line, .. } | Fail::Other(line) => (refusal::REFUSED, line),
        };
        Error::refused(word, self.line(&line))
    }

    /// One line for a person, never holding the key.
    pub fn line(&self, said: &str) -> String {
        let text = format!("Steery: {said}").replace(self.key.as_str(), "[redacted]");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > LINE {
            let mut cut: String = text.chars().take(LINE - 1).collect();
            cut.push('…');
            cut
        } else {
            text
        }
    }
}

/// `what` refused with HTTP `status`: the server's own message where it gave one.
pub fn refused(what: &str, status: u16, body: &[u8]) -> String {
    match serde_json::from_slice::<Refused>(body) {
        Ok(refused) => format!("{what}: {}", refused.error.message),
        Err(_) => format!("{what}: HTTP {status}"),
    }
}

/// A failure of `what` from the HTTP client.
fn sending(what: &str, error: &reqwest::Error) -> Fail {
    let line = format!("{what}: {}", chain(error));
    if error.is_connect() || error.is_timeout() {
        Fail::Offline(line)
    } else {
        Fail::Other(line)
    }
}

/// An error and every error under it, in one line.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let said = cause.to_string();
        if !line.contains(&said) {
            line.push_str(": ");
            line.push_str(&said);
        }
        source = cause.source();
    }
    line
}
