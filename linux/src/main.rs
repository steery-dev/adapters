//! Steery's `linux` adapter. It provides `processes`: it starts, finds, inspects
//! and signals processes on the computer it runs on, and keeps the runtime running there
//! as a systemd user service. It reads processes from `/proc` and asks systemd through
//! `systemctl --user`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use serde_json::Value;
use steery_adapter_common::user_name;
use steery_sdk::wire::{self, process};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal};
use tokio::time::Instant;

/// The seconds the runtime takes to stop once asked: its own shutdown grace, which its
/// unit gives systemd before it kills it.
const STOP_GRACE_SECS: u64 = 40;

/// The longest `service/status` waits on systemd, inside the runtime's 10 s bound.
const STATUS_BOUND: Duration = Duration::from_secs(8);

/// The longest `service/start`, `restart` and `remove` wait on systemd, inside the
/// runtime's 30 s bound.
const SERVICE_BOUND: Duration = Duration::from_secs(28);

fn main() -> io::Result<()> {
    steery_adapter_common::serve(|_notifier| Linux::default())
}

/// The adapter. It keeps only the person's home, given in `initialize`.
#[derive(Default)]
struct Linux {
    home: OnceLock<PathBuf>,
}

impl Program for Linux {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "linux".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Linux".into(),
                short: "Lx".into(),
                tint: None,
                mark: None,
            },
            harness: None,
            machine: Some(wire::Machine::Processes(wire::ProcessFacts {
                os: wire::Os::Linux,
                service: wire::ServiceManager::Systemd,
            })),
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_adapter_common::blocking;
        use steery_sdk::{params as given, result};
        match method {
            method::PROCESS_START => result(start(given(params)?)?),
            method::PROCESS_LIST => {
                let asked = given(params)?;
                result(blocking(move || Ok(list(asked))).await?)
            }
            method::PROCESS_ENVIRON => {
                let asked = given(params)?;
                result(blocking(move || environ(asked)).await?)
            }
            method::PROCESS_FILES => {
                let asked = given(params)?;
                result(blocking(move || files(asked)).await?)
            }
            method::PROCESS_LISTENING => {
                let asked = given(params)?;
                result(blocking(move || listening(asked)).await?)
            }
            method::PROCESS_SIGNAL => {
                let asked = given(params)?;
                result(blocking(move || Ok(signal(asked))).await?)
            }
            method::SERVICE_PLAN => result(self.service_plan(given(params)?)?),
            method::SERVICE_STATUS => result(self.service_status(given(params)?).await?),
            method::SERVICE_START => result(service_start(given(params)?).await?),
            method::SERVICE_RESTART => result(service_restart(given(params)?).await?),
            method::SERVICE_REMOVE => result(self.service_remove(given(params)?).await?),
            _ => Err(Error::Unknown),
        }
    }
}

/// Starts `argv` detached (`steery_adapter_common::start`). With a `scope` naming the
/// service the runtime runs as under systemd, it runs in a transient scope in that
/// service's slice, sharing its CPU quota and never its unit; `systemd-run --scope` execs
/// the command in place, so the pid is the command's own. Anywhere else it runs as it is.
fn start(mut start: process::Start) -> Result<process::Started, Error> {
    if let Some(scope) = start.scope.as_deref() {
        let label = unit_label(scope)?;
        if supervised(label) {
            start.argv = scoped(label, std::mem::take(&mut start.argv));
        }
    }
    steery_adapter_common::start(start, |pid| stat(pid).map(|stat| stat.start))
}

/// `argv` in a transient scope of its own in `label`'s slice.
fn scoped(label: &str, argv: Vec<String>) -> Vec<String> {
    let mut scoped = vec![
        "systemd-run".to_owned(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        format!("--slice={label}.slice"),
        "--".into(),
    ];
    scoped.extend(argv);
    scoped
}

/// Whether the runtime that started this adapter is the one systemd runs as `label`'s
/// service: its cgroup is that unit's.
fn supervised(label: &str) -> bool {
    // SAFETY: getppid takes nothing and cannot fail.
    let runtime = unsafe { libc::getppid() };
    let unit = format!("/{label}.service");
    runtime > 1
        && fs::read_to_string(format!("/proc/{runtime}/cgroup"))
            .is_ok_and(|groups| groups.lines().any(|line| line.ends_with(&unit)))
}

/// Every pid `/proc` lists.
fn all_pids() -> Vec<u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect()
}

