//! Steery's `boat` adapter. It provides `hosts` (PROTOCOL.md §3.2, §5.2) on Boat
//! (boat.dev), through Boat's published REST API and OpenSSH. Boat's own words for a host
//! (its API paths) stay inside [`api`], which names the items around them by Steery's.
//!
//! Its client is built from the settings `initialize` passes, and from nothing else: the
//! host, its credentials and its network are the person's and Boat's. What a host gets
//! beside Boat's own image is what the person names: a Boat environment (`BOAT_ENVIRONMENT`,
//! absent: the account's default, with the secrets, repositories and logins it carries),
//! variables (`BOAT_ENV`) and a size (`BOAT_TYPE`); and, where the runtime names a
//! Dockerfile, the image it builds ([`image`]), in a template every host of it is forked
//! from ([`Template`]). Boat's auto-stop stays the host's idle timer: set on each make and
//! resume, and moved on by `keepalive`.

mod api;
mod image;
mod ssh;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steery_sdk::wire::{self, hosts};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal};

use api::{Boat, Create, Deadline, Env, FAILED, Fault, Fork, MAKING, RUNNING};

/// The label every host this adapter makes carries, and every list is filtered by.
const MINE: (&str, &str) = ("steery", "host");
/// The label that finds a host again by the runtime's key.
const KEY_LABEL: &str = "steery-key";
/// The label of a template, the host a source's hosts are forked from ([`Template`]).
const TEMPLATE: (&str, &str) = ("steery", "template");
/// The name of a saved snapshot's template starts so.
const SNAPSHOT_PREFIX: &str = "steery-snapshot-";
/// Seconds to a host's auto-stop, from its make, its resume and each keepalive: past the
/// runtime's own idle stop, so Boat stops only a host the runtime no longer reads.
const TTL_SECONDS: u32 = 30 * 60;
/// Seconds to a template's auto-stop while it is being made: past any build, so a template
/// left behind stops itself. A made one is kept stopped.
const BUILD_TTL_SECONDS: u32 = 60 * 60;
/// How many of Steery's images are kept: the newest, and the two before it, which another
/// computer on an older release may still make hosts from.
const IMAGES_KEPT: usize = 3;
/// Seconds the adapter waits for Boat, inside the runtime's bounds (§9: 300 s, 60 s).
const LONG_SECONDS: u64 = 280;
const CHANGE_SECONDS: u64 = 55;
/// How often a host changing state is read again, and a build.
const POLL: Duration = Duration::from_secs(1);
const BUILD_POLL: Duration = Duration::from_secs(3);
/// The longest line a refusal carries.
const LINE_CHARS: usize = 300;
/// The sizes Boat makes.
const SIZES: &[&str] = &["small", "default", "large", "xlarge"];
/// How long a stopped host is kept before its disk is refreshed. Boat deletes a stopped
/// sandbox's disk 14 days after it stopped; one stopped this long is started and stopped
/// again, which keeps it 14 days more, so the runtime's own 30-day delete is what ends a
/// host that is never used again, as on Daytona, and the model keeper's sign-ins are kept.
const REFRESH_AFTER: Duration = Duration::from_secs(10 * 24 * 60 * 60);
/// How often stopped hosts are looked at, and how long after the adapter starts first.
const REFRESH_EVERY: Duration = Duration::from_secs(60 * 60);
const REFRESH_FIRST: Duration = Duration::from_secs(10 * 60);
/// How long a refreshed host runs before it is stopped again: a start the runtime makes
/// meanwhile has its `steery host serve` running by then, and that host is left running.
const SETTLE: Duration = Duration::from_secs(20);

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    steery_sdk::serve(|_notifier| Adapter::default()).await
}

/// The adapter: a Boat client once `initialize` gave an API key, and none before; whether
/// this computer has the OpenSSH client its channels run; where its key pair is kept, and
/// the pair once read; and the one build of an image at a time.
#[derive(Default)]
struct Adapter {
    boat: OnceLock<Option<Arc<Boat>>>,
    ssh: OnceLock<bool>,
    folder: OnceLock<PathBuf>,
    keys: Mutex<Option<ssh::Keys>>,
    building: tokio::sync::Mutex<()>,
}

/// Why no channel can be had here: Boat's runs `ssh` on the controlling computer, and
/// waiting does not install one. Said by `initialize` as `unavailable` (PROTOCOL.md §3.2),
/// so the machine is never added where it cannot work, and by `hosts/channel`, `hosts/put`
/// and `hosts/list` as the guard where it is used.
const NO_SSH: &str = "Boat's channel runs `ssh` and `ssh-keygen` on this computer, and they are not installed: install an OpenSSH client (e.g. apt install openssh-client)";

impl Adapter {
    /// `ssh` and `ssh-keygen` are on `PATH`, as found when the adapter was initialized.
    fn reach(&self) -> Result<(), Error> {
        let found = *self.ssh.get_or_init(|| {
            std::env::var_os("PATH")
                .is_some_and(|path| within(&path, "ssh") && within(&path, "ssh-keygen"))
        });
        found
            .then_some(())
            .ok_or_else(|| Error::refused(refusal::REFUSED, NO_SSH))
    }

    /// The adapter's key pair, made in its folder the first time one is needed.
    fn keys(&self) -> Result<ssh::Keys, Error> {
        self.reach()?;
        let mut kept = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(keys) = &*kept {
            return Ok(keys.clone());
        }
        let folder = self
            .folder
            .get()
            .ok_or_else(|| Error::invalid("the adapter was not initialized"))?;
        let keys = ssh::Keys::at(folder)
            .map_err(|why| Error::refused(refusal::REFUSED, format!("Boat: {why}")))?;
        *kept = Some(keys.clone());
        Ok(keys)
    }
}

