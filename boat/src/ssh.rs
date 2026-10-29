//! How this computer reaches a host on Boat: OpenSSH, with a key of the adapter's own that
//! Boat lets into each host it reaches (`POST /sandboxes/{id}/sshkey`), to the address Boat
//! answers, the host's key pinned as Boat answers it. A channel is that `ssh` running the
//! runtime's command; a file is written by one, carrying its bytes on standard input.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use crate::api::Ssh;
use crate::image::quote;

/// The adapter's key pair, in its folder in the runtime's config folder.
#[derive(Debug, Clone)]
pub struct Keys {
    dir: PathBuf,
    private: PathBuf,
    /// The public key, in OpenSSH format, as Boat takes it.
    pub public: String,
}

impl Keys {
    /// The key pair in `dir`, made there with `ssh-keygen` where there is none.
    pub fn at(dir: &Path) -> Result<Self, String> {
        private_dir(dir)?;
        let private = dir.join("id_ed25519");
        let public = dir.join("id_ed25519.pub");
        if !private.exists() || !public.exists() {
            let _ = std::fs::remove_file(&private);
            let _ = std::fs::remove_file(&public);
            let made = std::process::Command::new("ssh-keygen")
                .args([
                    "-q",
                    "-t",
                    "ed25519",
                    "-N",
                    "",
                    "-C",
                    "steery-adapter-boat",
                    "-f",
                ])
                .arg(&private)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("ssh-keygen did not run: {e}"))?;
            if !made.status.success() {
                return Err(format!(
                    "ssh-keygen could not make the adapter's key: {}",
                    String::from_utf8_lossy(&made.stderr).trim()
                ));
            }
        }
        let public = std::fs::read_to_string(&public)
            .map_err(|e| {
                format!(
                    "the adapter's public key {} does not read: {e}",
                    public.display()
                )
            })?
            .trim()
            .to_owned();
        Ok(Self {
            dir: dir.to_owned(),
            private,
            public,
        })
    }

    /// Pins the host key Boat answered for host `id`, in a file of that host's own, and
    /// answers the command that runs `command` in it over `ssh`.
    pub fn argv(&self, id: &str, reach: &Ssh, command: &[String]) -> Result<Vec<String>, String> {
        let (host, port) = address(reach)?;
        let known = self.dir.join("known_hosts");
        private_dir(&known)?;
        let file = known.join(file_name(id));
        let named = if port == 22 {
            host.clone()
        } else {
            format!("[{host}]:{port}")
        };
        std::fs::write(&file, format!("{named} {}\n", reach.host_key.trim())).map_err(|e| {
            format!(
                "the host key of {id} was not written to {}: {e}",
                file.display()
            )
        })?;
        let mut argv: Vec<String> = vec!["ssh".into(), "-T".into(), "-i".into()];
        argv.push(self.private.display().to_string());
        for option in [
            "IdentitiesOnly=yes".to_owned(),
            "BatchMode=yes".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            format!("UserKnownHostsFile={}", file.display()),
            "ConnectTimeout=20".to_owned(),
            "LogLevel=ERROR".to_owned(),
        ] {
            argv.extend(["-o".to_owned(), option]);
        }
        argv.extend(["-p".to_owned(), port.to_string()]);
        argv.push(format!("{}@{host}", reach.ssh_user));
        argv.push(
            command
                .iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
        Ok(argv)
    }
}

/// The host and port to reach: the forwarded IPv4 endpoint where Boat names one, else the
/// machine's own address on port 22.
fn address(reach: &Ssh) -> Result<(String, u16), String> {
    if let Some(endpoint) = reach.ssh_endpoint.as_deref().filter(|e| !e.is_empty()) {
        let (host, port) = endpoint
            .rsplit_once(':')
            .ok_or_else(|| format!("Boat's SSH endpoint {endpoint} names no port"))?;
        let port = port
            .parse()
            .map_err(|_| format!("Boat's SSH endpoint {endpoint} names no port"))?;
        return Ok((host.trim_matches(['[', ']']).to_owned(), port));
    }
    match reach.machine_ip.as_deref().filter(|ip| !ip.is_empty()) {
        Some(ip) => Ok((ip.to_owned(), 22)),
        None => Err("Boat named no address for the host's SSH server: is it running?".to_owned()),
    }
}

/// A host id as a file name: its letters, digits, `-` and `_`.
fn file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `dir`, made where missing, and kept to its owner (0700).
fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{} was not made: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{} was not kept private: {e}", dir.display()))
}