/// Every process running, or those of `pids` that run. A zombie, or a process that ends
/// while it is read, is left out.
fn list(list: process::List) -> process::Listed {
    let pids = list.pids.unwrap_or_else(all_pids);
    let mut users = BTreeMap::new();
    let processes = pids
        .into_iter()
        .filter_map(|pid| entry(pid, &mut users))
        .collect();
    process::Listed { processes }
}

fn entry(pid: u32, users: &mut BTreeMap<u32, String>) -> Option<process::Entry> {
    let stat = stat(pid).filter(Stat::running)?;
    let command = command(pid)?;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // Unreadable where `/proc` hides another user's processes (`hidepid`); a kernel
    // thread's is readable and empty.
    let args = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|cmdline| {
            let cmdline = cmdline.strip_suffix(&[0]).unwrap_or(&cmdline);
            if cmdline.is_empty() {
                return Vec::new();
            }
            cmdline
                .split(|byte| *byte == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect()
        });
    let user = users.entry(uid).or_insert_with(|| user_name(uid)).clone();
    Some(process::Entry {
        pid,
        ppid: stat.ppid,
        pgid: stat.pgid,
        process_start: stat.start,
        command,
        args,
        user,
        stopped: stat.stopped(),
        // Unreadable for another user's process unless this runs as root.
        cwd: fs::read_link(format!("/proc/{pid}/cwd")).ok(),
    })
}

/// A process's executable name, from `/proc/<pid>/comm`.
fn command(pid: u32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_owned())
}

/// The asked variables of a process, refused `not-found` unless the pid still has the
/// given start time.
fn environ(environ: process::Environ) -> Result<process::Values, Error> {
    same(environ.pid, environ.process_start)?;
    let bytes = fs::read(format!("/proc/{}/environ", environ.pid))
        .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("its environment: {e}")))?;
    let names: BTreeSet<&str> = environ.names.iter().map(String::as_str).collect();
    let mut values = BTreeMap::new();
    for pair in bytes.split(|byte| *byte == 0) {
        let pair = String::from_utf8_lossy(pair);
        if let Some((name, value)) = pair.split_once('=')
            && names.contains(name)
            && !values.contains_key(name)
        {
            values.insert(name.to_owned(), value.to_owned());
        }
    }
    same(environ.pid, environ.process_start)?;
    Ok(process::Values { values })
}

/// The files a process holds open: each descriptor that names an absolute path, only
/// those open for writing where `writing`. A process forked and not exec'd since holds
/// its parent's copies, so it answers none.
fn files(files: process::Files) -> Result<process::Paths, Error> {
    let stat = stat(files.pid).filter(Stat::running);
    same(files.pid, files.process_start)?;
    if stat.is_some_and(|stat| stat.unexeced()) {
        return Ok(process::Paths { paths: Vec::new() });
    }
    let fds = fs::read_dir(format!("/proc/{}/fd", files.pid))
        .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("its descriptors: {e}")))?;
    let paths: BTreeSet<PathBuf> = fds
        .flatten()
        .filter(|fd| !files.writing || writes(files.pid, &fd.file_name()))
        .filter_map(|fd| fs::read_link(fd.path()).ok())
        .filter(|path| path.is_absolute())
        .collect();
    same(files.pid, files.process_start)?;
    Ok(process::Paths {
        paths: paths.into_iter().collect(),
    })
}

/// Whether descriptor `fd` of `pid` is open for writing: its `flags` in
/// `/proc/<pid>/fdinfo/<fd>` (octal) have an access mode other than `O_RDONLY`.
fn writes(pid: u32, fd: &std::ffi::OsStr) -> bool {
    let Ok(info) = fs::read_to_string(Path::new(&format!("/proc/{pid}/fdinfo")).join(fd)) else {
        return false;
    };
    info.lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
        .is_some_and(|flags| flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32)
}