/// Whether a folder of `path` (as `PATH` spells it) holds an executable `program`.
fn within(path: &std::ffi::OsStr, program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).any(|dir| {
        std::fs::metadata(dir.join(program))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

impl Program for Adapter {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let given = |name: &str| {
            params
                .settings
                .get(name)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        };
        let env = match params.settings.get("BOAT_ENV") {
            Some(lines) => env_of(lines).map_err(|why| Error::refused(refusal::REFUSED, why))?,
            None => Env::default(),
        };
        let size = given("BOAT_TYPE");
        if let Some(size) = size.filter(|size| !SIZES.contains(size)) {
            return Err(Error::refused(
                refusal::REFUSED,
                format!("BOAT_TYPE is {size}: give small, default, large or xlarge"),
            ));
        }
        let boat = match given("BOAT_API_KEY") {
            Some(key) => Some(
                Boat::new(
                    key.to_owned(),
                    given("BOAT_API_URL"),
                    size.map(str::to_owned),
                    given("BOAT_ENVIRONMENT").map(str::to_owned),
                    env,
                )
                .map_err(|why| Error::refused(refusal::REFUSED, format!("Boat: {why}")))?,
            ),
            None => None,
        };
        let boat = boat.map(Arc::new);
        // Every stopped host's disk is kept past Boat's 14 days while this adapter runs.
        if let (Ok(()), Some(boat), Ok(runtime)) = (
            self.boat.set(boat.clone()),
            boat,
            tokio::runtime::Handle::try_current(),
        ) {
            runtime.spawn(refreshing(boat));
        }
        let _ = self.folder.set(params.config.join("adapters").join("boat"));
        // Said up front (PROTOCOL.md §3.2), so no machine is added where its channel cannot
        // run; the channel, the put and the list still refuse, as the guard where used.
        let unavailable = self.reach().err().map(|_| NO_SSH.to_owned());
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "boat".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Boat".into(),
                short: "Bt".into(),
                tint: None,
                mark: None,
            },
            harness: None,
            machine: Some(wire::Machine::Hosts(wire::HostFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::Ttl,
                optional: vec![wire::Optional::Keepalive, wire::Optional::Expose],
                builds: true,
                channel: wire::Channel::Argv,
                os: wire::Os::Linux,
                unavailable,
            })),
            settings: vec![
                setting(
                    "BOAT_API_KEY",
                    true,
                    true,
                    "An API key from boat.dev/dashboard (API keys), or `boat api-key create steery`",
                ),
                setting(
                    "BOAT_TYPE",
                    false,
                    false,
                    "The size of every sandbox: small, default, large or xlarge; absent: Boat's default (4 vCPU, 8 GB)",
                ),
                setting(
                    "BOAT_ENVIRONMENT",
                    false,
                    false,
                    "The Boat environment every sandbox gets: its secrets, secret files, repositories and logins; absent: your account's default",
                ),
                setting(
                    "BOAT_ENV",
                    true,
                    false,
                    "Environment for every sandbox, one NAME=VALUE per line (# comments); passed as Boat's env",
                ),
                setting(
                    "BOAT_API_URL",
                    false,
                    false,
                    "Boat's API URL, for a Boat other than boat.dev; absent: https://boat.dev/api/v1",
                ),
            ],
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::{params as given, result};
        let boat = || {
            self.boat.get().and_then(Option::as_ref).ok_or_else(|| {
                Error::refused(
                    refusal::REFUSED,
                    "Boat: no API key was given: give BOAT_API_KEY",
                )
            })
        };
        match method {
            method::HOSTS_LIST => {
                self.reach()?;
                result(list(boat()?, given(params)?).await?)
            }
            method::HOSTS_ENSURE => result(ensure(boat()?, &self.building, given(params)?).await?),
            method::HOSTS_START => result(start(boat()?, given(params)?).await?),
            method::HOSTS_STOP => result(stop(boat()?, given(params)?).await?),
            method::HOSTS_DELETE => result(delete(boat()?, given(params)?).await?),
            method::HOSTS_KEEPALIVE => result(keepalive(boat()?, given(params)?).await?),
            method::HOSTS_EXPOSE => result(expose(boat()?, given(params)?).await?),
            method::HOSTS_PUT => {
                let keys = self.keys()?;
                result(put(boat()?, &keys, given(params)?).await?)
            }
            method::HOSTS_CHANNEL => {
                let keys = self.keys()?;
                result(channel(boat()?, &keys, given(params)?).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}

fn setting(name: &str, secret: bool, required: bool, about: &str) -> wire::Setting {
    wire::Setting {
        name: name.into(),
        secret,
        required,
        about: about.into(),
    }
}

/// The environment `BOAT_ENV` names: one `NAME=VALUE` per line, blank lines and `#` lines
/// skipped; the value is everything after the first `=`, as it is. A malformed line is
/// refused by its number, never its text, which may hold a secret.
fn env_of(lines: &str) -> Result<Env, String> {
    let mut env = BTreeMap::new();
    for (at, line) in lines.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = at + 1;
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| format!("BOAT_ENV line {number} has no `=`: write NAME=VALUE"))?;
        let named = name.len() <= 128
            && name
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !named {
            return Err(format!(
                "BOAT_ENV line {number}: a name is at most 128 letters, digits and _, not starting with a digit"
            ));
        }
        env.insert(name.to_owned(), value.to_owned());
    }
    Ok(Env(env))
}

/// The refusal a fault is: `otherwise` where it is none of not found, key, down, room or
/// time, and `room` where Boat had no room. A refused key is `refused`, never `offline`:
/// it does not heal by waiting, and its line names the setting to give again. Its line
/// never holds the API key.
fn refuse(boat: &Boat, fault: Fault, otherwise: &'static str, room: &'static str) -> Error {
    let (word, line) = match fault {
        Fault::NotFound(line) => (refusal::NOT_FOUND, format!("Boat: {line}")),
        Fault::KeyRefused { status, said } => (
            refusal::REFUSED,
            format!("Boat refused the API key (HTTP {status}: {said}): give BOAT_API_KEY again"),
        ),
        Fault::Offline(line) => (refusal::OFFLINE, format!("Boat: {line}")),
        // Boat answered, and the host was not there in time.
        Fault::TimedOut(line) => (refusal::REFUSED, format!("Boat: {line} (timed out)")),
        Fault::NoPlace(line) => (room, format!("Boat: {line}")),
        Fault::Conflict { line, .. } | Fault::Failed(line) => (otherwise, format!("Boat: {line}")),
    };
    Error::refused(word, one_line(boat.key(), &line))
}

/// `text` on one line, without `key`, at most [`LINE_CHARS`] characters.
fn one_line(key: &str, text: &str) -> String {
    let text = if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, "[redacted]")
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(LINE_CHARS - 1) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// A host's id, never empty.
fn named(id: &str) -> Result<&str, Error> {
    if id.is_empty() {
        Err(Error::invalid("host must be a non-empty string"))
    } else {
        Ok(id)
    }
}

/// Milliseconds since 1970 of a time Boat writes (RFC 3339); 0 where it is none.
fn millis(at: Option<&str>) -> u64 {
    at.and_then(|at| {
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
    })
    .and_then(|at| u64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok())
    .unwrap_or(0)
}

/// Milliseconds since 1970, now.
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0)
}