/// The command that writes `length` bytes from its standard input to `path` with `mode`:
/// into a file beside it, which becomes `path` only once every byte is there, and is
/// removed otherwise.
pub fn writing(path: &str, length: usize, mode: u32) -> Vec<String> {
    let script = format!(
        r#"set -eu
p={path}
t="$p.steery-put.$$"
mkdir -p "$(dirname "$p")"
trap 'rm -f "$t"' EXIT
cat > "$t"
n=$(wc -c < "$t" | tr -d ' ')
if [ "$n" != {length} ]; then echo "steery-put: $n of {length} bytes arrived" >&2; exit 3; fi
chmod {mode:o} "$t"
mv -f "$t" "$p"
trap - EXIT
"#,
        path = quote(path),
    );
    vec!["sh".to_owned(), "-c".to_owned(), script]
}

/// Runs `argv` here with `input` on its standard input, and waits `within` for it to end:
/// its standard error's last line where it fails.
pub async fn run(argv: &[String], input: Vec<u8>, within: Duration) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no command")?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{program} did not start: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no standard input")?;
    let ran = async {
        // A command that ends early closes its input: what it said is the answer.
        let _ = stdin.write_all(&input).await;
        drop(stdin);
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(within, ran)
        .await
        .map_err(|_| format!("no end within {} s", within.as_secs()))?
        .map_err(|e| format!("{program} did not end: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let last = said
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    Err(match out.status.code() {
        Some(code) => format!("exit {code}: {}", last.trim()),
        None => format!("ended by a signal: {}", last.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(endpoint: Option<&str>, ip: Option<&str>) -> Ssh {
        Ssh {
            ssh_user: "user".into(),
            machine_ip: ip.map(str::to_owned),
            ssh_endpoint: endpoint.map(str::to_owned),
            host_key: "ssh-ed25519 AAAAkey".into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("steery-boat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn reaches_the_forwarded_endpoint_where_boat_names_one_else_the_machine() {
        assert_eq!(
            address(&reach(Some("51.254.44.205:19036"), Some("2001:db8::24"))).unwrap(),
            ("51.254.44.205".into(), 19036)
        );
        assert_eq!(
            address(&reach(None, Some("2001:db8::24"))).unwrap(),
            ("2001:db8::24".into(), 22)
        );
        assert!(address(&reach(None, None)).is_err());
        assert!(address(&reach(Some("51.254.44.205"), None)).is_err());
    }

    /// The channel's `ssh` takes the adapter's key only, never asks anything, and trusts
    /// the host key Boat answered and no other: pinned in that host's own file.
    #[test]
    fn a_channel_pins_the_host_key_boat_answered() {
        let dir = scratch("argv");
        let keys = Keys {
            dir: dir.clone(),
            private: dir.join("id_ed25519"),
            public: String::new(),
        };
        let command = [
            "/home/user/.local/bin/steery".to_owned(),
            "host".into(),
            "attach".into(),
            "it's".into(),
        ];
        let argv = keys
            .argv("bx_ab/1", &reach(Some("1.2.3.4:2200"), None), &command)
            .unwrap();
        let file = dir.join("known_hosts").join("bx_ab_1");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[1.2.3.4]:2200 ssh-ed25519 AAAAkey\n"
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=yes"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w[1] == format!("UserKnownHostsFile={}", file.display()))
        );
        assert!(argv.windows(2).any(|w| w == ["-p", "2200"]));
        assert_eq!(
            &argv[argv.len() - 2..],
            [
                "user@1.2.3.4",
                r"/home/user/.local/bin/steery host attach 'it'\''s'"
            ]
        );
        keys.argv("bx_ab/1", &reach(None, Some("9.9.9.9")), &command)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "9.9.9.9 ssh-ed25519 AAAAkey\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A put's bytes become the file whole, with its mode, or not at all.
    #[tokio::test]
    async fn a_write_lands_whole_or_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("put");
        let path = dir.join("deep/in it/steery");
        let at = path.to_str().unwrap();
        let bytes = b"#!/bin/sh\necho hi\n".to_vec();
        run(
            &writing(at, bytes.len(), 0o755),
            bytes.clone(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        // Short of what was promised: refused, the old file kept, no partial one left.
        let why = run(
            &writing(at, 100, 0o644),
            b"short".to_vec(),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert_eq!(why, "exit 3: steery-put: 5 of 100 bytes arrived");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["steery"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn keys_are_made_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("keys");
        let made = Keys::at(&dir).unwrap();
        assert!(made.public.starts_with("ssh-ed25519 "), "{}", made.public);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(Keys::at(&dir).unwrap().public, made.public);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
