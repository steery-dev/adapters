//! The adapter's work on macOS: libproc and `sysctl` for processes, launchd for the
//! service. The kernel lets a user read only its own processes' details, so a process of
//! another user is left out of a list and refused like a gone one; root reads them all.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::ptr;
use std::sync::LazyLock;
use std::time::Duration;

use libc::c_int;
use steery_adapter_common::user_name;
use steery_sdk::wire::{self, process};
use steery_sdk::{Error, refusal};
use tokio::time::Instant;

/// `PROC_PIDFDVNODEPATHINFO` of `sys/proc_info.h`, which libc does not declare.
const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `PROC_FLAG_EXEC` of `sys/proc_info.h`: the process has called exec.
const PROC_FLAG_EXEC: u32 = 0x4000;

/// `FWRITE` of `sys/fcntl.h`: a descriptor open for writing.
const FWRITE: u32 = 0x0002;

/// `struct proc_fileinfo` of `sys/proc_info.h`, which libc does not declare.
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    _fi_status: u32,
    _fi_offset: i64,
    _fi_type: i32,
    _fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` of `sys/proc_info.h`, which libc does not declare.
#[repr(C)]
struct VnodeFdInfoWithPath {
    file: ProcFileInfo,
    path: libc::vnode_info_path,
}

/// Starts `argv` detached (`steery_adapter_common::start`): launchd stops only the
/// stopped job's own process group, so it outlives this adapter and the runtime. `scope` is
/// ignored: launchd has no share of the CPU to run it in.
pub fn start(start: process::Start) -> Result<process::Started, Error> {
    steery_adapter_common::start(start, |pid| info(pid).map(|info| info.start))
}

/// Every process running, or those of `pids` that run, as [`entry`] reads each.
pub fn list(list: process::List) -> process::Listed {
    let pids = match list.pids {
        Some(pids) => pids,
        None => all_pids(),
    };
    let mut users = BTreeMap::new();
    let mut buffer = Vec::new();
    let processes = pids
        .into_iter()
        .filter_map(|pid| entry(pid, &mut users, &mut buffer))
        .collect();
    process::Listed { processes }
}

/// Every pid running, from `proc_listallpids`.
fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes started since the count.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let Ok(bytes) = c_int::try_from(pids.len() * size_of::<libc::pid_t>()) else {
        return Vec::new();
    };
    // SAFETY: the buffer is valid for `bytes`.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    pids.truncate(count);
    pids.into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

/// One process: left out where it is a zombie, gone, or another user's whose arguments
/// this user may not read; one of this user's whose arguments cannot be read is listed
/// without them. Root reads every process's, and lists every process. `kernel_task`
/// (pid 0) is no process to name.
fn entry(
    pid: u32,
    users: &mut BTreeMap<u32, String>,
    buffer: &mut Vec<u8>,
) -> Option<process::Entry> {
    if pid == 0 {
        return None;
    }
    let info = info(pid).filter(|info| !info.zombie)?;
    let args = procargs(pid, buffer).map(|procargs| procargs.args);
    if args.is_none() && info.uid != *ME && *ME != 0 {
        return None;
    }
    let user = users
        .entry(info.uid)
        .or_insert_with(|| user_name(info.uid))
        .clone();
    Some(process::Entry {
        pid,
        ppid: info.ppid,
        pgid: info.pgid,
        process_start: info.start,
        command: info.command,
        args,
        user,
        stopped: info.stopped,
        cwd: cwd(pid),
    })
}

/// The folder a process works in now; none where it is gone or this user may not read it.
fn cwd(pid: u32) -> Option<PathBuf> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: proc_vnodepathinfo is plain data.
    let mut paths: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
    // SAFETY: the buffer is valid for `size` bytes.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut paths).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(bytes_of(
        paths.pvi_cdir.vip_path.as_flattened(),
    )));
    path.is_absolute().then_some(path)
}

/// The user this adapter runs as.
static ME: LazyLock<u32> = LazyLock::new(|| {
    // SAFETY: getuid takes nothing and cannot fail.
    unsafe { libc::getuid() }
});

/// The asked variables of a process, refused `not-found` unless the pid still has the
/// given start time.
pub fn environ(environ: process::Environ) -> Result<process::Values, Error> {
    same(environ.pid, environ.process_start)?;
    let mut buffer = Vec::new();
    let procargs = procargs(environ.pid, &mut buffer).ok_or_else(|| {
        Error::refused(
            refusal::NOT_FOUND,
            format!("pid {}'s environment is unreadable", environ.pid),
        )
    })?;
    let names: BTreeSet<&str> = environ.names.iter().map(String::as_str).collect();
    let mut values = BTreeMap::new();
    for pair in &procargs.environ {
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

/// The files a process holds open: each vnode descriptor that names an absolute path,
/// only those open for writing where `writing`. A process forked and not exec'd since
/// holds its parent's copies, so it answers none.
pub fn files(files: process::Files) -> Result<process::Paths, Error> {
    let execed = info(files.pid).is_some_and(|info| info.execed);
    same(files.pid, files.process_start)?;
    if !execed {
        return Ok(process::Paths { paths: Vec::new() });
    }
    let unreadable = || {
        Error::refused(
            refusal::NOT_FOUND,
            format!(
                "pid {}'s descriptors: {}",
                files.pid,
                io::Error::last_os_error()
            ),
        )
    };
    let pid = c_int::try_from(files.pid).map_err(|_| unreadable())?;
    let size = size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size.
    let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
    let bytes = usize::try_from(bytes).map_err(|_| unreadable())?;
    // Room for descriptors opened since the size.
    // SAFETY: proc_fdinfo is plain data.
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; bytes / size + 16];
    let room = c_int::try_from(fds.len() * size).map_err(|_| unreadable())?;
    // SAFETY: the buffer is valid for `room` bytes.
    let bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room) };
    let bytes = usize::try_from(bytes).map_err(|_| unreadable())?;
    fds.truncate(bytes / size);
    let mut paths = BTreeSet::new();
    for fd in fds {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        // SAFETY: the struct is plain data.
        let mut vnode: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let size = size_of::<VnodeFdInfoWithPath>() as c_int;
        // SAFETY: the buffer is valid for `size` bytes.
        let read = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                (&raw mut vnode).cast(),
                size,
            )
        };
        // A descriptor closed since the list is left out.
        if read != size || (files.writing && vnode.file.fi_openflags & FWRITE == 0) {
            continue;
        }
        let path = PathBuf::from(OsString::from_vec(bytes_of(
            vnode.path.vip_path.as_flattened(),
        )));
        if path.is_absolute() {
            paths.insert(path);
        }
    }
    same(files.pid, files.process_start)?;
    Ok(process::Paths {
        paths: paths.into_iter().collect(),
    })
}

/// Sends the signal only where the pid still has the given start time, read right
/// before. It never signals pid 1 or below, this adapter or its parent. A group signal
/// goes to the group only when the pid leads it.
pub fn signal(signal: process::Signal) -> process::Sent {
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

/// The seconds the runtime takes to stop once asked: its own shutdown grace, which its
/// plist gives launchd before it kills it.
const STOP_GRACE_SECS: u64 = 40;

/// The longest `process/listening` waits on `lsof`, inside the runtime's 5 s bound.
const LISTENING_BOUND: Duration = Duration::from_secs(4);

/// The longest `service/status` waits on launchd, inside the runtime's 10 s bound.
const STATUS_BOUND: Duration = Duration::from_secs(8);

/// The longest `service/start`, `restart` and `remove` wait on launchd, inside the
/// runtime's 30 s bound.
const SERVICE_BOUND: Duration = Duration::from_secs(28);

/// The processes listening on a TCP port, as `lsof` finds them. macOS shows a user only
/// its own processes' sockets unless it runs as root.
pub async fn listening(listening: process::Listening) -> Result<process::Listeners, Error> {
    let deadline = Instant::now() + LISTENING_BOUND;
    let port = format!("-iTCP:{}", listening.port);
    let output = output(
        "/usr/sbin/lsof",
        &["-nP", &port, "-sTCP:LISTEN", "-Fp"],
        deadline,
    )
    .await
    .map_err(refused)?;
    // `lsof` exits 1 when nothing listens; each process it found is a `p<pid>` line.
    let pids: BTreeSet<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse().ok())
        .collect();
    let processes = pids
        .into_iter()
        .filter_map(|pid| {
            let info = info(pid).filter(|info| !info.zombie)?;
            Some(process::Listener {
                pid,
                process_start: info.start,
                command: info.command,
            })
        })
        .collect();
    Ok(process::Listeners { processes })
}

/// A LaunchAgent, as the runtime installs itself: it runs the binary at login and keeps
/// it running, handed its listener by launchd on 127.0.0.1:`port`, its output appended to
/// `log`. `ProcessType Interactive` lifts the light limits launchd puts on a job with none,
/// which ran every turn at utility QoS on one cluster of cores (a turn's spinner got 3.6%
/// of a core, 2026-11-03); `Nice 10` keeps it below its person's apps (macOS bounds a
/// user's CPU no other way, so `cpu` is checked and has nothing to set). It keeps every
/// directory of the `PATH` the plist on
/// disk gave it. The commands unload the job an earlier plist loaded, only where launchd
/// has one, so a first install says no `No such process`; enable it, which an earlier
/// `launchctl disable` would otherwise keep from loading; and load it in this user's GUI
/// domain. `bootout` returns while the old
/// job is still stopping (up to its `ExitTimeOut`), and a `bootstrap` then fails with
/// launchd's error 5 until it has gone: an update that replaced the plist was left with no
/// runtime at all (2026-10-09, 6.12.0). So the load is tried again each second until the
/// old job is gone or [`STOP_GRACE_SECS`] and a few more have passed.
pub fn service_plan(home: &Path, plan: process::ServicePlan) -> Result<process::Plan, Error> {
    let label = launchd_label(&plan.label)?;
    if !(1..=100).contains(&plan.cpu) {
        return Err(Error::invalid(format!(
            "cpu is a percent from 1 to 100, not {}",
            plan.cpu
        )));
    }
    if plan.port == 0 {
        return Err(Error::invalid("port 0 is no address to listen on"));
    }
    let binary = absolute_utf8(&plan.binary, "the binary")?;
    let log = absolute_utf8(&plan.log, "the log")?;
    let home_text = absolute_utf8(home, "the home")?;
    let path = plist(home, label);
    let mut env = plan.env;
    let search = search_path(env.get("PATH").map(String::as_str), &path, binary);
    env.insert("PATH".into(), search);
    let arguments: String = std::iter::once(binary)
        .chain(plan.args.iter().map(String::as_str))
        .map(|word| format!("    <string>{}</string>\n", escape(word)))
        .collect();
    let environment: String = env
        .iter()
        .map(|(name, value)| {
            format!(
                "    <key>{}</key><string>{}</string>\n",
                escape(name),
                escape(value)
            )
        })
        .collect();
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
{arguments}  </array>
  <key>WorkingDirectory</key><string>{home}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>{STOP_GRACE_SECS}</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>Nice</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict>
{environment}  </dict>
  <key>Sockets</key><dict>
    <key>Listeners</key><dict>
      <key>SockNodeName</key><string>127.0.0.1</string>
      <key>SockServiceName</key><string>{port}</string>
      <key>SockType</key><string>stream</string>
      <key>SockFamily</key><string>IPv4</string>
    </dict>
  </dict>
</dict></plist>
"#,
        home = escape(home_text),
        log = escape(log),
        port = plan.port,
    );
    let target = target(label);
    let launchctl = |args: &[&str], may_fail: bool| process::PlanCommand {
        argv: std::iter::once("launchctl")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        may_fail,
    };
    let commands = vec![
        bootout_where_loaded(&target),
        launchctl(&["enable", &target], false),
        bootstrap_after_stop(&path, &target),
    ];
    Ok(process::Plan {
        files: vec![process::PlannedFile {
            path,
            content,
            mode: 0o644,
        }],
        commands,
    })
}

/// The `launchctl bootout` of `target` where launchd has it loaded, and nothing where it
/// has none: a first install, or a service a person's Quit already let go. Its own failure
/// is said and does not stop the plan, as before; only the one that is no failure, a job
/// not there to unload, is no longer said.
fn bootout_where_loaded(target: &str) -> process::PlanCommand {
    process::PlanCommand {
        argv: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "launchctl print \"$1\" >/dev/null 2>&1 || exit 0; exec launchctl bootout \"$1\""
                .to_owned(),
            "steery-bootout".to_owned(),
            target.to_owned(),
        ],
        may_fail: true,
    }
}

/// The `launchctl bootstrap` of `path` into this user's GUI domain, tried again once a
/// second while launchd still answers that the job is busy stopping, for the stop grace
/// and [`BOOTSTRAP_SLACK_SECS`] more; any other failure, or the last one, is the plan's.
///
/// launchd answers the same error 5 for a job already loaded, and something else may load
/// it while this waits: Steery.app's watchdog starts the runtime when its door stays shut.
/// A failure is so a success when `launchctl print` finds the job loaded and not being
/// stopped: the old job, still stopping, reads `state = SIGTERMed` (or `SIGKILLed`).
fn bootstrap_after_stop(path: &Path, target: &str) -> process::PlanCommand {
    let tries = STOP_GRACE_SECS + BOOTSTRAP_SLACK_SECS;
    let script = format!(
        "i=0; while :; do out=$(launchctl bootstrap \"$1\" \"$2\" 2>&1) && exit 0; \
         case \"$(launchctl print \"$3\" 2>/dev/null)\" in \
         *\"state = SIG\"*) ;; *\"state = \"*) exit 0;; esac; \
         i=$((i+1)); case \"$out\" in *\"Bootstrap failed: 5:\"*) [ $i -lt {tries} ] && \
         {{ sleep 1; continue; }};; esac; printf '%s\\n' \"$out\" >&2; exit 1; done"
    );
    process::PlanCommand {
        argv: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            script,
            "steery-bootstrap".to_owned(),
            domain(),
            path.to_string_lossy().into_owned(),
            target.to_owned(),
        ],
        may_fail: false,
    }
}

/// How many seconds past the stop grace a load keeps waiting for the old job to go.
const BOOTSTRAP_SLACK_SECS: u64 = 10;

/// Whether the agent's plist is on disk, whether launchd runs it and as which process,
/// and the CPU this Mac grants the user: all its online cores.
pub async fn service_status(
    home: &Path,
    service: process::Service,
) -> Result<process::Status, Error> {
    let label = launchd_label(&service.label)?;
    let installed = plist(home, label).is_file();
    let deadline = Instant::now() + STATUS_BOUND;
    let pid = match printed(&target(label), deadline).await {
        Ok(Some(printed)) => printed.lines().find_map(|line| {
            line.trim()
                .strip_prefix("pid = ")?
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid != 0)
        }),
        Ok(None) => None,
        // launchd could not be asked, and nothing of the agent's is on disk.
        Err(_) if !installed => None,
        Err(line) => return Err(refused(line)),
    };
    let process_start = pid
        .and_then(info)
        .filter(|info| !info.zombie)
        .map(|info| info.start);
    // SAFETY: sysconf reads a constant of the running system.
    let online = u32::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1);
    Ok(process::Status {
        installed,
        running: process_start.is_some(),
        pid: process_start.and(pid),
        process_start,
        granted: online.saturating_mul(100),
    })
}

/// Has launchd run the agent: `kickstart` where its job is loaded (`-k` stops it first,
/// for a restart), else loads it from its plist, which runs it at load. launchd stops only
/// a job's own process group, so the turns it started in theirs keep running.
pub async fn service_start(
    home: &Path,
    service: process::Service,
    restart: bool,
) -> Result<wire::Empty, Error> {
    let label = launchd_label(&service.label)?;
    let target = target(label);
    let deadline = Instant::now() + SERVICE_BOUND;
    if loaded(&target, deadline).await.map_err(refused)? {
        let args: &[&str] = if restart {
            &["kickstart", "-k", &target]
        } else {
            &["kickstart", &target]
        };
        run("launchctl", args, deadline).await.map_err(refused)?;
        return Ok(wire::Empty {});
    }
    let path = plist(home, label);
    if !path.is_file() {
        return Err(refused(format!(
            "{label} is not installed: {} is not there",
            path.display()
        )));
    }
    run("launchctl", &["enable", &target], deadline)
        .await
        .map_err(refused)?;
    run(
        "launchctl",
        &["bootstrap", &domain(), &path.to_string_lossy()],
        deadline,
    )
    .await
    .map_err(refused)?;
    Ok(wire::Empty {})
}

/// Removes the plist, then unloads the job, as the runtime uninstalls itself: the file
/// first, since unloading stops the runtime that may be asking, and a plist left on disk
/// is one launchd loads again at the next login. A job that is gone by the time it is
/// unloaded is unloaded.
pub async fn service_remove(home: &Path, service: process::Service) -> Result<wire::Empty, Error> {
    let label = launchd_label(&service.label)?;
    let path = plist(home, label);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(refused(format!("removing {}: {e}", path.display()))),
    }
    let target = target(label);
    let deadline = Instant::now() + SERVICE_BOUND;
    if !loaded(&target, deadline).await.map_err(refused)? {
        return Ok(wire::Empty {});
    }
    if let Err(line) = run("launchctl", &["bootout", &target], deadline).await
        && loaded(&target, deadline).await.map_err(refused)?
    {
        return Err(refused(line));
    }
    Ok(wire::Empty {})
}

/// `label` as a launchd label: letters, digits and `-_.`.
fn launchd_label(label: &str) -> Result<&str, Error> {
    let fits = !label.is_empty()
        && label.len() <= 200
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if fits {
        Ok(label)
    } else {
        Err(Error::invalid(format!("{label:?} is not a launchd label")))
    }
}

/// The agent's plist.
fn plist(home: &Path, label: &str) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{label}.plist"))
}

/// This user's GUI domain.
fn domain() -> String {
    // SAFETY: geteuid takes nothing and cannot fail.
    format!("gui/{}", unsafe { libc::geteuid() })
}

/// The agent's job in this user's GUI domain.
fn target(label: &str) -> String {
    format!("{}/{label}", domain())
}

/// `path` as UTF-8 text, refused unless it is absolute.
fn absolute_utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str, Error> {
    if !path.is_absolute() {
        return Err(Error::invalid(format!("{what}'s path is not absolute")));
    }
    path.to_str()
        .ok_or_else(|| Error::invalid(format!("{what}'s path is not UTF-8")))
}

/// The `PATH` the agent runs with: the one asked for (else this adapter's own, which is
/// the runtime's), then every directory the plist on disk at `plist` gave it, then the
/// binary's own directory, each once, in that order, so an install never narrows it.
fn search_path(asked: Option<&str>, plist: &Path, binary: &str) -> String {
    let own = std::env::var("PATH").unwrap_or_default();
    let installed = std::fs::read_to_string(plist)
        .ok()
        .and_then(|body| plist_path(&body))
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

/// The `PATH` string of a plist's environment, read back as [`escape`] wrote it.
fn plist_path(body: &str) -> Option<String> {
    const PATH: &str = "<key>PATH</key><string>";
    let value = &body[body.find(PATH)? + PATH.len()..];
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// Whether launchd has `target` loaded: `print` answers for a loaded job and says
/// `Could not find service` for one that is not; anything else is no answer.
async fn loaded(target: &str, deadline: Instant) -> Result<bool, String> {
    printed(target, deadline)
        .await
        .map(|printed| printed.is_some())
}

/// What `launchctl print` says of a loaded `target`; `None` where it is not loaded.
async fn printed(target: &str, deadline: Instant) -> Result<Option<String>, String> {
    let args = ["print", target];
    let output = output("launchctl", &args, deadline).await?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    if String::from_utf8_lossy(&output.stderr).contains("Could not find service") {
        return Ok(None);
    }
    Err(failed("launchctl", &args, &output))
}

/// Runs `program` with `args` to its end before `deadline`, refused unless it succeeds,
/// with one line saying how it failed: launchd's own words where it said any.
async fn run(program: &str, args: &[&str], deadline: Instant) -> Result<(), String> {
    let output = output(program, args, deadline).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failed(program, args, &output))
    }
}

/// Runs `program` with `args` to its end before `deadline`, whatever its exit status.
async fn output(program: &str, args: &[&str], deadline: Instant) -> Result<Output, String> {
    let command = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout_at(deadline, command).await {
        Err(_) => Err(format!(
            "`{program} {}` did not finish in time",
            args.join(" ")
        )),
        Ok(Err(e)) => Err(format!("`{program} {}`: {e}", args.join(" "))),
        Ok(Ok(output)) => Ok(output),
    }
}

/// How a command failed, in one line: its standard error, or its exit status where it
/// said nothing.
fn failed(program: &str, args: &[&str], output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let said = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let said = if said.is_empty() {
        output.status.to_string()
    } else {
        said
    };
    format!("`{program} {}` failed: {said}", args.join(" "))
}

/// Refused `refused` with launchd's line.
fn refused(line: String) -> Error {
    Error::refused(refusal::REFUSED, line)
}

/// The five characters an XML document cannot carry raw. Every value written into the
/// plist is somebody's path or setting, so none of them is assumed to be safe.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// What `proc_pidinfo(PROC_PIDTBSDINFO)` gives this adapter.
struct Info {
    ppid: u32,
    /// Its process group.
    pgid: u32,
    /// The effective user.
    uid: u32,
    /// It exited and waits to be reaped.
    zombie: bool,
    /// It is stopped by a signal (Ctrl-Z, SIGSTOP).
    stopped: bool,
    /// It has called exec since it was forked.
    execed: bool,
    /// Milliseconds since 1970.
    start: u64,
    command: String,
}

/// A process's BSD info; none where it is gone or this user may not read it.
fn info(pid: u32) -> Option<Info> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is plain data.
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: the buffer is valid for `size` bytes.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size) };
    if read != size {
        return None;
    }
    // `pbi_name` is the longer name, as `proc_name` reads it; `pbi_comm` when it is empty.
    let name = if bsd.pbi_name[0] != 0 {
        &bsd.pbi_name[..]
    } else {
        &bsd.pbi_comm[..]
    };
    Some(Info {
        ppid: bsd.pbi_ppid,
        pgid: bsd.pbi_pgid,
        uid: bsd.pbi_uid,
        zombie: bsd.pbi_status == libc::SZOMB,
        stopped: bsd.pbi_status == libc::SSTOP,
        execed: bsd.pbi_flags & PROC_FLAG_EXEC != 0,
        start: bsd.pbi_start_tvsec * 1000 + bsd.pbi_start_tvusec / 1000,
        command: String::from_utf8_lossy(&bytes_of(name)).into_owned(),
    })
}

/// A C string's bytes, up to its first NUL.
fn bytes_of(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect()
}

/// What `KERN_PROCARGS2` gives this adapter.
struct Procargs {
    /// The arguments, its own name first.
    args: Vec<String>,
    /// The environment, `NAME=value` each.
    environ: Vec<String>,
}

/// A process's arguments and environment, read into `buffer`; none where it is gone or
/// this user may not read it. The kernel lays them out as `argc`, the executable's path
/// and NULs to align, then each argument and each variable NUL-terminated, and an empty
/// string after the environment.
fn procargs(pid: u32, buffer: &mut Vec<u8>) -> Option<Procargs> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        c_int::try_from(pid).ok()?,
    ];
    buffer.resize(*ARGMAX, 0);
    let mut size = buffer.len();
    // SAFETY: the name has three parts, and the buffer is valid for `size` bytes.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return None;
    }
    let bytes = buffer.get(..size)?;
    let argc = usize::try_from(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &bytes[4..];
    let rest = &rest[rest.iter().position(|b| *b == 0)?..];
    let rest = &rest[rest.iter().position(|b| *b != 0).unwrap_or(rest.len())..];
    let mut strings = rest.split(|b| *b == 0);
    let lossy = |text: &[u8]| String::from_utf8_lossy(text).into_owned();
    let args = strings.by_ref().take(argc).map(lossy).collect();
    let environ = strings
        .take_while(|text| !text.is_empty())
        .map(lossy)
        .collect();
    Some(Procargs { args, environ })
}

/// The kernel's bound on a process's arguments and environment together (`KERN_ARGMAX`).
static ARGMAX: LazyLock<usize> = LazyLock::new(|| {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut value: c_int = 0;
    let mut size = size_of::<c_int>();
    // SAFETY: the name has two parts, and `value` is valid for `size` bytes.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut value).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    match usize::try_from(value) {
        Ok(value) if code == 0 && value > 0 => value,
        _ => 1 << 20,
    }
});

/// Refused `not-found` unless the pid runs, no zombie, with a start time within
/// `SAME_START_MS` of the given one.
fn same(pid: u32, process_start: u64) -> Result<(), Error> {
    match info(pid).filter(|info| !info.zombie) {
        Some(info) if info.start.abs_diff(process_start) <= process::SAME_START_MS => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A service replaced while it runs, as a runtime replaces its own after an update, is
    /// loaded again once the plan's commands are run in order: `bootout` returns while the
    /// old job is still stopping, and a `bootstrap` then fails and leaves none loaded. A
    /// plan run while something else loads the job in that window (Steery.app's watchdog)
    /// succeeds too: the job is up. The first install, with nothing loaded to unload, runs
    /// every command cleanly: a person adding a machine is shown no `No such process`.
    /// Run against this user's own launchd, with a label of its own.
    #[test]
    fn a_running_service_replaced_by_its_plan_is_loaded_again() {
        let home = std::env::temp_dir().join(format!("steery-launchd-{}", std::process::id()));
        let label = format!("dev.steery.test.replace{}", std::process::id());
        let plan = service_plan(
            &home,
            process::ServicePlan {
                label: label.clone(),
                binary: PathBuf::from("/bin/sh"),
                // It takes three seconds to stop once launchd asks it to.
                args: vec![
                    "-c".to_owned(),
                    "trap 'sleep 3; exit 0' TERM; while :; do sleep 1; done".to_owned(),
                ],
                env: BTreeMap::new(),
                port: 55990,
                cpu: 50,
                log: home.join("run.log"),
            },
        )
        .unwrap();
        let launchctl = |args: &[&str]| {
            std::process::Command::new("launchctl")
                .args(args)
                .output()
                .unwrap()
        };
        let path = plist(&home, &label);
        // Each command in order, as the runtime runs a plan; `meanwhile` runs once the
        // old job was asked to stop, before the load. Says whether every command succeeded.
        let apply = |meanwhile: &dyn Fn()| -> bool {
            for file in &plan.files {
                std::fs::create_dir_all(file.path.parent().unwrap()).unwrap();
                std::fs::write(&file.path, &file.content).unwrap();
            }
            let mut clean = true;
            for (at, command) in plan.commands.iter().enumerate() {
                if at == plan.commands.len() - 1 {
                    meanwhile();
                }
                let ran = std::process::Command::new(&command.argv[0])
                    .args(&command.argv[1..])
                    .output()
                    .unwrap();
                assert!(
                    ran.status.success() || command.may_fail,
                    "{:?}: {}",
                    &command.argv[..2],
                    String::from_utf8_lossy(&ran.stderr)
                );
                clean &= ran.status.success();
            }
            clean
        };
        let loaded = || launchctl(&["print", &target(&label)]).status.success();
        let result = std::panic::catch_unwind(|| {
            assert!(apply(&|| {}), "a first install runs every command cleanly");
            std::thread::sleep(Duration::from_secs(1));
            assert!(loaded(), "installed");
            apply(&|| {});
            std::thread::sleep(Duration::from_secs(4));
            assert!(loaded(), "replaced while it ran, it is loaded again");
            // Something else loads it while the plan waits for the old job to stop.
            apply(&|| {
                while loaded() {
                    std::thread::sleep(Duration::from_millis(200));
                }
                launchctl(&["bootstrap", &domain(), &path.to_string_lossy()]);
            });
            assert!(loaded(), "loaded by another, the plan still succeeds");
        });
        launchctl(&["bootout", &target(&label)]);
        let _ = std::fs::remove_dir_all(&home);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