/// The processes listening on a TCP port: the `LISTEN` sockets of `/proc/net/tcp` and
/// `tcp6` on that port, and the processes holding a descriptor to one. Another user's
/// descriptors are unreadable unless this runs as root, so their processes are left out.
fn listening(listening: process::Listening) -> Result<process::Listeners, Error> {
    let mut tables = 0;
    let mut sockets = BTreeSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        tables += 1;
        sockets.extend(listening_sockets(&text, listening.port));
    }
    if tables == 0 {
        return Err(Error::refused(
            refusal::REFUSED,
            "/proc/net/tcp and /proc/net/tcp6 are unreadable",
        ));
    }
    let mut processes = Vec::new();
    if sockets.is_empty() {
        return Ok(process::Listeners { processes });
    }
    for pid in all_pids() {
        if holds_one(pid, &sockets)
            && let Some(stat) = stat(pid).filter(Stat::running)
            && let Some(command) = command(pid)
        {
            processes.push(process::Listener {
                pid,
                process_start: stat.start,
                command,
            });
        }
    }
    Ok(process::Listeners { processes })
}

/// The inodes of the sockets a `/proc/net/tcp` table lists in state `LISTEN` (`0A`) on
/// `port`. Each row after the header reads `sl local remote st … inode`, the local address
/// as `<hex address>:<hex port>`.
fn listening_sockets(table: &str, port: u16) -> impl Iterator<Item = u64> + '_ {
    table.lines().skip(1).filter_map(move |row| {
        let mut fields = row.split_whitespace();
        let local = fields.nth(1)?;
        let state = fields.nth(1)?;
        let inode: u64 = fields.nth(5)?.parse().ok()?;
        let at = u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()?;
        (at == port && state == "0A" && inode != 0).then_some(inode)
    })
}

/// Whether `pid` holds a descriptor to one of `sockets`.
fn holds_one(pid: u32, sockets: &BTreeSet<u64>) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten().any(|fd| {
        fs::read_link(fd.path()).is_ok_and(|target| {
            target
                .to_str()
                .and_then(|target| target.strip_prefix("socket:["))
                .and_then(|target| target.strip_suffix(']'))
                .and_then(|inode| inode.parse().ok())
                .is_some_and(|inode| sockets.contains(&inode))
        })
    })
}

/// Sends the signal only where the pid still has the given start time, read right
/// before. It never signals pid 1 or below, this adapter or its parent. A group signal
/// goes to the group only when the pid leads it.
fn signal(signal: process::Signal) -> process::Sent {
    let not_sent = process::Sent { sent: false };
    let me = std::process::id();
    // SAFETY: getppid takes nothing and cannot fail.
    let parent = unsafe { libc::getppid() } as u32;
    let Ok(pid) = libc::pid_t::try_from(signal.pid) else {
        return not_sent;
    };
    if pid <= 1 || signal.pid == me || signal.pid == parent {
        return not_sent;
    }
    if same(signal.pid, signal.process_start).is_err() {
        return not_sent;
    }
    let number = match signal.signal {
        process::SignalName::Int => libc::SIGINT,
        process::SignalName::Term => libc::SIGTERM,
        process::SignalName::Kill => libc::SIGKILL,
        process::SignalName::Cont => libc::SIGCONT,
    };
    // SAFETY: plain system calls on a checked pid.
    let code = unsafe {
        if signal.group && libc::getpgid(pid) == pid {
            libc::killpg(pid, number)
        } else {
            libc::kill(pid, number)
        }
    };
    process::Sent { sent: code == 0 }
}

