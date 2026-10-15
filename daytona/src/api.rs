//! Daytona's published REST API, as far as this adapter calls it: the control plane
//! (`openapi.json`: hosts, their lifecycle, SSH access) and each host's toolbox
//! (`toolbox-openapi.json`: its files), reached through the toolbox proxy Daytona names.
//!
//! A host is what Daytona's API serves under `/sandbox`; that path is Daytona's word and is
//! sent as it is. Every item here is named by Steery's words: [`Host`] is one as Daytona
//! answers it.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode, multipart};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

/// Daytona's own API, where the person names none.
pub const DEFAULT_API: &str = "https://app.daytona.io/api";

/// Most hosts one page of `GET /sandbox` holds.
const PAGE: u32 = 200;

/// How a call to Daytona failed.
#[derive(Debug)]
pub enum Fault {
    /// Daytona has no such host or file (HTTP 404).
    NotFound(String),
    /// Daytona refused the API key (HTTP 401, 403), and what it said: a key that does not
    /// heal by waiting, given again by the person.
    KeyRefused { status: u16, said: String },
    /// Daytona is down: not reached, a gateway that answers for it (HTTP 502, 503, 504),
    /// or a request that got no answer in its time.
    Offline(String),
    /// Daytona answered, and the host did not reach the state asked for in time.
    TimedOut(String),
    /// Daytona already has what was to be made: a host by that name (HTTP 409).
    Conflict(String),
    /// Anything else Daytona answered, or a host that ended in a state it was not asked for.
    Failed(String),
}

/// A host as Daytona answers it, as far as the adapter reads it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub id: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub error_reason: Option<String>,
    #[serde(default)]
    pub toolbox_proxy_url: Option<String>,
}

impl Host {
    /// Its state, `unknown` where Daytona names none.
    pub fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("unknown")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    items: Vec<Host>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// What a host is made from, as `POST /sandbox` takes it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Create<'a> {
    pub name: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_info: Option<BuildInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<&'a str>,
    #[serde(skip_serializing_if = "Env::is_empty")]
    pub env: &'a Env,
}

/// The environment every host is made with, as the person's `DAYTONA_ENV` names it. Its
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

/// An image to build a host from: Daytona builds it from a Dockerfile.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub dockerfile_content: String,
}

impl BuildInfo {
    /// The image `reference` as it is.
    pub fn image(reference: &str) -> Self {
        Self {
            dockerfile_content: format!("FROM {reference}\n"),
        }
    }
}

/// SSH access to a host, as `POST /sandbox/{id}/ssh-access` answers it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAccess {
    pub token: String,
    #[serde(default)]
    pub ssh_command: Option<String>,
}

#[derive(Deserialize)]
struct FileInfo {
    size: u64,
}

/// A port's preview, as `GET /sandbox/{id}/ports/{port}/preview-url` answers it. Its
/// `token` is unread: a public host's URL needs none.
#[derive(Deserialize)]
struct Preview {
    url: String,
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
}

/// A client of one Daytona organization, by its API key.
pub struct Daytona {
    http: reqwest::Client,
    api: String,
    key: String,
    target: Option<String>,
    env: Env,
}

impl Daytona {
    pub fn new(
        key: String,
        api: Option<&str>,
        target: Option<String>,
        env: Env,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "steery-adapter-daytona/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let api = api.unwrap_or(DEFAULT_API).trim_end_matches('/').to_owned();
        Ok(Self {
            http,
            api,
            key,
            target,
            env,
        })
    }

