# Steery adapters

An adapter connects [Steery](https://steery.dev) to one kind of machine: an operating system
(`macos`, `linux`) or a vendor's service that makes hosts (`daytona`, `opensandbox`): a
machine whose adapter makes hosts runs each conversation in a host of its own. `joined`
makes none: its hosts are made by their owner's own system and join the machine through
Steery's server. The Steery runtime
starts an adapter as a child process and speaks the program protocol to it, JSON-RPC 2.0
over standard input and output (`PROTOCOL.md` in
[steery-dev/sdk](https://github.com/steery-dev/sdk)). Drivers, one
per agent harness, live in `steery-dev/drivers`.

Each adapter is its own program, `steery-adapter-<id>`, built on the `steery-sdk` crate,
and answers version 3 of the program protocol. Steery adds the adapters below the way it
adds any program: from this repository's releases, each a prebuilt executable per target
(`steery programs add https://github.com/steery-dev/adapters`, which Steery's installer
runs). An operating-system adapter is built only for that system's targets, and the runtime
runs the one whose `initialize` names the system it runs on (`machine.os`). Everything a
person sees of an adapter, its label, its short name, its tint and its mark, is what its
`initialize` says.

```
macos/       steery-adapter-macos: processes on macOS
linux/       steery-adapter-linux: processes on Linux
daytona/     steery-adapter-daytona: hosts on Daytona
opensandbox/ steery-adapter-opensandbox: hosts on OpenSandbox
joined/      steery-adapter-joined: hosts that join their machine through Steery's server
```

Each crate is a Cargo workspace of its own, with its own `Cargo.lock`, so it builds alone.
What `linux` and `macos` share, where POSIX behaves the same on both, is the
[`common`](common/) crate, `steery-adapter-common`, each naming it by path (`../common`):
`process/start`'s detached start with its `STEERY_STARTED`, status file and `held`
standard input (an unlinked FIFO the process alone holds, so it never ends), the output
files it appends to, and a user's name. What each system keeps — `/proc` or libproc,
systemd or launchd — stays in its own adapter.

## Building

The SDK and the protocol live in [steery-dev/sdk](https://github.com/steery-dev/sdk).
Every crate names the SDK by git, at a tag,
`steery-sdk = { git = "https://github.com/steery-dev/sdk", tag = "v3.0.0" }`, and pins its
toolchain in `rust-toolchain.toml`:

```sh
git clone https://github.com/steery-dev/adapters
cd adapters/linux && cargo build
```

Every build goes through [Kache](https://github.com/kunobi-ninja/kache), the compiler cache
`.cargo/config.toml` names. Install it once per machine:
`brew install kunobi-ninja/kunobi/kache && kache daemon install` on a Mac, its
[APT package](https://kunobi.ninja/docs/kache/getting-started/installation) on Linux.

`./release.sh <tag>` builds every adapter with `--release --locked` with `cargo zigbuild`,
into `dist/<target>/`: `daytona`, `opensandbox` and `joined` for each target Steery runs on,
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu` (the Linux builds linking glibc 2.30 at most); `macos` for the
two Apple targets only, and `linux` for the two Linux targets only. It then packs them into
`dist/release/` with `steery programs pack --release <tag> --out dist/release
<target>=dist/<target>…`: one archive per adapter and target and the manifest,
`steery-programs.json` (PROTOCOL.md §7.1). With `--publish` it makes the GitHub release
`<tag>` of those files (`gh release create`); with `--no-pack` it stops after building. It
needs `cargo-zigbuild`, `zig`, the four targets (`rustup target add`), and `steery` on
`PATH` or named by `STEERY`.

## Checking an adapter

```sh
steery programs test target/debug/steery-adapter-linux
```

`steery programs test [--setting NAME[=VALUE]]… <command> [args…]` starts the adapter and
checks what it says it is, the methods that read and change nothing, an id sent twice, a
method it does not have, and its shutdown, one line per check; it exits 0 only when every
one passes. A read answered with a refusal word its method lists is an answer.
`--setting NAME=VALUE` passes a setting to `initialize`, and `--setting NAME` takes its
value from the environment, keeping a secret off the command line; a method that needs a
required setting not given is skipped. `daytona`, `opensandbox` and `joined` also have
their own `cargo test`.

## Writing an adapter

An adapter for a machine Steery does not ship is a program like these. `linux/` is the
worked example: one file, `src/main.rs`.

- **Its crate** depends on `steery-sdk` by git as above, with `tokio` for its runtime and
  `serde_json` for the parameters, and declares an empty `[workspace]` so it builds alone.
  Its binary is named `steery-adapter-<id>`; `main` is
  `steery_sdk::serve(|_notifier| Linux::default())`. The SDK reads requests, answers each
  one, and gives the notifier for what the adapter says unasked (a `stream` channel's
  `stream/data` and `stream/closed`).
- **`initialize`** (`Program::initialize`) refuses a runtime that does not offer
  `steery_sdk::PROTOCOL`, keeps the `home` it is given, and answers `wire::Initialized`:
  `program` (`id: "linux"`, `kind: Machine`, `release`, `label: "Linux"`, `short: "Lx"`,
  and optionally `tint` and `mark`), no `harness`,
  `machine` (`wire::Machine::Processes` with `os: Linux` and `service: Systemd`; an adapter
  that makes hosts answers `Hosts` with what it keeps, how it idles, its optional methods
  and its channel; PROTOCOL.md §3.2), and the settings it needs (none here; `daytona` and
  `opensandbox` declare theirs, §3.4).
- **The methods** (`Program::call`) are one match from each `steery_sdk::method` name to the
  function that answers it, its parameters read with `steery_sdk::params` and its answer
  given with `steery_sdk::result`: `process/*` and `service/*` for `processes` (§5.1),
  `hosts/*` for `hosts` (§5.2), `stream/*` for a `stream`
  channel (§5.3); any other answers `Error::Unknown`. A refusal is `Error::refused` with
  one of the protocol's words (`steery_sdk::refusal`).

## Adding an adapter

An adapter is added as every program is, Steery's own included (PROTOCOL.md §7.2):

```sh
steery programs add https://github.com/steery-dev/adapters
steery programs add https://github.com/me/my-cloud@v0.1.0 --setting MY_CLOUD_TOKEN
```

`<source>` is a GitHub repository (its latest release, followed by `steery programs
update`, or `@<tag>` for that one), a release's manifest, `steery-programs.json`, by URL or
path, one program's archive, or a built `steery-adapter-<id>` itself. Publish your own
adapter the way this repository does: build it for each target it runs on, pack it with
`steery programs pack`, and make the release of those files. Nothing is built on the
computer it is added to.

Each program's build for this computer is fetched, checked against its sha256, copied into
Steery's programs folder, started and checked as `steery programs test` checks it, with the
settings given (`--setting NAME=VALUE`, or `--setting NAME` from the environment; a secret
one not given is typed at the terminal, unechoed). Steery prints what it will add and asks
once at the terminal; `--yes` is that answer anywhere else. Every build the release has for
another target is kept beside the record, so a host gets the Linux build it needs.

`steery programs remove <id>` removes it.

## linux

`linux/` provides `processes` on Linux (`initialize` answers
`machine: {provides: "processes", os: "linux", service: "systemd"}`).

- `process/start` starts a turn detached, in a session of its own, with exactly the
  environment it is given. With a `scope` naming the service the runtime runs as under
  systemd (the runtime's cgroup is `<label>.service`), it runs in a transient scope in
  `<label>.slice` (`systemd-run --user --scope --slice=<label>.slice`), under the slice's
  CPU quota and outside the runtime's unit; anywhere else it runs as it is.
- `process/list` reads `/proc`; a zombie is not listed, and `args` is absent where `/proc`
  hides a process's command line. `process/environ` and `process/files` read `/proc`, and
  `process/signal` signals only a process whose start time still matches.
- `process/listening` finds the `LISTEN` sockets on the port in `/proc/net/tcp` and `tcp6`
  and the processes holding them under `/proc/<pid>/fd`.
- `service/plan` writes three systemd user units, as the runtime installs itself:
  `<label>.service` (the binary, `Restart=always`, `KillMode=process`, output appended to
  `log`, the `PATH` asked for merged with the one the unit on disk gave), `<label>.socket`
  (`ListenStream=127.0.0.1:<port>`, handed to the runtime) and `<label>.slice`
  (`CPUQuota` of `cpu` percent of what the computer grants; `MemoryMax` of `cpu` percent
  of physical memory, `MemoryHigh` nine tenths of it). Its commands reload systemd,
  enable the socket and the service, bind the socket again only where its unit changed,
  and `loginctl enable-linger`, which may fail.
- `service/status` answers whether the unit is on disk and systemd runs it, its pid, and
  `granted`: online cores, narrowed by the CPU affinity and every cgroup v1 or v2 quota
  above the runtime's own slice, in percent of one core.
- `service/start` starts the service; `service/restart` stops it, moves what it left in
  its cgroup (turns, kept by `KillMode=process`) into a scope of their own in the slice,
  and starts it; `service/remove` disables and removes the service, then the socket, then
  the slice's file, never stopping the slice or a turn. Each refuses `refused` with
  systemd's line.

## macos

`macos/` provides `processes` on macOS (`initialize` answers
`machine: {provides: "processes", os: "macos", service: "launchd"}`). It is released for
the Apple targets only. Built for another system it answers `initialize` and refuses every
method.

- `process/start` starts a turn detached, in a session of its own, with exactly the
  environment it is given. launchd has no share of the CPU to run it in, so `scope` is
  ignored.
- `process/list` reads processes through libproc and their arguments and environment
  through `sysctl` (`KERN_PROCARGS2`). A zombie is not listed. One of this user's whose
  arguments cannot be read is listed without `args`; another user's is left out unless
  the adapter runs as root. `process/files` reads `proc_pidfdinfo`, and `process/signal`
  signals only a process whose start time still matches.
- `process/listening` asks `lsof -iTCP:<port> -sTCP:LISTEN`.
- `service/plan` writes the LaunchAgent `~/Library/LaunchAgents/<label>.plist`, as the
  runtime installs itself: `KeepAlive`, a lower priority, output appended to `log`, the
  `PATH` asked for merged with the one the plist on disk gave, and a `Listeners` socket on
  127.0.0.1:`port` handed to the runtime. Its commands are `launchctl bootout` (which may
  fail), `enable` and `bootstrap gui/<uid>`.
- `service/status` answers whether the plist is on disk and launchd runs the job, its pid,
  and `granted`: every online core, in percent of one core.
- `service/start` and `service/restart` `launchctl kickstart` the job (`-k` to restart),
  or load it from its plist where it is not loaded; launchd stops only the job's own
  process group, so turns keep running. `service/remove` removes the plist, then boots
  the job out. Each refuses `refused` with launchd's line.

## daytona

`daytona/` provides `hosts` on [Daytona](https://daytona.io) (`initialize` answers
`machine: {provides: "hosts", keeps: "filesystem", idle: "activity", optional:
["archive", "keepalive", "expose"], builds: true, channel: "argv", os: "linux"}`). It calls Daytona's published
REST API directly, over HTTPS (rustls): the control plane ([`openapi.json`](https://www.daytona.io/docs/openapi.json)) and each host's
toolbox ([`toolbox-openapi.json`](https://www.daytona.io/docs/toolbox-openapi.json)),
through the toolbox proxy Daytona names for it. Daytona's own words for a host (its API
paths) stay inside `src/api.rs`, whose items are named by Steery's words (`api::Host` is a host as
Daytona answers it). It passes no image, environment, secret or network setting of its own,
and leaves Daytona's auto-stop as the host's idle timer.

- `hosts/list`: the hosts labelled `steery=host` and every label asked, page by page.
  Daytona's listing lags a change by a second or more.
- `hosts/ensure`: the host named `steery-<first 128 bits of the key's SHA-256, hex>`, or
  one made under that name, labelled `steery=host`, `steery-key=<key>` and the labels
  asked, from the snapshot, the image (built `FROM` it) or the Dockerfile (Daytona's
  `buildInfo.dockerfileContent`, built in your account once per text) the runtime names, or
  Daytona's default snapshot, with the environment `DAYTONA_ENV` names (Daytona's `env`,
  sent only when given), and waited for until it runs, through `pending_build` and
  `building_snapshot`. Daytona keeps names unique and reads them back at once, so an
  ensure sent twice, even at once, makes one host. A host of that name without those
  labels is refused; one being deleted is waited out; one still building is waited for.
- `hosts/start`, `stop`, `delete` and `archive` ask Daytona and wait until the host is
  `started`, `stopped`, `destroyed` or `archived`, reading it again every second (every
  two while archiving); `error` or `build_failed` fails. Archive stops a started host
  first, since Daytona archives only a stopped one. Its filesystem survives a stop and an
  archive.
- `hosts/keepalive` refreshes the host's last activity, which auto-stop counts from.
- `hosts/expose` makes the host public (`POST /sandbox/{id}/public/true`) and answers the
  port's preview URL (`https://<port>-<id>.<Daytona's proxy>`), with no headers: any caller
  reaches it, so the service behind it checks its own key.
- `hosts/put` uploads the file (`/files/upload-v2`), reads its size back
  (`/files/info`) and sets its mode (`/files/permissions`). A file that timed out or came
  out short is deleted and the put refused; a large file is better fetched from inside.
- `hosts/channel` is an `argv` channel: `ssh -T` to Daytona's SSH gateway
  (`ssh.app.daytona.io`, or the host and port Daytona's answer names) with a 60-minute
  access token, running the runtime's command, quoted for the host's shell.

The channel runs `ssh` on the controlling computer, which carries the command's standard
input and output byte for byte, so an OpenSSH client there is a prerequisite (`apt install
openssh-client` on a bare Debian or Ubuntu). Where none is on `PATH`, `initialize` says so
as `unavailable`, so no machine is added where it cannot work, and `hosts/channel` and
`hosts/list` — the first call adding a machine makes, so nothing is written — refuse
`refused`: "Daytona's channel runs `ssh` on this computer, and none is installed: install an
OpenSSH client (e.g. apt install openssh-client)".

It waits for Daytona inside the runtime's bounds: 280 s for `ensure`, `start`, `archive`
and `put` (bound 300 s), 55 s for the rest (bound 60 s), and 5 s for each file call after
an upload. Errors read as `not-found` (HTTP 404); `offline` where Daytona is down — not
reached, a gateway answering for it (HTTP 502, 503, 504), or a request with no answer in
its time — a retry the server bounds; `refused` for a refused API key (HTTP 401, 403),
whose line names the setting to give again (`Daytona refused the API key (HTTP 401: …):
give DAYTONA_API_KEY again`), since a bad key does not heal by waiting; `start-failed` (a
host that could not be made or started); or `refused`, which is also a host that did not
reach the state asked for in time, and a put whose upload got no answer by its bound. Each
line is Daytona's own message, without the API key.

Its settings, given once where the machine is added (on Steery's web for a cloud machine,
or `steery setup --adapter daytona --setting NAME[=VALUE]` on a computer):

| Setting | | |
|---|---|---|
| `DAYTONA_API_KEY` | secret, required | an API key from <https://app.daytona.io/dashboard/keys> |
| `DAYTONA_API_URL` | optional | your Daytona API URL, for a self-hosted Daytona; absent: `https://app.daytona.io/api` |
| `DAYTONA_TARGET` | optional | the region hosts are made in; absent: your organization's default |
| `DAYTONA_ENV` | secret, optional | environment set on every host it makes, one `NAME=VALUE` per line (blank and `#` lines skipped; the value is everything after the first `=`), passed to Daytona as it is; a malformed line refuses `initialize` naming its line number, never its value |

It reads them from `initialize` only, never from its environment or a file. Build and
check it:

```sh
cd daytona && cargo build && cargo clippy && cargo test
DAYTONA_API_KEY=… steery programs test target/debug/steery-adapter-daytona --setting DAYTONA_API_KEY
```

With the key, the check also lists your hosts (`hosts/list`); it makes none.

## opensandbox

`opensandbox/` provides `hosts` on [OpenSandbox](https://github.com/opensandbox-group/OpenSandbox)
(`initialize` answers `machine: {provides: "hosts", keeps: "filesystem", idle: "ttl",
optional: ["pause", "keepalive"], channel: "stream", os: "linux"}`). It is a Rust binary,
`steery-adapter-opensandbox`, speaking the program protocol through `steery-sdk`, and talks
to OpenSandbox's published lifecycle API and to the execd daemon in each host over HTTP and
WebSocket (rustls), with no vendor SDK. OpenSandbox's own words for a host (its API paths,
its key header) stay inside `src/provider.rs`, whose items are named by Steery's words
(`provider::Found` is a host as the API answers it):

- `hosts/list`: the hosts whose metadata holds `steery=host` and every label asked.
- `hosts/ensure`: finds the host whose metadata holds `steery-key=<key>`, or makes one
  from the image (`image.uri`) or snapshot (`snapshotId`) the runtime names. OpenSandbox
  has no default image, so with neither it refuses `no-place`. A host made from an image
  is given the entrypoint `tail -f /dev/null`, because the API requires one; its resource
  limits are left empty, so the server's own apply. Metadata keys and values follow
  Kubernetes label rules (63 characters; letters, digits, `-`, `_`, `.`), so a key or label
  outside them is refused by the server. OpenSandbox takes no name to make a host by and
  has nothing to lock on, so ensures for one key run one after the other in the adapter,
  and after making one it lists the key again: the oldest host, by the creation time the
  listing reads, is the key's, and a younger one it made is deleted. Two adapters making one
  at once thus answer one host, as long as the server lists each before the other's
  create answers (a create still pulling its image is not listed).
- `hosts/start` resumes a paused host and waits until it runs; `stop` and `pause`
  pause it (OpenSandbox's only stop that keeps it); `delete` deletes it. Its filesystem
  survives a pause.
- `hosts/keepalive` renews the expiration by the TTL the server gave it when it was made
  (kept in its metadata as `steery-ttl`), or by an hour; a host with no expiration needs
  none. The adapter sets no timeout of its own, so the server's default is the host's
  idle timer; on the Docker runtime that default is none, and a host lives until it is
  deleted.
- `hosts/put` uploads the file through execd (`/files/upload`) with its mode, then reads
  its size back (`/files/info`); a file that is not whole is removed before it refuses.
- `hosts/channel` is a `stream` channel: it opens a pipe-mode PTY session in execd running
  the command (`POST /pty`) and joins its WebSocket (`/pty/{id}/ws?pty=0`), reached through
  the host's endpoint for port 44772 and the headers that endpoint names.
  `stream/write` sends standard-input frames; standard output comes back as `stream/data`,
  standard error goes to the adapter's log, and `stream/closed` carries the exit code (none
  for a command a signal ended).
  execd has no frame that closes only standard input, so `stream/close` sends the command
  SIGHUP, ends the session after two seconds if it still runs, and closes the socket.

Errors read as `not-found` (HTTP 404); `offline` where OpenSandbox is down — not reached,
a gateway answering for it (HTTP 502, 503, 504), or no answer in time; `refused` for a
refused API key (HTTP 401, 403), whose line names `OPENSANDBOX_API_KEY` to give (again);
`start-failed` (a host that could not be made or resumed); or `refused`.

Its settings, given once where the machine is added (on Steery's web for a cloud machine,
or `steery setup --adapter opensandbox --setting NAME[=VALUE]` on a computer):

| Setting | | |
|---|---|---|
| `OPENSANDBOX_URL` | required | your OpenSandbox server, as its SDKs take it: `http://localhost:8080` |
| `OPENSANDBOX_API_KEY` | secret, optional | the server's `server.api_key`, where it has one |

It reads them from `initialize` only, never from its environment. Build and check it:

```sh
cd opensandbox && cargo build && cargo clippy && cargo test
steery programs test target/debug/steery-adapter-opensandbox \
  --setting OPENSANDBOX_URL=http://localhost:8080 --setting OPENSANDBOX_API_KEY
```

The check lists hosts on the server it is given; a local server is the
`opensandbox/server` image (`opensandbox/server:release-1.1.0`, which runs hosts with
`opensandbox/execd:v1.1.0`) with the Docker runtime, as its
`server/docker-compose.example.yaml` sets it up. On a Mac, set the server's `host_ip` to
`127.0.0.1` rather than the example's `host.docker.internal`: the endpoints the server
hands out must be reachable from the Mac itself, where the adapter runs, and
`host.docker.internal` resolves only inside containers.

## joined

`joined/` provides `hosts` for a machine whose hosts join it (`initialize` answers
`machine: {provides: "hosts", keeps: "filesystem", idle: "none", optional: ["keepalive"],
channel: "stream", os: "linux"}`, label *Your own hosts*). Each host is made, started and
stopped by its owner's own system, runs `steery host join` with the machine's join token,
and keeps a socket open to Steery's server (`specs/JOINED-HOSTS.md` in steery-dev/server).
The adapter reaches those hosts only through that server, as the machine:

- `hosts/list`: `GET /v1/join/hosts`, each host that ever joined, by the name it joined
  with: `state` `started` while its socket is open, else `stopped`; `created_at` when it
  first joined; its one label `name`. A filter on any other label, the runtime's
  `steery-machine` among them, lists none without asking the server, so removing the
  machine deletes nothing.
- `hosts/channel` is a `stream` channel: a WebSocket to `/v1/join/control?host=<name>`,
  which the server splices to the host's socket; the host connects it to its `serve`.
  Binary frames are the bytes each way; the socket closing, from either end, is
  `stream/closed`, with no exit code. A host whose socket is not open is refused `offline`
  with the server's line ("<name> is offline: its system starts it").
- `hosts/keepalive` answers `{}`: the host's own system decides when it stops.
- `hosts/ensure`, `start`, `stop`, `delete` and the rest are methods it does not have
  (`unknown method`): the runtime then adopts hosts from `hosts/list` alone and makes,
  starts and stops none.

It declares no setting: Steery's server hands it `STEERY_URL` and `STEERY_KEY` (the
machine's own key) where it runs the machine. Errors read as `not-found` (HTTP 404),
`offline` (not reached, HTTP 502–504, no answer in time, or the host not joined), or
`refused`, each line the server's own message without the key. Build and check it:

```sh
cd joined && cargo build && cargo clippy && cargo test
steery programs test target/debug/steery-adapter-joined
```

## What Steery adds to a host

A machine whose adapter makes hosts (a cloud machine added on Steery's web, or a computer
set up with `steery setup --adapter daytona`) adds to each host it uses:

- the `steery` binary, which `steery host serve` runs inside, and the Linux build of each
  program added on the computer, from its release, for the host's target;
- the harness's MCP entry, skill and hooks, as on a laptop.

Where you name no source and the adapter builds (`daytona`), Steery makes every host from
its own Dockerfile, built by Daytona in your account once per Steery release: Daytona's
sandbox image with Claude Code, omp and `cli-proxy-api`. One more host, the machine's
keeper, runs `cli-proxy-api` holding your Claude accounts on its own disk, reached by
`hosts/expose`; every other host's Claude Code and omp reach it by `ANTHROPIC_BASE_URL`,
`ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`, unless the host sets them itself
(`DAYTONA_ENV` wins). You sign accounts in through Steery request cards; tokens never leave
your Daytona account.

It adds nothing else. With your own source the host is yours: your snapshot, your Daytona
settings, your harness, set up the way you already set them up.

A new machine makes one host of its own as soon as it runs, so it learns which harnesses
your image has and offers them. That host is stopped when idle like any other, and the
first conversation started there takes it. Each later conversation gets a host of its own.

Where `steery` is missing in a host, Steery installs it over the channel the first time.
To have it there already, add the one line to your image. In a Dockerfile:

```dockerfile
RUN curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin
```

In Daytona's declarative builder:

```ts
.runCommands('curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin')
```

## License

MIT