impl Linux {
    /// The person's home: the one `initialize` gave, else `HOME`.
    fn home(&self) -> Result<PathBuf, Error> {
        match self.home.get() {
            Some(home) => Ok(home.clone()),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::invalid("no home")),
        }
    }

    /// The folder systemd reads this user's units from.
    fn units(&self) -> Result<PathBuf, Error> {
        Ok(self.home()?.join(".config/systemd/user"))
    }

    /// Three units, as the runtime installs itself: `<label>.service` runs the binary in
    /// `<label>.slice`, handed its listener by `<label>.socket` on 127.0.0.1:`port`, and
    /// the slice bounds it and everything it starts to `cpu` percent of the CPU this
    /// computer grants the user and of its physical memory: the kernel reclaims and
    /// throttles the slice from nine tenths of that memory and kills inside it at the
    /// whole, so the slice never runs the computer out of memory. The service keeps every
    /// directory of the `PATH` the unit on disk gave it. The commands load the units,
    /// enable the socket and the service, bind the socket again only where its address
    /// moved, and let the user linger, which may be refused: the runtime then runs while
    /// the user is logged in.
    fn service_plan(&self, plan: process::ServicePlan) -> Result<process::Plan, Error> {
        let label = unit_label(&plan.label)?;
        if !(1..=100).contains(&plan.cpu) {
            return Err(Error::invalid(format!(
                "cpu is a percent from 1 to 100, not {}",
                plan.cpu
            )));
        }
        if plan.port == 0 {
            return Err(Error::invalid("port 0 is no address to listen on"));
        }
        let home = self.home()?;
        let units = self.units()?;
        let binary = absolute_utf8(&plan.binary, "the binary")?;
        let log = absolute_utf8(&plan.log, "the log")?;
        let home_text = absolute_utf8(&home, "the home")?;
        let service = format!("{label}.service");
        let socket = format!("{label}.socket");
        let slice = format!("{label}.slice");
        let service_path = units.join(&service);
        let socket_path = units.join(&socket);

        let mut env = plan.env;
        let search = search_path(env.get("PATH").map(String::as_str), &service_path, binary);
        env.insert("PATH".into(), search);
        let exec = std::iter::once(binary)
            .chain(plan.args.iter().map(String::as_str))
            .map(|word| quoted(word, true))
            .collect::<Vec<_>>()
            .join(" ");
        let environment: String = env
            .iter()
            .map(|(name, value)| {
                format!(
                    "Environment={}\n",
                    quoted(&format!("{name}={value}"), false)
                )
            })
            .collect();
        let service_unit = format!(
            "[Unit]\n\
             Description=Steery runtime ({label})\n\
             After=network-online.target {socket}\n\
             Requires={socket}\n\n\
             [Service]\n\
             ExecStart={exec}\n\
             WorkingDirectory={home}\n\
             Slice={slice}\n\
             Restart=always\n\
             RestartSec=10\n\
             KillMode=process\n\
             TimeoutStopSec={STOP_GRACE_SECS}\n\
             Nice=10\n\
             {environment}\
             StandardOutput=append:{log}\n\
             StandardError=append:{log}\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            home = home_text.replace('%', "%%"),
            log = log.replace('%', "%%"),
        );
        let socket_unit = format!(
            "[Unit]\n\
             Description=Steery runtime's listener ({label})\n\n\
             [Socket]\n\
             ListenStream=127.0.0.1:{port}\n\
             Accept=no\n\n\
             [Install]\n\
             WantedBy=sockets.target\n",
            port = plan.port,
        );
        let quota = (granted_cpu(label) * u64::from(plan.cpu) / 100).max(1);
        let memory_high = (u16::from(plan.cpu) * 9 / 10).max(1);
        let memory_max = plan.cpu;
        let slice_unit = format!(
            "[Unit]\n\
             Description=Steery runtime and what it starts ({label})\n\n\
             [Slice]\n\
             CPUWeight=20\n\
             IOWeight=20\n\
             CPUQuota={quota}%\n\
             MemoryHigh={memory_high}%\n\
             MemoryMax={memory_max}%\n"
        );
        // A socket whose address stands keeps its queue for the next runtime; one whose
        // address moved is bound again.
        let moved = fs::read_to_string(&socket_path).ok().as_deref() != Some(socket_unit.as_str());
        let manage = |args: &[&str]| process::PlanCommand {
            argv: ["systemctl", "--user"]
                .iter()
                .chain(args)
                .map(|word| (*word).to_owned())
                .collect(),
            may_fail: false,
        };
        Ok(process::Plan {
            files: [
                (service_path, service_unit),
                (socket_path, socket_unit),
                (units.join(&slice), slice_unit),
            ]
            .into_iter()
            .map(|(path, content)| process::PlannedFile {
                path,
                content,
                mode: 0o644,
            })
            .collect(),
            commands: vec![
                manage(&["daemon-reload"]),
                manage(&["enable", &socket, &service]),
                manage(&[if moved { "restart" } else { "start" }, &socket]),
                process::PlanCommand {
                    argv: vec!["loginctl".into(), "enable-linger".into()],
                    may_fail: true,
                },
            ],
        })
    }

    /// Whether the service's unit is on disk, whether systemd runs it and as which
    /// process, and the CPU this computer grants the user.
    async fn service_status(&self, service: process::Service) -> Result<process::Status, Error> {
        let label = unit_label(&service.label)?;
        let name = format!("{label}.service");
        let installed = self.units()?.join(&name).is_file();
        let deadline = Instant::now() + STATUS_BOUND;
        let shown = systemctl(&["show", "--property=ActiveState,MainPID", &name], deadline).await;
        let (running, pid) = match shown {
            Ok(shown) => (
                matches!(
                    property(&shown, "ActiveState"),
                    Some("active" | "reloading")
                ),
                property(&shown, "MainPID")
                    .and_then(|pid| pid.parse::<u32>().ok())
                    .filter(|pid| *pid != 0),
            ),
            // No user systemd to ask, and nothing of the service's on disk.
            Err(_) if !installed => (false, None),
            Err(line) => return Err(refused(line)),
        };
        let process_start = pid
            .and_then(stat)
            .filter(Stat::running)
            .map(|stat| stat.start);
        Ok(process::Status {
            installed,
            running,
            pid: process_start.and(pid),
            process_start,
            granted: u32::try_from(granted_cpu(label)).unwrap_or(u32::MAX),
        })
    }

    /// Stops and removes the service as the runtime uninstalls itself: the service, then
    /// its socket, which would otherwise hold the port and start a runtime at the next
    /// connection, then the slice's file. The slice itself is never stopped, since that
    /// would stop every turn still running in it.
    async fn service_remove(&self, service: process::Service) -> Result<wire::Empty, Error> {
        let label = unit_label(&service.label)?;
        let units = self.units()?;
        let deadline = Instant::now() + SERVICE_BOUND;
        let name = format!("{label}.service");
        let shown = systemctl(
            &["show", "--property=LoadState,ActiveState", &name],
            deadline,
        )
        .await
        .map_err(refused)?;
        let loaded = property(&shown, "LoadState")
            .ok_or_else(|| refused(format!("systemd said no LoadState for {name}")))?;
        let active = property(&shown, "ActiveState")
            .ok_or_else(|| refused(format!("systemd said no ActiveState for {name}")))?;
        if loaded != "not-found" {
            systemctl(&["disable", "--now", &name], deadline)
                .await
                .map_err(refused)?;
        } else if !matches!(active, "inactive" | "failed") {
            // A removed unit file does not stop a process systemd already runs.
            systemctl(&["stop", &name], deadline)
                .await
                .map_err(refused)?;
        }
        removed(&units.join(&name))?;
        let socket = format!("{label}.socket");
        let socket_path = units.join(&socket);
        if socket_path.exists() {
            systemctl(&["disable", "--now", &socket], deadline)
                .await
                .map_err(refused)?;
        }
        removed(&socket_path)?;
        removed(&units.join(format!("{label}.slice")))?;
        systemctl(&["daemon-reload"], deadline)
            .await
            .map_err(refused)?;
        Ok(wire::Empty {})
    }
}

