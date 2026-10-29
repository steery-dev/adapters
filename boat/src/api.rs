//! Boat's published REST API (`https://boat.dev/api/v1`, described by
//! `https://docs.boat.dev/openapi/boat-v1.yaml`), as far as this adapter calls it: sandboxes,
//! their lifecycle and forks, their SSH key and hosted ports.
//!
//! A host is what Boat's API serves under `/sandboxes`; that path is Boat's word and is sent
//! as it is. Every item here is named by Steery's words: [`Host`] is one as Boat answers it.
//! Boat reports a stopped host as `archived`.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

/// Boat's own API, where the person names none.
pub const DEFAULT_API: &str = "https://boat.dev/api/v1";

/// Most hosts one page of `GET /sandboxes` holds.
const PAGE: u32 = 200;

/// The states of a host that accepts commands and connections.
pub const RUNNING: &[&str] = &["ready", "idle", "running"];
/// The states of a host that is being made, which ends running.
pub const MAKING: &[&str] = &["init", "provisioning", "provisioned", "cloning"];
/// The states a host that is being changed fails in. `cancelled` is a make that got no
/// machine, removed by Boat.
pub const FAILED: &[&str] = &["error", "cancelled"];

/// How a call to Boat failed.
#[derive(Debug)]
pub enum Fault {
    /// Boat has no such host or saved snapshot (HTTP 404).
    NotFound(String),
    /// Boat refused the API key (HTTP 401), and what it said: a key that does not heal by
    /// waiting, given again by the person.
    KeyRefused { status: u16, said: String },
    /// Boat is down: not reached, a gateway that answers for it (HTTP 502, 503, 504 without
    /// one of Boat's capacity codes), or a request that got no answer in its time.
    Offline(String),
    /// Boat answered, and the host did not reach the state asked for in time.
    TimedOut(String),
    /// Boat has no room now: the account's sandboxes at once or starts per minute are
    /// full (HTTP 429), or no machine of the size is free (HTTP 503 `no_ready_machine`,
    /// `out_of_capacity`).
    NoPlace(String),
    /// A state conflict (HTTP 409), by Boat's `code`: `snapshot_data_expired`,
    /// `sandbox_resuming`, `idempotency_in_progress`.
    Conflict { code: String, line: String },
    /// Anything else Boat answered, or a host that ended in a state it was not asked for.
    Failed(String),
}

/// A host as Boat answers it, as far as the adapter reads it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Why it is stopped, failed or cancelled.
    #[serde(default)]
    pub error: Option<String>,
    /// Its setup script's progress: `pending`, `running`, `done`, `failed`; absent where it
    /// was made with none.
    #[serde(default)]
    pub setup_status: Option<String>,
    /// The setup script's exit code and the end of its standard error, where it failed.
    #[serde(default)]
    pub setup_error: Option<String>,
    /// When it last stopped, by any cause; absent while it runs.
    #[serde(default)]
    pub stopped_at: Option<String>,
    /// When Boat deleted its disk, 14 days after it stopped; absent while Boat keeps it.
    #[serde(default)]
    pub data_expired_at: Option<String>,
}

impl Host {
    /// Its state, `unknown` where Boat names none.
    pub fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("unknown")
    }

    /// Whether it carries every one of `labels`.
    pub fn carries(&self, labels: &BTreeMap<String, String>) -> bool {
        let Some(its) = &self.labels else {
            return labels.is_empty();
        };
        labels
            .iter()
            .all(|(name, value)| its.get(name) == Some(value))
    }
}

#[derive(Deserialize)]
struct One {
    sandbox: Host,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    sandboxes: Vec<Host>,
    #[serde(default)]
    page_info: Option<PageInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    has_more: bool,
}

/// What a host is made from, as `POST /sandboxes` takes it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Create<'a> {
    pub name: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    /// Seconds to the auto-stop, from the make.
    pub ttl_seconds: u32,
    /// The size: `small`, `default`, `large`, `xlarge`; absent: Boat's default.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub size: Option<&'a str>,
    /// The saved snapshot it starts from; absent: Boat's own image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<&'a str>,
    /// The Boat environment it gets; absent: the account's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<&'a str>,
    #[serde(skip_serializing_if = "Env::is_empty")]
    pub env: &'a Env,
    /// A script Boat runs in it once it is ready, as its user, in `bash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_script: Option<&'a str>,
}