/// The first 128 bits of the SHA-256 of `parts`, each ended by a 0 byte, in hex.
fn digest(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hash.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn list(boat: &Boat, params: hosts::List) -> Result<hosts::Listed, Error> {
    let mut labels = params.labels.unwrap_or_default();
    labels.insert(MINE.0.into(), MINE.1.into());
    let found = boat
        .list(&labels, None, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    let hosts = found
        .into_iter()
        // A cancelled make got no machine and was removed: no host.
        .filter(|host| host.state() != "cancelled")
        .map(|host| hosts::Host {
            state: host.state().to_owned(),
            created_at: millis(host.created_at.as_deref()),
            labels: host.labels.unwrap_or_default(),
            host: host.id,
        })
        .collect();
    Ok(hosts::Listed { hosts })
}

/// The host made for `key`, or one made for it now and waited for until it runs: forked
/// from the template of the source asked (the Dockerfile's image, or a saved snapshot),
/// made first where there is none yet, or made from Boat's own image.
///
/// It is found by its labels, which Boat's listing reads back at once. A make whose answer
/// was lost is made once all the same: Boat takes each under an idempotency key made from
/// `key` and what it is made from, and answers the same host for it again.
async fn ensure(
    boat: &Boat,
    building: &tokio::sync::Mutex<()>,
    params: hosts::Ensure,
) -> Result<hosts::Named, Error> {
    if params.key.is_empty() {
        return Err(Error::invalid("key must be a non-empty string"));
    }
    let deadline = Deadline::after(LONG_SECONDS);
    let fail = |fault| refuse(boat, fault, refusal::START_FAILED, refusal::NO_PLACE);
    let mut mine = BTreeMap::from([
        (MINE.0.to_owned(), MINE.1.to_owned()),
        (KEY_LABEL.to_owned(), params.key.clone()),
    ]);
    // Found before anything is built: a host keeps what it was made from.
    if let Some(host) = found(boat, &mine, deadline).await.map_err(fail)? {
        return Ok(hosts::Named { host });
    }
    let template = match &params.source {
        Some(hosts::Source::Snapshot(name)) => Some(Template::snapshot(name)),
        Some(hosts::Source::Image(_)) => {
            return Err(Error::invalid(
                "Boat makes sandboxes from its own image or a saved snapshot, and pulls no image: name a saved snapshot",
            ));
        }
        Some(hosts::Source::Dockerfile(text)) => {
            let image = image::of(text).map_err(|why| {
                Error::refused(
                    refusal::START_FAILED,
                    format!("Boat cannot build this image: {why}"),
                )
            })?;
            Some(Template::image(image))
        }
        None => None,
    };
    let mut labels = params.labels;
    labels.append(&mut mine);
    let host = match template {
        Some(template) => forked(boat, building, &template, &params.key, &labels, deadline).await,
        None => {
            let name = format!("steery-{}", digest(&[&params.key]));
            let create = Create {
                name: &name,
                labels: &labels,
                ttl_seconds: TTL_SECONDS,
                size: boat.size(),
                from: None,
                environment: boat.environment(),
                env: boat.env(),
                setup_script: None,
            };
            made(boat, &create, &name, deadline).await
        }
    }
    .map_err(fail)?;
    Ok(hosts::Named { host })
}

/// The oldest host carrying `labels` that Boat did not cancel, waited for while it is
/// being made.
async fn found(
    boat: &Boat,
    labels: &BTreeMap<String, String>,
    deadline: Deadline,
) -> Result<Option<String>, Fault> {
    let mut hosts: Vec<api::Host> = boat
        .list(labels, None, deadline)
        .await?
        .into_iter()
        .filter(|host| host.state() != "cancelled")
        .collect();
    hosts.sort_by_key(|host| millis(host.created_at.as_deref()));
    let Some(host) = hosts.into_iter().next() else {
        return Ok(None);
    };
    if MAKING.contains(&host.state()) {
        let mut targets = RUNNING.to_vec();
        targets.push("archived");
        boat.wait(&host.id, &targets, FAILED, "start", POLL, deadline)
            .await?;
    }
    Ok(Some(host.id))
}

/// Makes the host `create` names under the idempotency key `once`, and waits until it
/// runs. A make Boat answers with a host it no longer has was taken under that key before
/// and since deleted: it is made again, once, under a key of its own.
async fn made(
    boat: &Boat,
    create: &Create<'_>,
    once: &str,
    deadline: Deadline,
) -> Result<String, Fault> {
    let mut once = once.to_owned();
    let mut again = false;
    loop {
        let host = match boat.create(create, &once, deadline).await {
            Ok(host) => host,
            // The first make under this key is still being taken.
            Err(Fault::Conflict { code, .. }) if code == "idempotency_in_progress" => {
                deadline
                    .pause(POLL, || "Boat was still taking the make".to_owned())
                    .await?;
                continue;
            }
            Err(fault) => return Err(fault),
        };
        if host.state() == "cancelled" {
            return Err(Fault::NoPlace(format!(
                "Boat got no machine for the host: {}",
                host.error.as_deref().unwrap_or("cancelled")
            )));
        }
        if RUNNING.contains(&host.state()) {
            return Ok(host.id);
        }
        match boat
            .wait(&host.id, RUNNING, FAILED, "start", POLL, deadline)
            .await
        {
            Ok(_) => return Ok(host.id),
            Err(Fault::NotFound(_)) if !again => {
                again = true;
                once = format!("{once}-{}", now_millis());
            }
            Err(fault) => return Err(fault),
        }
    }
}

/// What a source's hosts are forked from: a host of its own, kept stopped, made once per
/// source and found again by its name. Boat keeps none of the labels a make from a saved
/// snapshot asks for, and a fork keeps them, so every host made from a source is a fork of
/// its template; a fork of a stopped host also runs in seconds.
struct Template {
    /// `steery-image-<digest>` for a Dockerfile's image, `steery-snapshot-<digest>` for a
    /// saved snapshot.
    name: String,
    /// The setup script that builds a Dockerfile's image in it.
    script: Option<String>,
    /// The saved snapshot it is made from.
    from: Option<String>,
}

impl Template {
    fn image(image: image::Image) -> Self {
        Self {
            name: image.name,
            script: Some(image.script),
            from: None,
        }
    }

    fn snapshot(name: &str) -> Self {
        Self {
            name: format!("{SNAPSHOT_PREFIX}{}", &digest(&[name])[..16]),
            script: None,
            from: Some(name.to_owned()),
        }
    }
}

/// A host forked from `template` with `labels`, waited for until it runs. A template whose
/// disk Boat no longer keeps (14 days after it stopped), or that is gone, is made again,
/// once; a fork Boat answers with a host it no longer has is forked again, once, under an
/// idempotency key of its own.
async fn forked(
    boat: &Boat,
    building: &tokio::sync::Mutex<()>,
    template: &Template,
    key: &str,
    labels: &BTreeMap<String, String>,
    deadline: Deadline,
) -> Result<String, Fault> {
    let fork = Fork {
        labels,
        ttl_seconds: TTL_SECONDS,
        size: boat.size(),
        environment: boat.environment(),
        env: boat.env(),
    };
    let (mut remade, mut stale) = (false, String::new());
    loop {
        let source = {
            let _one = building.lock().await;
            templated(boat, template, deadline).await?
        };
        let once = format!("steery-{}{stale}", digest(&[key, &source]));
        let id = match boat.fork(&source, &fork, &once, deadline).await {
            Ok(id) => id,
            Err(Fault::Conflict { code, .. }) if code == "idempotency_in_progress" => {
                deadline
                    .pause(POLL, || "Boat was still taking the fork".to_owned())
                    .await?;
                continue;
            }
            Err(Fault::Conflict { code, .. }) if code == "snapshot_data_expired" && !remade => {
                remade = true;
                let _ = boat.delete(&source, deadline).await;
                continue;
            }
            Err(Fault::NotFound(_)) if !remade => {
                remade = true;
                continue;
            }
            Err(fault) => return Err(fault),
        };
        match boat
            .wait(&id, RUNNING, FAILED, "start", POLL, deadline)
            .await
        {
            Ok(_) => return Ok(id),
            Err(Fault::NotFound(_)) if stale.is_empty() => stale = format!("-{}", now_millis()),
            Err(fault) => return Err(fault),
        }
    }
}

/// The id of `template`'s host, stopped and ready to fork: found by its name, or made now,
/// a Dockerfile's image built in it by its setup script. One that failed, or stopped before
/// its build ended, is deleted and made again, once; a build that failed is deleted and
/// said, its next ensure building again. Once an image is built, all but the newest
/// [`IMAGES_KEPT`] of Steery's images are deleted.
async fn templated(boat: &Boat, template: &Template, deadline: Deadline) -> Result<String, Fault> {
    let mut remade = false;
    'made: loop {
        let id = match kept(boat, &template.name, deadline).await? {
            Some(id) => id,
            None => {
                let labels = BTreeMap::from([(TEMPLATE.0.to_owned(), TEMPLATE.1.to_owned())]);
                let create = Create {
                    name: &template.name,
                    labels: &labels,
                    ttl_seconds: BUILD_TTL_SECONDS,
                    size: boat.size(),
                    from: template.from.as_deref(),
                    environment: boat.environment(),
                    env: &Env::default(),
                    setup_script: template.script.as_deref(),
                };
                let once = format!("{}-{}", template.name, now_millis());
                made(boat, &create, &once, deadline).await?
            }
        };
        loop {
            let host = boat.get(&id, deadline).await?;
            let state = host.state();
            let setup = host.setup_status.as_deref().unwrap_or("pending");
            let builds = template.script.is_some();
            if builds && setup == "failed" {
                let _ = boat.delete(&id, deadline).await;
                return Err(Fault::Failed(format!(
                    "building Steery's image {} failed: {}",
                    template.name,
                    host.setup_error.as_deref().unwrap_or("no reason given")
                )));
            }
            let building = builds && setup != "done";
            let lost = FAILED.contains(&state)
                || (building && !RUNNING.contains(&state) && !MAKING.contains(&state));
            if lost {
                let _ = boat.delete(&id, deadline).await;
                if remade {
                    return Err(Fault::Failed(format!(
                        "{} was {state} before it was ready, twice",
                        template.name
                    )));
                }
                remade = true;
                continue 'made;
            }
            if building || MAKING.contains(&state) || state == "archiving" {
                deadline
                    .pause(BUILD_POLL, || {
                        format!(
                            "{} was still being made ({state}, setup {setup})",
                            template.name
                        )
                    })
                    .await?;
                continue;
            }
            if RUNNING.contains(&state) {
                // Kept stopped, so it costs nothing between forks.
                match boat.act(&id, "stop", json!({}), deadline).await {
                    Ok(()) => {}
                    Err(Fault::Conflict { code, .. }) if code == "sandbox_stopping" => {}
                    Err(fault) => return Err(fault),
                }
                boat.wait(&id, &["archived"], &["error"], "stop", POLL, deadline)
                    .await?;
                if builds {
                    prune(boat, &template.name, deadline).await;
                }
            }
            return Ok(id);
        }
    }
}

/// The oldest host named `name` that Boat did not cancel.
async fn kept(boat: &Boat, name: &str, deadline: Deadline) -> Result<Option<String>, Fault> {
    let mut hosts = boat.list(&BTreeMap::new(), Some(name), deadline).await?;
    hosts.retain(|host| host.name.as_deref() == Some(name) && host.state() != "cancelled");
    hosts.sort_by_key(|host| millis(host.created_at.as_deref()));
    Ok(hosts.into_iter().next().map(|host| host.id))
}

/// Deletes all but the newest [`IMAGES_KEPT`] of Steery's images, `keep` among them. A host
/// forked from one keeps its own disk. Logged, never failed: the build already ended well.
async fn prune(boat: &Boat, keep: &str, deadline: Deadline) {
    let mut images = match boat
        .list(&BTreeMap::new(), Some(image::PREFIX), deadline)
        .await
    {
        Ok(images) => images,
        Err(fault) => {
            eprintln!("boat: Steery's older images were not read: {fault:?}");
            return;
        }
    };
    images.retain(|host| host.name.as_deref() != Some(keep));
    images.sort_by_key(|host| std::cmp::Reverse(millis(host.created_at.as_deref())));
    for old in images.iter().skip(IMAGES_KEPT - 1) {
        if let Err(fault) = boat.delete(&old.id, deadline).await {
            eprintln!(
                "boat: Steery's older image {} was not deleted: {fault:?}",
                old.id
            );
        }
    }
}

/// Refreshes every stopped host's disk before Boat deletes it ([`REFRESH_AFTER`]): the
/// hosts and templates of this Boat account Steery made, every [`REFRESH_EVERY`] from
/// [`REFRESH_FIRST`] after the adapter started. Logged, never failed.
async fn refreshing(boat: Arc<Boat>) {
    tokio::time::sleep(REFRESH_FIRST).await;
    let mut every = tokio::time::interval(REFRESH_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        for label in [MINE, TEMPLATE] {
            let labels = BTreeMap::from([(label.0.to_owned(), label.1.to_owned())]);
            let hosts = match boat
                .list(&labels, None, Deadline::after(CHANGE_SECONDS))
                .await
            {
                Ok(hosts) => hosts,
                Err(fault) => {
                    eprintln!("boat: the stopped hosts to refresh were not read: {fault:?}");
                    continue;
                }
            };
            let now = now_millis();
            for host in hosts.iter().filter(|host| due(host, now)) {
                match refreshed(&boat, &host.id).await {
                    Ok(state) => eprintln!("boat: host {} refreshed: {state}", host.id),
                    Err(fault) => eprintln!("boat: host {} was not refreshed: {fault:?}", host.id),
                }
            }
        }
    }
}

/// Whether `host`'s disk is refreshed at `now` (milliseconds since 1970): stopped for
/// [`REFRESH_AFTER`] or more, and still kept by Boat.
fn due(host: &api::Host, now: u128) -> bool {
    let stopped = millis(host.stopped_at.as_deref());
    host.state() == "archived"
        && host.data_expired_at.is_none()
        && stopped > 0
        && now.saturating_sub(u128::from(stopped)) >= REFRESH_AFTER.as_millis()
}

/// Starts the stopped host `id` and, [`SETTLE`] later, stops it again, which starts Boat's
/// 14 days afresh; one whose `steery host serve` runs by then, started by the runtime in
/// the meantime, is left running for the runtime to stop once idle.
async fn refreshed(boat: &Boat, id: &str) -> Result<&'static str, Fault> {
    let deadline = Deadline::after(LONG_SECONDS);
    boat.act(id, "resume", json!({ "ttlSeconds": TTL_SECONDS }), deadline)
        .await?;
    boat.wait(id, RUNNING, FAILED, "start", POLL, deadline)
        .await?;
    tokio::time::sleep(SETTLE).await;
    let (_, said) = boat
        .run(
            id,
            // `[s]` so the pattern does not find the shell this command runs in.
            "pgrep -f 'steery host [s]erve' >/dev/null && echo used || echo unused",
            20,
            deadline,
        )
        .await?;
    if said.trim() == "used" {
        return Ok("used since it started, left running");
    }
    match boat.act(id, "stop", json!({}), deadline).await {
        Ok(()) => {}
        Err(Fault::Conflict { code, .. }) if code == "sandbox_stopping" => {}
        Err(fault) => return Err(fault),
    }
    boat.wait(id, &["archived"], &["error"], "stop", POLL, deadline)
        .await?;
    Ok("kept 14 days more")
}