/// Has systemd start the service.
async fn service_start(service: process::Service) -> Result<wire::Empty, Error> {
    let label = unit_label(&service.label)?;
    let deadline = Instant::now() + SERVICE_BOUND;
    systemctl(&["start", &format!("{label}.service")], deadline)
        .await
        .map_err(refused)?;
    Ok(wire::Empty {})
}

/// Stops the service and starts it again. `KillMode=process` stops only the runtime;
/// what it started and is still in its unit's cgroup is moved into a scope of its own in
/// the slice first, so the new runtime starts in the slice, outside every turn.
async fn service_restart(service: process::Service) -> Result<wire::Empty, Error> {
    let label = unit_label(&service.label)?;
    let name = format!("{label}.service");
    let deadline = Instant::now() + SERVICE_BOUND;
    systemctl(&["stop", &name], deadline)
        .await
        .map_err(refused)?;
    settle_leftovers(label, &name, deadline).await;
    systemctl(&["start", &name], deadline)
        .await
        .map_err(refused)?;
    Ok(wire::Empty {})
}

/// Moves whatever is still in the stopped service's cgroup into one transient scope in
/// its slice. Said in the log and carried on when it fails: the service still starts.
async fn settle_leftovers(label: &str, service: &str, deadline: Instant) {
    let group = match systemctl(
        &["show", "--property=ControlGroup", "--value", service],
        deadline,
    )
    .await
    {
        Ok(group) => group.trim().to_owned(),
        Err(line) => {
            eprintln!("linux: finding what the stopped runtime left: {line}");
            return;
        }
    };
    if group.is_empty() {
        return;
    }
    let Ok(procs) = fs::read_to_string(format!("/sys/fs/cgroup{group}/cgroup.procs")) else {
        return;
    };
    let pids: Vec<&str> = procs.split_whitespace().collect();
    if pids.is_empty() {
        return;
    }
    let scope = format!("{label}-leftover-{}.scope", random_hex());
    let slice = format!("{label}.slice");
    let count = pids.len().to_string();
    let mut args = vec![
        "--user",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        scope.as_str(),
        "fail",
        "2",
        "Slice",
        "s",
        slice.as_str(),
        "PIDs",
        "au",
        count.as_str(),
    ];
    args.extend(&pids);
    args.push("0");
    match run("busctl", &args, deadline).await {
        Ok(_) => {
            eprintln!("linux: moved {count} process(es) the stopped runtime left into {scope}")
        }
        Err(line) => eprintln!("linux: could not move what the stopped runtime left: {line}"),
    }
}