/// The environment every host is made with, as the person's `BOAT_ENV` names it. Its
/// values may be secrets: `Debug` shows the names only.
#[derive(Default, Serialize)]
#[serde(transparent)]
pub struct Env(pub BTreeMap<String, String>);

impl Env {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// Where a host's SSH server is, as `POST /sandboxes/{id}/sshkey` answers once it took
/// the adapter's key.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ssh {
    pub ssh_user: String,
    /// The machine's own address, IPv6 or IPv4.
    #[serde(default)]
    pub machine_ip: Option<String>,
    /// A public IPv4 `host:port` forwarded to its SSH server, where the machine has no
    /// public IPv4 of its own.
    #[serde(default)]
    pub ssh_endpoint: Option<String>,
    /// Its SSH server's public host key, to pin.
    pub host_key: String,
}

#[derive(Deserialize)]
struct Hosted {
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ran {
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: String,
}

/// A fork of a host, as `POST /sandboxes/{id}/fork` takes it. A fork takes none of its
/// source's labels, and keeps them where a make from a saved snapshot drops them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fork<'a> {
    pub labels: &'a BTreeMap<String, String>,
    /// Seconds to the auto-stop, from the fork: a fork takes none of its source's.
    pub ttl_seconds: u32,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub size: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<&'a str>,
    #[serde(skip_serializing_if = "Env::is_empty")]
    pub env: &'a Env,
}

#[derive(Deserialize)]
struct Forked {
    id: String,
}

/// A time by which a method answers.
#[derive(Clone, Copy)]
pub struct Deadline {
    at: Instant,
    seconds: u64,
}

impl Deadline {
    pub fn after(seconds: u64) -> Self {
        Self {
            at: Instant::now() + Duration::from_secs(seconds),
            seconds,
        }
    }

    pub fn seconds(&self) -> u64 {
        self.seconds
    }

    /// The time left, or none.
    pub fn left(&self) -> Option<Duration> {
        self.at
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    fn within(&self) -> Result<Duration, Fault> {
        self.left()
            .ok_or_else(|| Fault::TimedOut(format!("no answer within {} s", self.seconds)))
    }

    /// Sleeps `every`, or what is left of the deadline; the line `still` times out with
    /// where none is left.
    pub async fn pause(
        &self,
        every: Duration,
        still: impl FnOnce() -> String,
    ) -> Result<(), Fault> {
        let Some(left) = self.left() else {
            return Err(Fault::TimedOut(still()));
        };
        tokio::time::sleep(every.min(left)).await;
        Ok(())
    }
}

/// A client of one Boat account, by its API key.
pub struct Boat {
    http: reqwest::Client,
    api: String,
    key: String,
    size: Option<String>,
    environment: Option<String>,
    env: Env,
}

impl Boat {
    pub fn new(
        key: String,
        api: Option<&str>,
        size: Option<String>,
        environment: Option<String>,
        env: Env,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("steery-adapter-boat/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let api = api.unwrap_or(DEFAULT_API).trim_end_matches('/').to_owned();
        Ok(Self {
            http,
            api,
            key,
            size,
            environment,
            env,
        })
    }

    /// The API key, to keep out of every line.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn size(&self) -> Option<&str> {
        self.size.as_deref()
    }

    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        deadline: Deadline,
    ) -> Result<RequestBuilder, Fault> {
        Ok(self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(&self.key)
            .timeout(deadline.within()?))
    }

