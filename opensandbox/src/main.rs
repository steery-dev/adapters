//! Steery's adapter for OpenSandbox: it provides hosts (PROTOCOL.md §3.2, §5.2, §5.3).
//!
//! The runtime starts it and speaks the program protocol on standard input and output. It
//! talks to OpenSandbox's lifecycle API and to the execd daemon inside each host over
//! their published HTTP and WebSocket APIs. The host, its image, credentials and network
//! are the company's and OpenSandbox's: a host is given no image, environment, secret or
//! network setting of the adapter's own.

mod channel;
mod provider;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use steery_sdk::wire::hosts::{self, Ensure, Named, Put, Source, State};
use steery_sdk::wire::{self, Empty};
use steery_sdk::{Error, PROTOCOL, Program, method, params as given, refusal, result};

use crate::channel::Channels;
use crate::provider::{Fail, Found, Provider};

/// The metadata every host this adapter makes carries, and every list is filtered by.
const MINE: (&str, &str) = ("steery", "host");
/// The metadata that finds a host again by the runtime's key.
const KEY_LABEL: &str = "steery-key";
/// The metadata that keeps the TTL a host was made with, in seconds, for keepalive.
const TTL_LABEL: &str = "steery-ttl";
/// The TTL keepalive renews by where a host kept none.
const DEFAULT_TTL_SECONDS: i64 = 3600;
/// OpenSandbox requires an entrypoint with an image; this one only keeps the host up.
const IDLE_ENTRYPOINT: [&str; 3] = ["tail", "-f", "/dev/null"];
/// How long a host may take to make, resume or pause, inside the runtime's bounds
/// (PROTOCOL.md §9: 300 s, 60 s).
const MAKE_WAIT: Duration = Duration::from_secs(280);
const CHANGE_WAIT: Duration = Duration::from_secs(55);
const POLL: Duration = Duration::from_secs(1);
/// The states a host found by key is never reused from.
const GONE: [&str; 2] = ["Stopping", "Terminated"];

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut shared = None;
    let served = steery_sdk::serve(|notifier| {
        let channels = Arc::new(Channels::new(notifier));
        shared = Some(channels.clone());
        OpenSandbox {
            provider: RwLock::new(Err("initialize comes first".into())),
            channels,
            keys: Keys::default(),
        }
    })
    .await;
    if let Some(channels) = shared {
        channels.shut().await;
    }
    served
}

/// The adapter: the provider its settings name, and its open channels.
struct OpenSandbox {
    provider: RwLock<Result<Arc<Provider>, String>>,
    channels: Arc<Channels>,
    keys: Keys,
}

/// One lock per key a `hosts/ensure` runs for: two ensures for one key in this process
/// run one after the other, so the second finds what the first made.
#[derive(Default)]
struct Keys(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl Keys {
    fn of(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// Forget `key`'s lock once no other ensure holds or waits for it.
    fn release(&self, key: &str, lock: Arc<tokio::sync::Mutex<()>>) {
        let mut keys = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if Arc::strong_count(&lock) == 2 {
            keys.remove(key);
        }
    }
}

impl OpenSandbox {
    /// The provider, or the refusal its settings are.
    fn provider(&self) -> Result<Arc<Provider>, Error> {
        match &*self.provider.read().unwrap_or_else(|e| e.into_inner()) {
            Ok(provider) => Ok(provider.clone()),
            Err(why) => Err(Error::refused(
                refusal::REFUSED,
                format!("OpenSandbox: {why}"),
            )),
        }
    }
}

impl Program for OpenSandbox {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        *self.provider.write().unwrap_or_else(|e| e.into_inner()) =
            Provider::new(&params.settings).map(Arc::new);
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "opensandbox".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "OpenSandbox".into(),
                short: "OS".into(),
                tint: None,
                mark: None,
            },
            harness: None,
            machine: Some(wire::Machine::Hosts(wire::HostFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::Ttl,
                optional: vec![wire::Optional::Pause, wire::Optional::Keepalive],
                builds: false,
                channel: wire::Channel::Stream,
                os: wire::Os::Linux,
                unavailable: None,
            })),
            settings: vec![
                wire::Setting {
                    name: "OPENSANDBOX_URL".into(),
                    secret: false,
                    required: true,
                    about:
                        "Your OpenSandbox server's URL, as its SDKs take it: http://localhost:8080"
                            .into(),
                },
                wire::Setting {
                    name: "OPENSANDBOX_API_KEY".into(),
                    secret: true,
                    required: false,
                    about: "The server's API key (its server.api_key), where it has one".into(),
                },
            ],
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::HOSTS_LIST => result(list(&*self.provider()?, given(params)?).await?),
            method::HOSTS_ENSURE => {
                let asked: Ensure = given(params)?;
                let provider = self.provider()?;
                let key = asked.key.clone();
                let lock = self.keys.of(&key);
                let answer = {
                    let _held = lock.lock().await;
                    ensure(&provider, asked).await
                };
                self.keys.release(&key, lock);
                result(answer?)
            }
            method::HOSTS_START => result(start(&*self.provider()?, given(params)?).await?),
            method::HOSTS_STOP | method::HOSTS_PAUSE => {
                result(pause(&*self.provider()?, given(params)?).await?)
            }
            method::HOSTS_DELETE => result(delete(&*self.provider()?, given(params)?).await?),
            method::HOSTS_KEEPALIVE => {
                result(keepalive(&*self.provider()?, given(params)?).await?)
            }
            method::HOSTS_PUT => result(put(&*self.provider()?, given(params)?).await?),
            method::HOSTS_CHANNEL => {
                let asked = given(params)?;
                result(self.channels.channel(self.provider()?, asked).await?)
            }
            method::STREAM_WRITE => result(self.channels.write(given(params)?).await?),
            method::STREAM_CLOSE => result(self.channels.close(given(params)?).await?),
            _ => Err(Error::Unknown),
        }
    }
}