/// `label` as the stem of a systemd unit's name: letters, digits and `-_.:`.
fn unit_label(label: &str) -> Result<&str, Error> {
    let fits = !label.is_empty()
        && label.len() <= 200
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if fits {
        Ok(label)
    } else {
        Err(Error::invalid(format!("{label:?} is not a unit name")))
    }
}

/// `path` as UTF-8 text, refused unless it is absolute.
fn absolute_utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str, Error> {
    if !path.is_absolute() {
        return Err(Error::invalid(format!("{what}'s path is not absolute")));
    }
    path.to_str()
        .ok_or_else(|| Error::invalid(format!("{what}'s path is not UTF-8")))
}

/// The `PATH` the service runs with: the one asked for (else this adapter's own, which
/// is the runtime's), then every directory the unit on disk at `unit` gave it, then the
/// binary's own directory, each once, in that order, so an install never narrows it.
fn search_path(asked: Option<&str>, unit: &Path, binary: &str) -> String {
    let own = std::env::var("PATH").unwrap_or_default();
    let installed = fs::read_to_string(unit)
        .ok()
        .and_then(|body| unit_path(&body))
        .unwrap_or_default();
    let beside = Path::new(binary)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or_default();
    let mut kept: Vec<&str> = Vec::new();
    for dir in asked
        .unwrap_or(&own)
        .split(':')
        .chain(installed.split(':'))
        .chain(std::iter::once(beside))
    {
        if !dir.is_empty() && !kept.contains(&dir) {
            kept.push(dir);
        }
    }
    kept.join(":")
}

/// The `PATH` of a unit's `Environment="PATH=…"`, read back as [`quoted`] wrote it.
fn unit_path(body: &str) -> Option<String> {
    let quoted = body
        .lines()
        .find_map(|line| line.strip_prefix("Environment=\"PATH="))?
        .strip_suffix('"')?;
    let mut value = String::with_capacity(quoted.len());
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => value.push('\n'),
                Some(c) => value.push(c),
                None => {}
            },
            '%' => {
                value.push('%');
                if chars.clone().next() == Some('%') {
                    chars.next();
                }
            }
            _ => value.push(c),
        }
    }
    Some(value)
}