    /// Every host carrying all of `labels` and, where one is named, a name starting with
    /// `prefix`, page by page.
    pub async fn list(
        &self,
        labels: &BTreeMap<String, String>,
        prefix: Option<&str>,
        deadline: Deadline,
    ) -> Result<Vec<Host>, Fault> {
        let limit = PAGE.to_string();
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = labels
                .iter()
                .map(|(name, value)| ("label", format!("{name}={value}")))
                .collect();
            query.extend(prefix.map(|prefix| ("namePrefix", prefix.to_owned())));
            query.push(("limit", limit.clone()));
            if let Some(cursor) = cursor.take() {
                query.push(("cursor", cursor));
            }
            let page: Page = json(
                send(
                    self.request(Method::GET, "/sandboxes", deadline)?
                        .query(&query),
                )
                .await?,
            )
            .await?;
            // Each filter is checked here too, whatever Boat's does with several.
            found.extend(page.sandboxes.into_iter().filter(|host| {
                host.carries(labels)
                    && prefix.is_none_or(|prefix| {
                        host.name
                            .as_deref()
                            .is_some_and(|name| name.starts_with(prefix))
                    })
            }));
            match page.page_info {
                Some(PageInfo {
                    next_cursor: Some(next),
                    has_more: true,
                }) if !next.is_empty() => cursor = Some(next),
                _ => return Ok(found),
            }
        }
    }

    pub async fn get(&self, id: &str, deadline: Deadline) -> Result<Host, Fault> {
        let one: One =
            json(send(self.request(Method::GET, &host_path(id, ""), deadline)?).await?).await?;
        Ok(one.sandbox)
    }

    /// `POST /sandboxes`, retried by Boat as one make under `once` (its `Idempotency-Key`).
    pub async fn create(
        &self,
        create: &Create<'_>,
        once: &str,
        deadline: Deadline,
    ) -> Result<Host, Fault> {
        let request = self
            .request(Method::POST, "/sandboxes", deadline)?
            .header("Idempotency-Key", once)
            .json(create);
        let one: One = json(send(request).await?).await?;
        Ok(one.sandbox)
    }

    /// `POST /sandboxes/{id}/{action}` with `body`: `stop`, `resume`.
    pub async fn act(
        &self,
        id: &str,
        action: &str,
        body: Value,
        deadline: Deadline,
    ) -> Result<(), Fault> {
        send(
            self.request(Method::POST, &host_path(id, action), deadline)?
                .json(&body),
        )
        .await
        .map(drop)
    }

    /// `PATCH /sandboxes/{id}`: its auto-stop set `seconds` from now.
    pub async fn extend(&self, id: &str, seconds: u32, deadline: Deadline) -> Result<(), Fault> {
        send(
            self.request(Method::PATCH, &host_path(id, ""), deadline)?
                .json(&json!({ "ttlSeconds": seconds })),
        )
        .await
        .map(drop)
    }

    /// `DELETE /sandboxes/{id}`, its data with it, confirmed by its id as Boat asks.
    pub async fn delete(&self, id: &str, deadline: Deadline) -> Result<(), Fault> {
        send(
            self.request(Method::DELETE, &host_path(id, ""), deadline)?
                .header("X-Ascii-Confirm-Delete", id),
        )
        .await
        .map(drop)
    }

    /// `POST /sandboxes/{id}/sshkey`: `public` (OpenSSH format) let in, and where its SSH
    /// server is now.
    pub async fn ssh(&self, id: &str, public: &str, deadline: Deadline) -> Result<Ssh, Fault> {
        json(
            send(
                self.request(Method::POST, &host_path(id, "sshkey"), deadline)?
                    .json(&json!({ "key": public })),
            )
            .await?,
        )
        .await
    }

    /// `POST /sandboxes/{id}/host`: `port` on a public HTTPS URL that needs no token, the
    /// same while the host lives.
    pub async fn host(&self, id: &str, port: u16, deadline: Deadline) -> Result<String, Fault> {
        let hosted: Hosted = json(
            send(
                self.request(Method::POST, &host_path(id, "host"), deadline)?
                    .json(&json!({ "port": port, "public": true })),
            )
            .await?,
        )
        .await?;
        Ok(hosted.url)
    }

    /// `POST /sandboxes/{id}/commands`: `command` run in the host by its shell, waited for
    /// `seconds` at most: its exit code (none where a signal ended it) and standard output.
    pub async fn run(
        &self,
        id: &str,
        command: &str,
        seconds: u32,
        deadline: Deadline,
    ) -> Result<(Option<i32>, String), Fault> {
        let ran: Ran = json(
            send(
                self.request(Method::POST, &host_path(id, "commands"), deadline)?
                    .json(&json!({ "command": command, "timeoutSeconds": seconds })),
            )
            .await?,
        )
        .await?;
        Ok((ran.exit_code, ran.stdout))
    }

    /// `POST /sandboxes/{id}/fork`: a new host from `id`'s newest snapshot, its id; retried
    /// by Boat as one fork under `once` (its `Idempotency-Key`).
    pub async fn fork(
        &self,
        id: &str,
        fork: &Fork<'_>,
        once: &str,
        deadline: Deadline,
    ) -> Result<String, Fault> {
        let request = self
            .request(Method::POST, &host_path(id, "fork"), deadline)?
            .header("Idempotency-Key", once)
            .json(fork);
        let forked: Forked = json(send(request).await?).await?;
        Ok(forked.id)
    }

    /// Polls `id` until it is in one of `targets`, every `every`. A state in `fails` fails;
    /// a host Boat no longer has is `deleted`, an answer where `targets` holds it and a
    /// fault where not. A failed reading is read again until the deadline, but a refused
    /// key is final.
    pub async fn wait(
        &self,
        id: &str,
        targets: &[&str],
        fails: &[&str],
        what: &str,
        every: Duration,
        deadline: Deadline,
    ) -> Result<String, Fault> {
        loop {
            let still = match self.get(id, deadline).await {
                Ok(host) => {
                    let state = host.state();
                    if targets.contains(&state) {
                        return Ok(state.to_owned());
                    }
                    if fails.contains(&state) {
                        let reason = host.error.as_deref().unwrap_or("no reason given");
                        return Err(Fault::Failed(format!(
                            "host {id} failed to {what}: {state}: {reason}"
                        )));
                    }
                    state.to_owned()
                }
                Err(Fault::NotFound(_)) if targets.contains(&"deleted") => {
                    return Ok("deleted".into());
                }
                Err(fault @ (Fault::NotFound(_) | Fault::KeyRefused { .. })) => return Err(fault),
                // The last reading's own fault: Boat down to the end is offline.
                Err(fault) if deadline.left().is_none() => return Err(fault),
                Err(fault) => {
                    eprintln!("boat: reading host {id} again: {fault:?}");
                    "unread".to_owned()
                }
            };
            deadline
                .pause(every, || {
                    format!(
                        "host {id} still {still} after {} s, not {what}",
                        deadline.seconds()
                    )
                })
                .await?;
        }
    }
}

