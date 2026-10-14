//! What Steery's operating-system adapters share, where POSIX behaves the same on Linux and
//! macOS: `process/start`'s detached start, with the `STEERY_STARTED` it sets and the
//! status file it writes once it reaps the process; the files a process writes its output
//! to; and a user's name. What the system itself keeps — `/proc` or libproc, systemd or
//! launchd — stays in each adapter.

use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use steery_sdk::wire::process;
use steery_sdk::{Error, refusal};

/// Starts `start.argv` detached, in a session and process group of its own, with exactly
/// the environment `start.env` (and `STEERY_STARTED=<this adapter's pid>:<what>` where
/// `start.started` names one), so it outlives the adapter and the runtime. `process_start`
/// reads the started process's start time, before it can be reaped: until it is, an
/// exited child keeps its entry. Once reaped, how it ended is written to `start.status`.
/// `start.scope` is the adapter's to have applied to `argv` already. A `held` start's
/// standard input never ends (`held`); `held` with `stdin` is refused `invalid`.
pub fn start(
    start: process::Start,
    process_start: impl FnOnce(u32) -> Option<u64>,
) -> Result<process::Started, Error> {
    let Some((program, args)) = start.argv.split_first() else {
        return Err(Error::invalid("argv is empty"));
    };
    let failed = |what: &str, path: &Path, e: io::Error| {
        Error::refused(
            refusal::START_FAILED,
            format!("{what} {}: {e}", path.display()),
        )
    };
    let stdin = match (&start.stdin, start.held) {
        (Some(_), true) => return Err(Error::invalid("held and stdin are both set")),
        (Some(path), false) => {
            Stdio::from(File::open(path).map_err(|e| failed("stdin", path, e))?)
        }
        (None, true) => Stdio::from(held().map_err(|e| {
            Error::refused(refusal::START_FAILED, format!("held stdin: {e}"))
        })?),
        (None, false) => Stdio::null(),
    };
    let stdout = append(&start.stdout).map_err(|e| failed("stdout", &start.stdout, e))?;
    let stderr = append(&start.stderr).map_err(|e| failed("stderr", &start.stderr, e))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(&start.env)
        .current_dir(&start.cwd)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    if let Some(what) = &start.started {
        command.env("STEERY_STARTED", format!("{}:{what}", std::process::id()));
    }
    // SAFETY: setsid is async-signal-safe, and the closure touches nothing else.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| Error::refused(refusal::START_FAILED, format!("{program}: {e}")))?;
    let pid = child
        .id()
        .ok_or_else(|| Error::refused(refusal::START_FAILED, "it exited at once"))?;
    let process_start = process_start(pid)
        .ok_or_else(|| Error::refused(refusal::START_FAILED, "its start time is unreadable"))?;
    let status = start.status;
    tokio::spawn(async move {
        if let (Ok(ended), Some(path)) = (child.wait().await, status) {
            write_ended(&path, ended);
        }
    });
    Ok(process::Started { pid, process_start })
}

/// A standard input that never ends: a FIFO in a private folder, opened for reading and
/// writing, then unlinked with its folder. The process's own fd 0 is then a writer too, so
/// its reads block and never see an end, and nothing outside it holds the FIFO: it
/// outlives the adapter and the runtime restarting.
fn held() -> io::Result<File> {
    let template = std::env::temp_dir().join("steery-held-XXXXXX");
    let mut template = CString::new(template.into_os_string().into_vec())?.into_bytes_with_nul();
    // SAFETY: mkdtemp rewrites the template's X's in place, inside its NUL-terminated buffer.
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    template.pop();
    let folder = PathBuf::from(OsString::from_vec(template));
    let fifo = folder.join("stdin");
    let opened = CString::new(fifo.as_os_str().as_bytes())
        .map_err(io::Error::from)
        .and_then(|path| {
            // SAFETY: mkfifo reads one NUL-terminated path.
            if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == -1 {
                return Err(io::Error::last_os_error());
            }
            OpenOptions::new().read(true).write(true).open(&fifo)
        });
    let _ = fs::remove_file(&fifo);
    let _ = fs::remove_dir(&folder);
    opened
}

/// Writes how a reaped child ended to `path`, mode 0600, whole: through a file beside it
/// renamed over it. A failure is the reader's to see as no file.
pub fn write_ended(path: &Path, ended: ExitStatus) {
    let ended = match (ended.code(), ended.signal()) {
        (Some(code), _) => process::Ended::Code(code),
        (None, Some(signal)) => process::Ended::Signal(signal),
        (None, None) => return,
    };
    let Ok(said) = serde_json::to_vec(&ended) else {
        return;
    };
    let mut beside = path.as_os_str().to_owned();
    beside.push(".writing");
    let beside = PathBuf::from(beside);
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&beside)
        .and_then(|mut file| file.write_all(&said))
        .and_then(|()| fs::rename(&beside, path));
    if written.is_err() {
        let _ = fs::remove_file(&beside);
    }
}