/// One quoted word of a unit: what systemd reads inside quotes written so it is read as
/// itself, and `$` doubled where systemd expands variables (`ExecStart=`).
fn quoted(text: &str, dollars: bool) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            '$' if dollars => out.push_str("$$"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One `NAME=value` line of `systemctl show`.
fn property<'a>(shown: &'a str, name: &str) -> Option<&'a str> {
    shown.lines().find_map(|line| {
        line.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// Removes a unit file; one already gone is removed.
fn removed(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(refused(format!("removing {}: {e}", path.display()))),
    }
}

/// `systemctl --user args`, as [`run`].
async fn systemctl(args: &[&str], deadline: Instant) -> Result<String, String> {
    let args: Vec<&str> = std::iter::once("--user")
        .chain(args.iter().copied())
        .collect();
    run("systemctl", &args, deadline).await
}

/// Runs `program` with `args` to its end before `deadline`: its standard output, or one
/// line saying how it failed, the manager's own words where it said any.
async fn run(program: &str, args: &[&str], deadline: Instant) -> Result<String, String> {
    let said = format!("`{program} {}`", args.join(" "));
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout_at(deadline, output).await {
        Err(_) => Err(format!("{said} did not finish in time")),
        Ok(Err(e)) => Err(format!("{said}: {e}")),
        Ok(Ok(output)) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(Ok(output)) => Err(format!(
            "{said} failed: {}",
            one_line(&output.stderr, output.status)
        )),
    }
}

/// A command's standard error as one line, or its exit status where it said nothing.
fn one_line(stderr: &[u8], status: ExitStatus) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if line.is_empty() {
        status.to_string()
    } else {
        line
    }
}

/// Refused `refused` with the manager's line.
fn refused(line: String) -> Error {
    Error::refused(refusal::REFUSED, line)
}

/// Eight random hex digits, for a scope unit's name.
fn random_hex() -> String {
    let mut bytes = [0u8; 4];
    // SAFETY: the buffer is valid for its length.
    let read = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if read != bytes.len() as isize {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        bytes = (nanos ^ std::process::id()).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The CPU this computer grants the user, in percent of one core: its online cores,
/// narrowed by the cores this process may be scheduled on and by every CPU quota above
/// it (a container's, a pod's, a slice's), on cgroup v2 or v1. `label`'s own slice and
/// service are left out: their quota is what a plan writes, and counting it would shrink
/// it on every install.
fn granted_cpu(label: &str) -> u64 {
    // SAFETY: sysconf reads a constant of the running system.
    let online = u64::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1);
    let mut granted = online * 100;
    if let Some(cores) = schedulable_cores() {
        granted = granted.min(cores * 100);
    }
    if let Some(quota) = cgroup_quota(label) {
        granted = granted.min(quota);
    }
    granted.max(1)
}

/// The cores this process may run on.
fn schedulable_cores() -> Option<u64> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set; sched_getaffinity fills it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the set is valid for its size.
    let read = unsafe { libc::sched_getaffinity(0, size_of_val(&set), &mut set) };
    // SAFETY: the set was filled.
    let count = (read == 0).then(|| unsafe { libc::CPU_COUNT(&set) })?;
    u64::try_from(count).ok().filter(|n| *n > 0)
}