/// A path segment, percent-encoded.
fn segment(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The path of host `id`, and of its `action` where one is named.
fn host_path(id: &str, action: &str) -> String {
    let id = segment(id);
    if action.is_empty() {
        format!("/sandboxes/{id}")
    } else {
        format!("/sandboxes/{id}/{action}")
    }
}

/// Sends `request`; an answer outside 2xx is a fault, with Boat's own code and message.
async fn send(request: RequestBuilder) -> Result<Response, Fault> {
    let response = request.send().await.map_err(transport)?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let (code, message) = said(&body).unwrap_or_else(|| {
        let reason = status.canonical_reason().unwrap_or("error");
        let message = if body.trim().is_empty() {
            reason.to_owned()
        } else {
            body
        };
        (String::new(), message)
    });
    Err(fault(status, &code, message))
}

/// The capacity codes of a 503: Boat up, with no machine of the size free.
const NO_MACHINE: &[&str] = &["no_ready_machine", "out_of_capacity"];

/// The fault an answer outside 2xx is, `code` and `message` being what Boat said.
fn fault(status: StatusCode, code: &str, message: String) -> Fault {
    let line = format!("{message} (HTTP {})", status.as_u16());
    match status {
        StatusCode::NOT_FOUND => Fault::NotFound(line),
        StatusCode::UNAUTHORIZED => Fault::KeyRefused {
            status: status.as_u16(),
            said: message,
        },
        StatusCode::TOO_MANY_REQUESTS => Fault::NoPlace(line),
        StatusCode::SERVICE_UNAVAILABLE if NO_MACHINE.contains(&code) => Fault::NoPlace(line),
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
            Fault::Offline(line)
        }
        StatusCode::CONFLICT => Fault::Conflict {
            code: code.to_owned(),
            line,
        },
        _ => Fault::Failed(line),
    }
}

/// The `code` and `message` of a Boat error body.
fn said(body: &str) -> Option<(String, String)> {
    let body: Value = serde_json::from_str(body).ok()?;
    let message = body.get("message")?.as_str()?.to_owned();
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some((code, message))
}

async fn json<T: DeserializeOwned>(response: Response) -> Result<T, Fault> {
    let body = response.bytes().await.map_err(transport)?;
    serde_json::from_slice(&body)
        .map_err(|e| Fault::Failed(format!("an answer that does not read: {e}")))
}