/// Resumes a stopped host, and waits until it runs; one running already is answered as
/// it is. One whose disk Boat no longer keeps (14 days after it stopped) is `not-found`:
/// gone for good.
async fn start(boat: &Boat, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(LONG_SECONDS);
    let started = async {
        let mut host = boat.get(id, deadline).await?;
        if host.state() == "archiving" {
            boat.wait(id, &["archived"], FAILED, "stop", POLL, deadline)
                .await?;
            host = boat.get(id, deadline).await?;
        }
        let state = host.state();
        if RUNNING.contains(&state) {
            return Ok(state.to_owned());
        }
        if state == "cancelled" {
            return Err(Fault::NotFound(format!("host {id} was cancelled by Boat")));
        }
        if !MAKING.contains(&state) {
            let resumed = boat
                .act(id, "resume", json!({ "ttlSeconds": TTL_SECONDS }), deadline)
                .await;
            match resumed {
                Ok(()) => {}
                Err(Fault::Conflict { code, line }) if code == "snapshot_data_expired" => {
                    return Err(Fault::NotFound(format!(
                        "host {id}'s disk is gone: Boat keeps a stopped sandbox's disk for 14 days ({line})"
                    )));
                }
                // Resumed by another call already.
                Err(Fault::Conflict { code, .. }) if code == "sandbox_resuming" => {}
                Err(fault) => return Err(fault),
            }
        }
        boat.wait(id, RUNNING, FAILED, "start", POLL, deadline)
            .await
    };
    let state = started
        .await
        .map_err(|fault| refuse(boat, fault, refusal::START_FAILED, refusal::START_FAILED))?;
    Ok(hosts::State { state })
}