    /// The API key, to keep out of every line.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn request(&self, method: Method, url: &str, within: Duration) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.key)
            .timeout(within)
    }

    fn control(
        &self,
        method: Method,
        path: &str,
        deadline: Deadline,
    ) -> Result<RequestBuilder, Fault> {
        Ok(self.request(method, &format!("{}{path}", self.api), deadline.within()?))
    }

    /// Every host carrying all of `labels`, page by page. Daytona's listing is an index
    /// that may lag a change by a second or more.
    pub async fn list(
        &self,
        labels: &BTreeMap<String, String>,
        deadline: Deadline,
    ) -> Result<Vec<Host>, Fault> {
        let labels = serde_json::to_string(labels).map_err(|e| Fault::Failed(e.to_string()))?;
        let limit = PAGE.to_string();
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![("labels", labels.clone()), ("limit", limit.clone())];
            if let Some(cursor) = cursor.take() {
                query.push(("cursor", cursor));
            }
            let page: Page = json(
                send(
                    self.control(Method::GET, "/sandbox", deadline)?
                        .query(&query),
                )
                .await?,
            )
            .await?;
            found.extend(page.items);
            match page.next_cursor.filter(|next| !next.is_empty()) {
                Some(next) => cursor = Some(next),
                None => return Ok(found),
            }
        }
    }

    pub async fn get(&self, id: &str, deadline: Deadline) -> Result<Host, Fault> {
        json(send(self.control(Method::GET, &host_path(id, ""), deadline)?).await?).await
    }

    pub async fn create(&self, create: &Create<'_>, deadline: Deadline) -> Result<Host, Fault> {
        json(
            send(
                self.control(Method::POST, "/sandbox", deadline)?
                    .json(create),
            )
            .await?,
        )
        .await
    }

    /// `POST /sandbox/{id}/{action}`: `start`, `stop`, `archive`, `last-activity`,
    /// `public/true`.
    pub async fn act(&self, id: &str, action: &str, deadline: Deadline) -> Result<(), Fault> {
        send(self.control(Method::POST, &host_path(id, action), deadline)?)
            .await
            .map(drop)
    }

    pub async fn delete(&self, id: &str, deadline: Deadline) -> Result<(), Fault> {
        send(self.control(Method::DELETE, &host_path(id, ""), deadline)?)
            .await
            .map(drop)
    }

    /// `GET /sandbox/{id}/ports/{port}/preview-url`: the URL Daytona's proxy reaches `port`
    /// of host `id` by.
    pub async fn preview(&self, id: &str, port: u16, deadline: Deadline) -> Result<String, Fault> {
        let path = host_path(id, &format!("ports/{port}/preview-url"));
        let preview: Preview =
            json(send(self.control(Method::GET, &path, deadline)?).await?).await?;
        Ok(preview.url)
    }

    pub async fn ssh_access(
        &self,
        id: &str,
        minutes: u32,
        deadline: Deadline,
    ) -> Result<SshAccess, Fault> {
        let request = self
            .control(Method::POST, &host_path(id, "ssh-access"), deadline)?
            .query(&[("expiresInMinutes", minutes)]);
        json(send(request).await?).await
    }

    /// Polls `id` until it is in one of `targets`, every `every`. A state in `fails`, or
    /// `destroyed` where it is not a target, fails; a host Daytona no longer has is
    /// `destroyed`. A failed reading is read again until the deadline, as Daytona's own
    /// clients do, but a refused key or a missing host is final.
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
            let reading = match self.get(id, deadline).await {
                Ok(host) => Ok(host),
                Err(Fault::NotFound(_)) if targets.contains(&"destroyed") => {
                    return Ok("destroyed".into());
                }
                Err(fault @ (Fault::NotFound(_) | Fault::KeyRefused { .. })) => return Err(fault),
                Err(fault) => Err(fault),
            };
            let still = match reading {
                Ok(host) => {
                    let state = host.state();
                    if targets.contains(&state) {
                        return Ok(state.to_owned());
                    }
                    if fails.contains(&state) || state == "destroyed" {
                        let reason = host.error_reason.as_deref().unwrap_or("no reason given");
                        return Err(Fault::Failed(format!(
                            "host {id} failed to {what}: {state}: {reason}"
                        )));
                    }
                    state.to_owned()
                }
                // The last reading's own fault: Daytona down to the end is offline.
                Err(fault) if deadline.left().is_none() => return Err(fault),
                Err(fault) => {
                    eprintln!("daytona: reading host {id} again: {fault:?}");
                    "unread".to_owned()
                }
            };
            let Some(left) = deadline.left() else {
                return Err(Fault::TimedOut(format!(
                    "host {id} still {still} after {} s, not {what}",
                    deadline.seconds()
                )));
            };
            tokio::time::sleep(every.min(left)).await;
        }
    }

    /// The base of `host`'s toolbox API.
    pub fn toolbox(&self, host: &Host) -> Result<String, Fault> {
        let proxy = host
            .toolbox_proxy_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .ok_or_else(|| Fault::Failed(format!("host {} has no toolbox URL", host.id)))?;
        Ok(format!("{}/{}", proxy.trim_end_matches('/'), host.id))
    }

    /// `POST /files/upload-v2`: `content` as the file `path`.
    pub async fn upload(
        &self,
        toolbox: &str,
        path: &str,
        content: Vec<u8>,
        within: Duration,
    ) -> Result<(), Fault> {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let form =
            multipart::Form::new().part("file", multipart::Part::bytes(content).file_name(name));
        let request = self
            .request(Method::POST, &format!("{toolbox}/files/upload-v2"), within)
            .query(&[("path", path)])
            .multipart(form);
        send(request).await.map(drop)
    }

    /// `GET /files/info`: the size of the file `path`.
    pub async fn size(&self, toolbox: &str, path: &str, within: Duration) -> Result<u64, Fault> {
        let request = self
            .request(Method::GET, &format!("{toolbox}/files/info"), within)
            .query(&[("path", path)]);
        Ok(json::<FileInfo>(send(request).await?).await?.size)
    }

    /// `POST /files/permissions`: the file `path` given `mode`.
    pub async fn chmod(
        &self,
        toolbox: &str,
        path: &str,
        mode: u32,
        within: Duration,
    ) -> Result<(), Fault> {
        let request = self
            .request(
                Method::POST,
                &format!("{toolbox}/files/permissions"),
                within,
            )
            .query(&[("path", path), ("mode", &format!("{mode:o}"))]);
        send(request).await.map(drop)
    }

    /// `DELETE /files`: the file `path`.
    pub async fn remove(&self, toolbox: &str, path: &str, within: Duration) -> Result<(), Fault> {
        let request = self
            .request(Method::DELETE, &format!("{toolbox}/files"), within)
            .query(&[("path", path)]);
        send(request).await.map(drop)
    }
}