/// A request that got no answer: Boat not reached, or not answering in its time; both are
/// Boat down (PROGRAMS.md §7).
fn transport(error: reqwest::Error) -> Fault {
    let line = chain(&error);
    if error.is_timeout() {
        Fault::Offline(format!("{line} (no answer in time)"))
    } else if error.is_connect() || error.is_request() {
        Fault::Offline(line)
    } else {
        Fault::Failed(line)
    }
}

/// An error with every cause under it, which reqwest keeps apart.
fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        line.push_str(": ");
        line.push_str(&inner.to_string());
        cause = inner.source();
    }
    line
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// A Boat on localhost that answers every request with `status` and `body`, or, where
    /// `status` is none, takes every request and never answers. Its URL.
    pub async fn boat_answering(status: Option<u16>, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                match status {
                    Some(status) => {
                        let answer = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(answer.as_bytes()).await;
                    }
                    None => held.push(socket),
                }
            }
        });
        url
    }

    pub fn boat_at(url: &str) -> Boat {
        Boat::new("boat_secret".into(), Some(url), None, None, Env::default()).unwrap()
    }

    async fn fault_of(status: Option<u16>, body: &'static str) -> Fault {
        let url = boat_answering(status, body).await;
        let boat = boat_at(&url);
        let request = boat.http.get(&url).timeout(Duration::from_millis(300));
        send(request).await.unwrap_err()
    }

    #[tokio::test]
    async fn boat_down_behind_its_gateway_is_offline() {
        for status in [502, 503, 504] {
            let fault = fault_of(Some(status), r#"{"message":"said"}"#).await;
            assert!(
                matches!(&fault, Fault::Offline(line) if line == &format!("said (HTTP {status})")),
                "{fault:?}"
            );
        }
        let fault = fault_of(None, "").await;
        assert!(
            matches!(&fault, Fault::Offline(line) if line.ends_with("(no answer in time)")),
            "{fault:?}"
        );
    }

    /// A full account or a size with no free machine is no place, which heals by waiting;
    /// a 503 without Boat's capacity code is Boat down.
    #[tokio::test]
    async fn no_room_at_boat_is_no_place() {
        let full = fault_of(
            Some(429),
            r#"{"code":"limit_reached","message":"100 at once"}"#,
        )
        .await;
        assert!(
            matches!(&full, Fault::NoPlace(line) if line == "100 at once (HTTP 429)"),
            "{full:?}"
        );
        let none = fault_of(
            Some(503),
            r#"{"code":"no_ready_machine","message":"none free"}"#,
        )
        .await;
        assert!(matches!(none, Fault::NoPlace(_)), "{none:?}");
    }

    #[tokio::test]
    async fn a_refused_key_and_other_answers_read_as_their_faults() {
        let fault = fault_of(Some(401), r#"{"code":"unauthorized","message":"said"}"#).await;
        assert!(
            matches!(&fault, Fault::KeyRefused { status: 401, said } if said == "said"),
            "{fault:?}"
        );
        assert!(matches!(
            fault_of(Some(404), "{}").await,
            Fault::NotFound(_)
        ));
        let expired = fault_of(
            Some(409),
            r#"{"code":"snapshot_data_expired","message":"gone"}"#,
        )
        .await;
        assert!(
            matches!(&expired, Fault::Conflict { code, .. } if code == "snapshot_data_expired"),
            "{expired:?}"
        );
        // A size the plan does not allow is Boat's answer, never the key's.
        assert!(matches!(
            fault_of(Some(403), r#"{"message":"trial"}"#).await,
            Fault::Failed(_)
        ));
        assert!(
            matches!(fault_of(Some(500), "oops").await, Fault::Failed(line) if line == "oops (HTTP 500)")
        );
    }

    #[test]
    fn a_host_carries_the_labels_asked_only_where_each_is_its_own() {
        let host: Host = serde_json::from_value(json!({
            "id": "bx_1", "state": "idle", "labels": {"steery": "host", "steery-key": "k"}
        }))
        .unwrap();
        let asked = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert!(host.carries(&asked(&[("steery", "host")])));
        assert!(host.carries(&asked(&[("steery", "host"), ("steery-key", "k")])));
        assert!(!host.carries(&asked(&[("steery", "host"), ("steery-key", "other")])));
        assert!(!host.carries(&asked(&[("steery-machine", "m")])));
    }
}