/// A file opened for appending, made with mode 0600 if missing.
pub fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// The user's name from the password database, else the number.
pub fn user_name(uid: u32) -> String {
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: passwd is plain data; getpwuid_r fills it or leaves `found` null.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the length given.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 || found.is_null() || entry.pw_name.is_null() {
            return uid.to_string();
        }
        // SAFETY: pw_name points into `buffer`, NUL-terminated.
        return unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    use super::*;

    fn folder(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!(
            "steery-adapter-common-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The status file, once the reaper wrote it.
    async fn ended(path: &Path) -> process::Ended {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(said) = fs::read(path) {
                return serde_json::from_slice(&said).unwrap();
            }
            assert!(Instant::now() < until, "no status file");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A start runs the command in a session of its own, with exactly the environment
    /// given and `STEERY_STARTED`, its output appended to files made 0600; once it is
    /// reaped, how it ended is written whole, 0600. A held start's standard input never
    /// ends, so a read waits until a signal ends it; held with `stdin` is refused.
    #[tokio::test]
    async fn a_start_is_detached_and_its_end_is_written() {
        let folder = folder("start");
        let (stdout, stderr, status) = (
            folder.join("out"),
            folder.join("err"),
            folder.join("status"),
        );
        fs::write(&stdout, "before\n").unwrap();
        let started = start(
            process::Start {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo \"$GIVEN $STEERY_STARTED ${HOME:-none}\"; echo said >&2; sleep 0.3; exit 3".into(),
                ],
                env: BTreeMap::from([
                    ("GIVEN".to_owned(), "given".to_owned()),
                    ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ]),
                cwd: folder.clone(),
                stdin: None,
                held: false,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                scope: None,
                started: Some("turn".into()),
                status: Some(status.clone()),
            },
            |_| Some(7),
        )
        .unwrap();
        assert_eq!(started.process_start, 7);
        // SAFETY: getsid reads one process's session.
        let session = unsafe { libc::getsid(started.pid as libc::pid_t) };
        assert_eq!(session, started.pid as libc::pid_t, "a session of its own");
        assert_eq!(ended(&status).await, process::Ended::Code(3));
        assert_eq!(mode(&status), 0o600);
        assert_eq!(
            fs::read_to_string(&stdout).unwrap(),
            format!("before\ngiven {}:turn none\n", std::process::id())
        );
        assert_eq!(fs::read_to_string(&stderr).unwrap(), "said\n");
        assert_eq!(mode(&stderr), 0o600);
        assert!(!folder.join("status.writing").exists());

        let held_status = folder.join("held-status");
        let held = |stdin: Option<PathBuf>| process::Start {
            argv: vec!["/bin/sh".into(), "-c".into(), "read l; echo got".into()],
            env: BTreeMap::new(),
            cwd: folder.clone(),
            stdin,
            held: true,
            stdout: folder.join("held-out"),
            stderr: folder.join("held-err"),
            scope: None,
            started: None,
            status: Some(held_status.clone()),
        };
        assert!(matches!(
            start(held(Some(stdout.clone())), |_| Some(8)),
            Err(Error::Invalid(_))
        ));
        let started = start(held(None), |_| Some(8)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!held_status.exists(), "a held read never returns");
        // SAFETY: kill signals one process.
        assert_eq!(unsafe { libc::kill(started.pid as libc::pid_t, libc::SIGTERM) }, 0);
        assert_eq!(ended(&held_status).await, process::Ended::Signal(libc::SIGTERM));
        assert_eq!(fs::read_to_string(folder.join("held-out")).unwrap(), "");
        let _ = fs::remove_dir_all(&folder);
    }

    /// A process a signal ended says the signal; one that cannot be started, or whose
    /// start time cannot be read, is refused `start-failed`.
    #[tokio::test]
    async fn a_signal_is_said_and_a_failed_start_refused() {
        let folder = folder("signal");
        let status = folder.join("status");
        let asked = |argv: Vec<String>| process::Start {
            argv,
            env: BTreeMap::new(),
            cwd: folder.clone(),
            stdin: None,
            held: false,
            stdout: folder.join("out"),
            stderr: folder.join("err"),
            scope: None,
            started: None,
            status: Some(status.clone()),
        };
        start(
            asked(vec!["/bin/sh".into(), "-c".into(), "kill -9 $$".into()]),
            |_| Some(1),
        )
        .unwrap();
        assert_eq!(ended(&status).await, process::Ended::Signal(9));
        let word = |refused: Result<process::Started, Error>| match refused {
            Err(Error::Refused { word, .. }) => word,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            word(start(
                asked(vec![folder.join("none").display().to_string()]),
                |_| Some(1)
            )),
            refusal::START_FAILED
        );
        assert_eq!(
            word(start(
                asked(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]),
                |_| None
            )),
            refusal::START_FAILED
        );
        assert!(matches!(
            start(asked(Vec::new()), |_| Some(1)),
            Err(Error::Invalid(_))
        ));
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_user_is_named_where_the_password_database_names_one() {
        // SAFETY: getuid takes nothing and cannot fail.
        let me = unsafe { libc::getuid() };
        let named = user_name(me);
        assert!(!named.is_empty() && named != me.to_string(), "{named}");
        assert_eq!(user_name(0), "root");
        assert_eq!(user_name(3_999_999_999), "3999999999");
    }
}