/// Stops the host, Boat keeping its disk, and waits until it is stopped (`archived`).
async fn stop(boat: &Boat, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(CHANGE_SECONDS);
    let stopped = async {
        if boat.get(id, deadline).await?.state() == "archived" {
            return Ok("archived".to_owned());
        }
        match boat.act(id, "stop", json!({}), deadline).await {
            Ok(()) => {}
            Err(Fault::Conflict { code, .. }) if code == "sandbox_stopping" => {}
            Err(fault) => return Err(fault),
        }
        boat.wait(id, &["archived"], &["error"], "stop", POLL, deadline)
            .await
    };
    let state = stopped
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    Ok(hosts::State { state })
}

/// Deletes the host and its disk, and waits until Boat no longer has it.
async fn delete(boat: &Boat, params: hosts::Target) -> Result<hosts::State, Error> {
    let id = named(&params.host)?;
    let deadline = Deadline::after(CHANGE_SECONDS);
    let deleted = async {
        boat.delete(id, deadline).await?;
        boat.wait(id, &["deleted"], &[], "delete", POLL, deadline)
            .await
    };
    let state = deleted
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    Ok(hosts::State { state })
}

/// Moves the host's auto-stop [`TTL_SECONDS`] on from now.
async fn keepalive(boat: &Boat, params: hosts::Target) -> Result<wire::Empty, Error> {
    let id = named(&params.host)?;
    boat.extend(id, TTL_SECONDS, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    Ok(wire::Empty {})
}

/// Hosts `port` on a public HTTPS URL, which Boat makes from the host and port and so
/// stays the same while the host lives; any caller reaches it, so the service behind it
/// checks its own key.
async fn expose(boat: &Boat, params: hosts::Expose) -> Result<hosts::Exposed, Error> {
    let id = named(&params.host)?;
    let url = boat
        .host(id, params.port, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    Ok(hosts::Exposed {
        url,
        headers: BTreeMap::new(),
    })
}

/// Writes the file over `ssh`: into a file beside it that becomes it only once every byte
/// arrived, with its mode, so a short write leaves nothing to run.
async fn put(boat: &Boat, keys: &ssh::Keys, params: hosts::Put) -> Result<wire::Empty, Error> {
    let id = named(&params.host)?;
    let path = params
        .path
        .to_str()
        .filter(|path| Path::new(path).is_absolute())
        .ok_or_else(|| Error::invalid("path must be an absolute UTF-8 path"))?
        .to_owned();
    if params.mode > 0o7777 {
        return Err(Error::invalid("mode must be Unix mode bits"));
    }
    let content = base64::engine::general_purpose::STANDARD
        .decode(&params.content)
        .map_err(|e| Error::invalid(format!("content must be base64: {e}")))?;
    let deadline = Deadline::after(LONG_SECONDS);
    let reach = boat
        .ssh(id, &keys.public, deadline)
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    let argv = keys
        .argv(id, &reach, &ssh::writing(&path, content.len(), params.mode))
        .map_err(|why| Error::refused(refusal::REFUSED, format!("Boat: {why}")))?;
    let within = deadline.left().unwrap_or_default();
    ssh::run(&argv, content, within).await.map_err(|why| {
        Error::refused(
            refusal::REFUSED,
            one_line(
                boat.key(),
                &format!("Boat: writing {path} in {id}: {why}; nothing was left"),
            ),
        )
    })?;
    Ok(wire::Empty {})
}

/// An argv channel: `ssh -T` with the adapter's key, which Boat lets into the host first,
/// to the address Boat answers, the host's key pinned as Boat answers it, running
/// `command` in the host.
async fn channel(
    boat: &Boat,
    keys: &ssh::Keys,
    params: hosts::Channel,
) -> Result<hosts::Reach, Error> {
    let id = named(&params.host)?;
    if params.command.is_empty() {
        return Err(Error::invalid(
            "command must be a non-empty array of strings",
        ));
    }
    let reach = boat
        .ssh(id, &keys.public, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(boat, fault, refusal::REFUSED, refusal::REFUSED))?;
    let argv = keys
        .argv(id, &reach, &params.command)
        .map_err(|why| Error::refused(refusal::REFUSED, format!("Boat: {why}")))?;
    Ok(hosts::Reach::Argv {
        argv,
        env: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_boat_times_as_milliseconds() {
        assert_eq!(millis(Some("2024-10-01T12:00:00.000Z")), 1_727_784_000_000);
        assert_eq!(millis(Some("not a time")), 0);
        assert_eq!(millis(None), 0);
    }

    #[test]
    fn keeps_the_key_out_of_a_line() {
        assert_eq!(
            one_line("boat_k", "Boat: bad  key boat_k\nhere"),
            "Boat: bad key [redacted] here"
        );
    }

    /// The refusal word and line `hosts/list` answers against a Boat answering `status`
    /// with `body`.
    async fn listed(status: u16, body: &'static str) -> (String, String) {
        let url = api::tests::boat_answering(Some(status), body).await;
        let boat = api::tests::boat_at(&url);
        match list(&boat, hosts::List { labels: None }).await {
            Err(Error::Refused { word, line }) => (word, line.unwrap_or_default()),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refused_key_is_refused_and_names_the_setting_to_give_again() {
        assert_eq!(
            listed(
                401,
                r#"{"code":"unauthorized","message":"Invalid key boat_secret"}"#
            )
            .await,
            (
                refusal::REFUSED.to_owned(),
                "Boat refused the API key (HTTP 401: Invalid key [redacted]): give BOAT_API_KEY again"
                    .to_owned()
            )
        );
    }

    #[tokio::test]
    async fn boat_down_is_offline() {
        for status in [502, 503, 504] {
            let (word, line) = listed(status, "").await;
            assert_eq!(word, refusal::OFFLINE, "{line}");
        }
    }

    /// An ensure Boat has no room for is `no-place`, which the runtime waits out; the same
    /// fault on a call that makes nothing is `refused`.
    #[tokio::test]
    async fn no_room_for_a_make_is_no_place() {
        let url = api::tests::boat_answering(
            Some(429),
            r#"{"code":"limit_reached","message":"100 sandboxes at once"}"#,
        )
        .await;
        let boat = api::tests::boat_at(&url);
        let building = tokio::sync::Mutex::new(());
        let ensured = ensure(
            &boat,
            &building,
            hosts::Ensure {
                key: "k".into(),
                source: None,
                labels: BTreeMap::new(),
            },
        )
        .await;
        assert!(
            matches!(&ensured, Err(Error::Refused { word, .. }) if word == refusal::NO_PLACE),
            "{ensured:?}"
        );
        let (word, _) = listed(429, r#"{"code":"rate_limited","message":"12 a minute"}"#).await;
        assert_eq!(word, refusal::REFUSED);
    }

    /// An image source is refused: Boat pulls no image.
    #[tokio::test]
    async fn an_image_source_is_refused_naming_what_boat_takes() {
        let url = api::tests::boat_answering(
            Some(200),
            r#"{"ok":true,"sandboxes":[],"pageInfo":{"hasMore":false}}"#,
        )
        .await;
        let boat = api::tests::boat_at(&url);
        let building = tokio::sync::Mutex::new(());
        let ensured = ensure(
            &boat,
            &building,
            hosts::Ensure {
                key: "k".into(),
                source: Some(hosts::Source::Image("ubuntu:24.04".into())),
                labels: BTreeMap::new(),
            },
        )
        .await;
        assert!(
            matches!(&ensured, Err(Error::Invalid(line)) if line.contains("saved snapshot")),
            "{ensured:?}"
        );
    }

    /// Without `ssh` on this computer no channel can be had, so `hosts/channel`,
    /// `hosts/put` and `hosts/list` refuse `refused`, saying what to install, before they
    /// ask Boat anything.
    #[tokio::test]
    async fn without_an_ssh_client_a_channel_is_refused_saying_what_to_install() {
        let without = Adapter {
            ssh: OnceLock::from(false),
            ..Adapter::default()
        };
        let _ = without.boat.set(None);
        let refused = |answer: Result<Value, Error>| match answer {
            Err(Error::Refused { word, line }) => (word, line.unwrap_or_default()),
            other => panic!("{other:?}"),
        };
        for (method, params) in [
            (
                method::HOSTS_CHANNEL,
                json!({"host": "s", "command": ["steery", "serve"]}),
            ),
            (
                method::HOSTS_PUT,
                json!({"host": "s", "path": "/tmp/x", "content": "", "mode": 420}),
            ),
            (method::HOSTS_LIST, json!({})),
        ] {
            assert_eq!(
                refused(without.call(method, params).await),
                (refusal::REFUSED.to_owned(), NO_SSH.to_owned()),
                "{method}"
            );
        }
    }

    /// `initialize` says up front that no host can be had from a computer without `ssh`
    /// (PROTOCOL.md §3.2), so adding the machine refuses before it writes anything; and it
    /// refuses a size Boat does not make.
    #[test]
    fn initialize_says_what_is_missing_and_refuses_a_size_boat_has_not() {
        let params = |settings: Value| -> wire::InitializeParams {
            serde_json::from_value(json!({
                "protocol": [PROTOCOL],
                "runtime": {"release": "7.0.171", "os": "linux", "arch": "x86_64"},
                "home": "/home/p", "config": "/home/p/.config/steery",
                "settings": settings,
            }))
            .unwrap()
        };
        let unavailable = |ssh: bool| {
            let adapter = Adapter {
                ssh: OnceLock::from(ssh),
                ..Adapter::default()
            };
            match adapter.initialize(&params(json!({}))).unwrap().machine {
                Some(wire::Machine::Hosts(facts)) => facts.unavailable,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(unavailable(false).as_deref(), Some(NO_SSH));
        assert_eq!(unavailable(true), None);
        let sized = Adapter::default()
            .initialize(&params(json!({"BOAT_API_KEY": "k", "BOAT_TYPE": "huge"})));
        assert!(
            matches!(&sized, Err(Error::Refused { line: Some(line), .. }) if line.contains("BOAT_TYPE is huge")),
            "{sized:?}"
        );
    }

    #[test]
    fn reads_boat_env_line_by_line_and_never_says_a_value() {
        let env = env_of("# sign-ins\n\nA=1\n  B_2=x=y; z \nEMPTY=\n").unwrap();
        assert_eq!(
            env.0,
            BTreeMap::from([
                ("A".into(), "1".into()),
                ("B_2".into(), "x=y; z ".into()),
                ("EMPTY".into(), String::new()),
            ])
        );
        assert_eq!(format!("{env:?}"), r#"{"A", "B_2", "EMPTY"}"#);
        let why = env_of("A=1\nsecret-value\n").unwrap_err();
        assert!(why.contains("line 2") && !why.contains("secret"), "{why}");
        let why = env_of("A=1\n\n9X=secret\n").unwrap_err();
        assert!(why.contains("line 3") && !why.contains("secret"), "{why}");
    }

    /// The create body carries what the person named and nothing else.
    #[test]
    fn the_create_body_carries_only_what_was_named() {
        let labels = BTreeMap::from([("steery".to_owned(), "host".to_owned())]);
        let body = |env: &Env, size: Option<&str>, from: Option<&str>| {
            serde_json::to_value(Create {
                name: "n",
                labels: &labels,
                ttl_seconds: TTL_SECONDS,
                size,
                from,
                environment: None,
                env,
                setup_script: None,
            })
            .unwrap()
        };
        assert_eq!(
            body(&Env::default(), None, None),
            json!({"name": "n", "labels": {"steery": "host"}, "ttlSeconds": 1800})
        );
        let env = env_of("K=v").unwrap();
        let named = body(&env, Some("large"), Some("steery-image-ab"));
        assert_eq!(named["env"], json!({"K": "v"}));
        assert_eq!(named["type"], "large");
        assert_eq!(named["from"], "steery-image-ab");
    }

    /// A host is refreshed once stopped 10 days, while Boat still keeps its disk; never
    /// while it runs, nor once its disk is gone.
    #[test]
    fn a_stopped_host_is_refreshed_from_ten_days_while_its_disk_is_kept() {
        let host = |state: &str, stopped: Option<&str>, expired: Option<&str>| -> api::Host {
            serde_json::from_value(json!({
                "id": "bx_1", "state": state, "stoppedAt": stopped, "dataExpiredAt": expired
            }))
            .unwrap()
        };
        let stopped = "2026-10-01T12:00:00.000Z";
        let at = u128::from(millis(Some(stopped)));
        let day = 24 * 60 * 60 * 1000;
        let archived = host("archived", Some(stopped), None);
        assert!(!due(&archived, at + 10 * day - 1));
        assert!(due(&archived, at + 10 * day));
        assert!(due(&archived, at + 13 * day));
        assert!(!due(&host("idle", Some(stopped), None), at + 13 * day));
        assert!(!due(
            &host("archived", Some(stopped), Some("2026-10-15T12:00:00Z")),
            at + 15 * day
        ));
        assert!(!due(&host("archived", None, None), at + 13 * day));
    }
}