/// The tightest CPU quota on this process's cgroup or any above it, in percent of a
/// core, starting above `label`'s slice or service where this runs inside one.
fn cgroup_quota(label: &str) -> Option<u64> {
    let groups = fs::read_to_string("/proc/self/cgroup").ok()?;
    let own = [format!("{label}.slice"), format!("{label}.service")];
    let mut tightest: Option<u64> = None;
    for line in groups.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // v2 is the unified line; v1 is the line whose controllers include `cpu`.
        let base = if controllers.is_empty() {
            PathBuf::from("/sys/fs/cgroup")
        } else if controllers.split(',').any(|c| c == "cpu") {
            match ["/sys/fs/cgroup/cpu", "/sys/fs/cgroup/cpu,cpuacct"]
                .into_iter()
                .map(PathBuf::from)
                .find(|dir| dir.is_dir())
            {
                Some(base) => base,
                None => continue,
            }
        } else {
            continue;
        };
        let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if let Some(at) = parts.iter().position(|p| own.iter().any(|own| own == p)) {
            parts.truncate(at);
        }
        // Each ancestor, deepest first, then the mount's root: inside a container the
        // root is the container's own cgroup, and a host path it names may not exist here.
        for depth in (0..=parts.len()).rev() {
            let dir = parts[..depth]
                .iter()
                .fold(base.clone(), |dir, part| dir.join(part));
            if let Some(quota) = quota_at(&dir) {
                tightest = Some(tightest.map_or(quota, |t| t.min(quota)));
            }
        }
    }
    tightest
}

/// One cgroup's CPU quota in percent of a core: v2's `cpu.max`, v1's `cfs_quota_us` over
/// `cfs_period_us`. `None` where it sets none or the files are not there.
fn quota_at(dir: &Path) -> Option<u64> {
    let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
    let (quota, period) = if let Some(max) = read("cpu.max") {
        let mut words = max.split_whitespace();
        (
            words.next()?.parse::<u64>().ok()?,
            words.next()?.parse::<u64>().ok()?,
        )
    } else {
        let quota = read("cpu.cfs_quota_us")?.trim().parse::<i64>().ok()?;
        let period = read("cpu.cfs_period_us")?.trim().parse::<u64>().ok()?;
        (u64::try_from(quota).ok()?, period)
    };
    (period > 0).then(|| (quota * 100).div_ceil(period))
}

/// What `/proc/<pid>/stat` gives this adapter.
struct Stat {
    /// The state letter: `Z` a zombie, `X` dead, `T` stopped.
    state: char,
    ppid: u32,
    /// Its process group.
    pgid: u32,
    /// The kernel's `PF_*` flags.
    flags: u64,
    /// Milliseconds since 1970.
    start: u64,
}

/// `PF_FORKNOEXEC` of `linux/sched.h`: forked and not exec'd since.
const PF_FORKNOEXEC: u64 = 0x40;

impl Stat {
    /// Whether it still runs: no zombie, not dead.
    fn running(&self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }

    /// Whether it is stopped by a signal (Ctrl-Z, SIGSTOP).
    fn stopped(&self) -> bool {
        self.state == 'T'
    }

    /// Whether it was forked and has not exec'd since, so its descriptors are its
    /// parent's copies.
    fn unexeced(&self) -> bool {
        self.flags & PF_FORKNOEXEC != 0
    }
}

/// Reads fields 3 (state), 4 (ppid), 5 (pgrp), 9 (flags) and 22 (start time in clock
/// ticks since boot), counted after the last `)`, since the command name may hold spaces
/// and parentheses.
fn stat(pid: u32) -> Option<Stat> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    let flags = fields.nth(3)?.parse().ok()?;
    let ticks: u64 = fields.nth(12)?.parse().ok()?;
    let (boot, tick) = (*CLOCK)?;
    Some(Stat {
        state,
        ppid,
        pgid,
        flags,
        start: boot + ticks * 1000 / tick,
    })
}

/// The boot time in milliseconds since 1970 (`btime` of `/proc/stat`) and the clock
/// ticks per second.
static CLOCK: LazyLock<Option<(u64, u64)>> = LazyLock::new(|| {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let boot: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: sysconf only reads a constant.
    let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (tick > 0).then_some((boot * 1000, tick as u64))
});

/// Refused `not-found` unless the pid runs with a start time within `SAME_START_MS` of
/// the given one.
fn same(pid: u32, process_start: u64) -> Result<(), Error> {
    match stat(pid).filter(Stat::running) {
        Some(stat) if stat.start.abs_diff(process_start) <= process::SAME_START_MS => Ok(()),
        Some(_) => Err(Error::refused(
            refusal::NOT_FOUND,
            format!("pid {pid} is another process now"),
        )),
        None => Err(Error::refused(
            refusal::NOT_FOUND,
            format!("pid {pid} is gone"),
        )),
    }
}