/// The control plane's path of host `id`, and of its `action` where one is named.
fn host_path(id: &str, action: &str) -> String {
    let id: String = id
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect();
    if action.is_empty() {
        format!("/sandbox/{id}")
    } else {
        format!("/sandbox/{id}/{action}")
    }
}

/// Sends `request`; an answer outside 2xx is a fault, with Daytona's own message.
async fn send(request: RequestBuilder) -> Result<Response, Fault> {
    let response = request.send().await.map_err(transport)?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = said(&body).unwrap_or_else(|| {
        let reason = status.canonical_reason().unwrap_or("error");
        if body.trim().is_empty() {
            reason.to_owned()
        } else {
            body
        }
    });
    Err(fault(status, message))
}

/// The fault an answer outside 2xx is, `message` being what Daytona said.
fn fault(status: StatusCode, message: String) -> Fault {
    let line = format!("{message} (HTTP {})", status.as_u16());
    match status {
        StatusCode::NOT_FOUND => Fault::NotFound(line),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Fault::KeyRefused {
            status: status.as_u16(),
            said: message,
        },
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
            Fault::Offline(line)
        }
        StatusCode::CONFLICT => Fault::Conflict(line),
        _ => Fault::Failed(line),
    }
}

/// The `message` of a Daytona error body: a string, or a list of them.
fn said(body: &str) -> Option<String> {
    match serde_json::from_str::<Value>(body).ok()?.get("message")? {
        Value::String(message) => Some(message.clone()),
        Value::Array(messages) => Some(
            messages
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        ),
        _ => None,
    }
}

async fn json<T: DeserializeOwned>(response: Response) -> Result<T, Fault> {
    let body = response.bytes().await.map_err(transport)?;
    serde_json::from_slice(&body)
        .map_err(|e| Fault::Failed(format!("an answer that does not read: {e}")))
}

/// A request that got no answer: Daytona not reached, or not answering in its time; both
/// are Daytona down (PROGRAMS.md §7).
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

    /// A Daytona on localhost that answers every request with `status` and `body`, or,
    /// where `status` is none, takes every request and never answers. Its URL.
    pub async fn daytona_answering(status: Option<u16>, body: &'static str) -> String {
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

    async fn fault_of(status: Option<u16>) -> Fault {
        let url = daytona_answering(status, r#"{"message":"said"}"#).await;
        let daytona = Daytona::new("sk".into(), Some(&url), None, Env::default()).unwrap();
        let request = daytona.request(Method::GET, &url, Duration::from_millis(300));
        send(request).await.unwrap_err()
    }

    #[tokio::test]
    async fn daytona_down_behind_its_gateway_is_offline() {
        for status in [502, 503, 504] {
            let fault = fault_of(Some(status)).await;
            assert!(
                matches!(&fault, Fault::Offline(line) if line == &format!("said (HTTP {status})")),
                "{fault:?}"
            );
        }
        let fault = fault_of(None).await;
        assert!(
            matches!(&fault, Fault::Offline(line) if line.ends_with("(no answer in time)")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_key_and_other_answers_read_as_their_faults() {
        for status in [401, 403] {
            let fault = fault_of(Some(status)).await;
            assert!(
                matches!(&fault, Fault::KeyRefused { status: got, said } if *got == status && said == "said"),
                "{fault:?}"
            );
        }
        assert!(matches!(fault_of(Some(404)).await, Fault::NotFound(_)));
        assert!(matches!(fault_of(Some(409)).await, Fault::Conflict(_)));
        assert!(matches!(fault_of(Some(500)).await, Fault::Failed(_)));
    }
}