/// `name` as given, refused where it is empty.
fn named<'a>(value: &'a str, name: &str) -> Result<&'a str, Error> {
    if value.is_empty() {
        Err(Error::invalid(format!("{name} must be a non-empty string")))
    } else {
        Ok(value)
    }
}

/// An RFC 3339 time in milliseconds since 1970.
fn millis(at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// Read the host until its state is one of `want`, or until `wait` has passed.
async fn until(
    provider: &Provider,
    id: &str,
    want: &[&str],
    wait: Duration,
) -> Result<Found, Fail> {
    let deadline = Instant::now() + wait;
    loop {
        let found = provider.get(id).await?;
        let state = found.status.state.as_str();
        if want.contains(&state) {
            return Ok(found);
        }
        if state == "Failed" || state == "Terminated" {
            let said = found
                .status
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
                .map(|message| format!(": {message}"))
                .unwrap_or_default();
            return Err(Fail::Refused(
                refusal::START_FAILED,
                format!("host {id} is {state}{said}"),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Fail::Refused(
                refusal::START_FAILED,
                format!("host {id} is still {state}, not {}", want.join(" or ")),
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn list(provider: &Provider, asked: hosts::List) -> Result<hosts::Listed, Error> {
    let mut labels = asked.labels.unwrap_or_default();
    labels.insert(MINE.0.into(), MINE.1.into());
    let found = provider
        .listed(&labels)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
    let hosts = found
        .into_iter()
        .map(|found| hosts::Host {
            created_at: millis(&found.created_at)
                .and_then(|at| u64::try_from(at).ok())
                .unwrap_or(0),
            host: found.id,
            state: found.status.state,
            labels: found.metadata.unwrap_or_default(),
        })
        .collect();
    Ok(hosts::Listed { hosts })
}

async fn ensure(provider: &Provider, asked: Ensure) -> Result<Named, Error> {
    let key = named(&asked.key, "key")?;
    let mine = BTreeMap::from([
        (MINE.0.to_owned(), MINE.1.to_owned()),
        (KEY_LABEL.to_owned(), key.to_owned()),
    ]);
    let listed = provider
        .listed(&mine)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
    if let Some(found) = oldest(listed.iter()) {
        return Ok(Named {
            host: found.id.clone(),
        });
    }
    let mut metadata = asked.labels;
    metadata.extend(mine.clone());
    // `resourceLimits` is required outside pool mode; empty, the server's own defaults apply.
    let body = match &asked.source {
        Some(Source::Image(image)) => json!({
            "image": { "uri": image },
            "entrypoint": IDLE_ENTRYPOINT,
            "resourceLimits": {},
            "metadata": metadata,
        }),
        Some(Source::Snapshot(snapshot)) => json!({
            "snapshotId": snapshot,
            "resourceLimits": {},
            "metadata": metadata,
        }),
        // Its facts say no `builds`, so the runtime never asks for one.
        Some(Source::Dockerfile(_)) => {
            return Err(Error::invalid(
                "OpenSandbox builds no Dockerfile: name an image or a snapshot",
            ));
        }
        None => {
            return Err(Error::refused(
                refusal::NO_PLACE,
                "OpenSandbox needs an image or a snapshot to make a host: it has no default. Name one as the machine's source.",
            ));
        }
    };
    let made = provider
        .create(&body)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::START_FAILED))?;
    // OpenSandbox takes no name to make a host by and has nothing to lock on, so an
    // ensure for the same key in another process may have made one at the same time: the
    // oldest of them is the key's host, and a younger one made here is deleted.
    let others = match provider.listed(&mine).await {
        Ok(listed) => listed,
        Err(fail) => {
            eprintln!("{}", provider.line(fail.said()));
            Vec::new()
        }
    };
    // Each is compared as the listing reads it: the creation time the server answered a
    // create with is not the one it lists.
    let listed_made = others.iter().any(|found| found.id == made.id);
    let kept = oldest(others.iter().chain((!listed_made).then_some(&made)))
        .map_or(made.id.clone(), |found| found.id.clone());
    if kept != made.id {
        if let Err(fail) = provider.delete(&made.id).await {
            eprintln!("{}", provider.line(fail.said()));
        }
        return Ok(Named { host: kept });
    }
    // The server gave it a TTL: keep it, so keepalive renews by the same.
    let ttl = made
        .expires_at
        .as_deref()
        .and_then(millis)
        .zip(millis(&made.created_at))
        .map(|(expires, created)| (expires - created + 500) / 1000)
        .filter(|ttl| *ttl > 0);
    if let Some(ttl) = ttl {
        let patch = json!({ TTL_LABEL: ttl.to_string() });
        if let Err(fail) = provider.patch_metadata(&made.id, &patch).await {
            eprintln!("{}", provider.line(fail.said()));
        }
    }
    Ok(Named { host: made.id })
}

/// The oldest host that is not going away; of two made at once, the one whose id sorts
/// first.
fn oldest<'a>(found: impl Iterator<Item = &'a Found>) -> Option<&'a Found> {
    found
        .filter(|found| !GONE.contains(&found.status.state.as_str()))
        .min_by_key(|found| (millis(&found.created_at).unwrap_or(i64::MAX), &found.id))
}

async fn start(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.host, "host")?;
    let started = async {
        let mut found = provider.get(id).await?;
        if found.status.state == "Pausing" {
            found = until(provider, id, &["Paused"], MAKE_WAIT).await?;
        }
        match found.status.state.as_str() {
            "Paused" => {
                provider.act(id, "resume").await?;
                found = until(provider, id, &["Running"], MAKE_WAIT).await?;
            }
            "Pending" | "Resuming" => {
                found = until(provider, id, &["Running"], MAKE_WAIT).await?;
            }
            _ => {}
        }
        Ok(State {
            state: found.status.state,
        })
    };
    started
        .await
        .map_err(|fail| provider.refuse(fail, refusal::START_FAILED))
}

/// Pause: OpenSandbox's only stop that keeps the host.
async fn pause(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.host, "host")?;
    let paused = async {
        let mut found = provider.get(id).await?;
        if matches!(found.status.state.as_str(), "Pending" | "Resuming") {
            found = until(provider, id, &["Running"], CHANGE_WAIT).await?;
        }
        match found.status.state.as_str() {
            "Running" => {
                provider.act(id, "pause").await?;
                found = until(provider, id, &["Paused"], CHANGE_WAIT).await?;
            }
            "Pausing" => {
                found = until(provider, id, &["Paused"], CHANGE_WAIT).await?;
            }
            _ => {}
        }
        Ok(State {
            state: found.status.state,
        })
    };
    paused
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

async fn delete(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.host, "host")?;
    let deleted = async {
        provider.delete(id).await?;
        match provider.get(id).await {
            Ok(found) => Ok(State {
                state: found.status.state,
            }),
            Err(fail) if fail.is_not_found() => Ok(State {
                state: "Terminated".into(),
            }),
            Err(fail) => Err(fail),
        }
    };
    deleted
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

/// Renew the host's expiration by the TTL it was made with; one with none never expires.
async fn keepalive(provider: &Provider, asked: Named) -> Result<Empty, Error> {
    let id = named(&asked.host, "host")?;
    let kept = async {
        let found = provider.get(id).await?;
        if found.expires_at.is_none() {
            return Ok(Empty {});
        }
        let ttl = found
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(TTL_LABEL))
            .and_then(|ttl| ttl.parse::<i64>().ok())
            .filter(|ttl| *ttl > 0)
            .unwrap_or(DEFAULT_TTL_SECONDS);
        let expires_at = (Utc::now() + chrono::Duration::seconds(ttl))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        provider.renew(id, &expires_at).await?;
        Ok(Empty {})
    };
    kept.await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

/// Write the file through execd, check that every byte is there, and remove what is not
/// whole before refusing (PROTOCOL.md §5.2).
async fn put(provider: &Provider, asked: Put) -> Result<Empty, Error> {
    let id = named(&asked.host, "host")?;
    let path = asked
        .path
        .to_str()
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| Error::invalid("path must be absolute"))?;
    let content = STANDARD
        .decode(asked.content.as_bytes())
        .map_err(|_| Error::invalid("content must be base64"))?;
    let length = content.len() as u64;
    let execd = provider
        .execd(id)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
    let written = async {
        provider.upload(&execd, path, content, asked.mode).await?;
        let size = provider.size(&execd, path).await?;
        if size == length {
            Ok(())
        } else {
            Err(Fail::Other(format!(
                "upload file: {path} holds {size} of {length} bytes"
            )))
        }
    };
    match written.await {
        Ok(()) => Ok(Empty {}),
        Err(fail) => {
            if let Err(left) = provider.remove_file(&execd, path).await
                && !left.is_not_found()
            {
                eprintln!("{}", provider.line(left.said()));
            }
            Err(provider.refuse(fail, refusal::REFUSED))
        }
    }
}
