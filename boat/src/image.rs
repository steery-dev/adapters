//! A Dockerfile built the way Boat can build one. Boat starts every sandbox from its own
//! Ubuntu image, or from a snapshot of one, and pulls no image; so a Dockerfile's steps are
//! run, in order, in a fresh sandbox, as the setup script Boat runs once it is ready, and
//! that sandbox, stopped, is the template every host of that text is forked from. The same
//! bytes make the same template, named from their digest, built once.
//!
//! What a step does on a sandbox's disk is carried: `RUN` (shell and exec form), `USER`
//! (`root` runs under `sudo`; any other user is the sandbox's own, `user`), `ENV`, `ARG`
//! and `WORKDIR`. `FROM` names an image Boat cannot start from, so the sandbox's own is
//! used whatever it names, and a second `FROM` (a multi-stage build) is refused, as are
//! `COPY` and `ADD`: Boat is given no build context. What only a container's start reads
//! (`LABEL`, `CMD`, `ENTRYPOINT`, `EXPOSE`, `VOLUME`, `STOPSIGNAL`, `HEALTHCHECK`, `SHELL`,
//! `ONBUILD`, `MAINTAINER`) changes no disk, and is read only into the digest.

use sha2::{Digest, Sha256};

/// The prefix of the name of every template this adapter keeps Steery's image in.
pub const PREFIX: &str = "steery-image-";

/// A Dockerfile as Boat builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The name of the template it is built in: [`PREFIX`] and [`Self::digest`].
    pub name: String,
    /// The first 64 bits of the text's SHA-256, in hex.
    pub digest: String,
    /// The setup script that runs its steps.
    pub script: String,
}

/// Who a step runs as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum As {
    User,
    Root,
}

/// The image `text` builds, or why Boat cannot build it.
pub fn of(text: &str) -> Result<Image, String> {
    let digest: String = Sha256::digest(text.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut steps: Vec<String> = Vec::new();
    let mut runs_as = As::User;
    let mut workdir: Option<String> = None;
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut from = false;
    for (number, line) in instructions(text) {
        let (word, rest) = match line.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (line.as_str(), ""),
        };
        let at = |why: &str| format!("Dockerfile line {number}: {why}");
        match word.to_ascii_uppercase().as_str() {
            "FROM" if from => {
                return Err(at(
                    "a second FROM: Boat builds one stage, so a multi-stage build is not built",
                ));
            }
            "FROM" => from = true,
            _ if !from => return Err(at("a step before FROM")),
            "RUN" => {
                let command = if rest.starts_with('[') {
                    let argv: Vec<String> = serde_json::from_str(rest)
                        .map_err(|_| at("RUN's exec form is not a JSON array of strings"))?;
                    if argv.is_empty() {
                        return Err(at("RUN names no command"));
                    }
                    argv.iter()
                        .map(|arg| quote(arg))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else if rest.starts_with("--") {
                    return Err(at(
                        "RUN's flags (--mount, --network) are BuildKit's, not Boat's",
                    ));
                } else if rest.is_empty() {
                    return Err(at("RUN names no command"));
                } else {
                    format!("/bin/sh -c {}", quote(rest))
                };
                // In double quotes, so `$NAME` and `${NAME:-…}` are read as Docker reads them
                // when the step runs; a command substitution is not.
                let env: String = vars
                    .iter()
                    .map(|(name, value)| format!(" \"{name}={}\"", in_double_quotes(value)))
                    .collect();
                let run = match runs_as {
                    As::User => format!("env{env} {command}"),
                    As::Root => format!("sudo -H env{env} {command}"),
                };
                let within = workdir
                    .as_deref()
                    .map_or_else(|| "\"$HOME\"".to_owned(), quote);
                steps.push(format!("step {number} {within} {}", quote(&run)));
            }
            "USER" => {
                let user = rest.split(':').next().unwrap_or_default();
                if user.is_empty() {
                    return Err(at("USER names nobody"));
                }
                runs_as = if matches!(user, "root" | "0") {
                    As::Root
                } else {
                    As::User
                };
            }
            "WORKDIR" => {
                if rest.is_empty() {
                    return Err(at("WORKDIR names no folder"));
                }
                let dir = match (&workdir, rest.starts_with('/')) {
                    (Some(base), false) => format!("{}/{rest}", base.trim_end_matches('/')),
                    _ => rest.to_owned(),
                };
                let make = format!("[ -d {0} ] || sudo -H mkdir -p {0}", quote(&dir));
                steps.push(format!("step {number} / {}", quote(&make)));
                workdir = Some(dir);
            }
            "ENV" => vars.extend(env_pairs(rest).map_err(|why| at(&why))?),
            "ARG" => {
                let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
                if !is_name(name) {
                    return Err(at("ARG names no variable"));
                }
                if !value.is_empty() || !vars.iter().any(|(had, _)| had == name) {
                    vars.push((name.to_owned(), unquoted(value)));
                }
            }
            "COPY" | "ADD" => {
                return Err(at(
                    "COPY and ADD need a build context, and Boat is given none: fetch the files in a RUN",
                ));
            }
            "LABEL" | "CMD" | "ENTRYPOINT" | "EXPOSE" | "VOLUME" | "STOPSIGNAL" | "HEALTHCHECK"
            | "SHELL" | "ONBUILD" | "MAINTAINER" => {}
            other => return Err(at(&format!("{other} is not a Dockerfile instruction"))),
        }
    }
    if !from {
        return Err("the Dockerfile has no FROM".to_owned());
    }
    let mut script = format!(
        "# Steery's image {digest}, as steery-adapter-boat builds a Dockerfile on Boat.\n\
         set -eu\n\
         step() {{\n  \
           printf 'steery-image: line %s\\n' \"$1\"\n  \
           (cd \"$2\" && eval \"$3\") || {{ code=$?; printf 'steery-image: Dockerfile line %s failed with exit %s\\n' \"$1\" \"$code\" >&2; exit \"$code\"; }}\n\
         }}\n"
    );
    for step in &steps {
        script.push_str(step);
        script.push('\n');
    }
    script.push_str("printf 'steery-image: built\\n'\n");
    Ok(Image {
        name: format!("{PREFIX}{digest}"),
        digest,
        script,
    })
}

/// The Dockerfile's instructions with the line each starts on: continuation lines joined
/// (a `\` at a line's end, as Docker removes it with the line break), comments and blank
/// lines skipped, also within a continuation.
fn instructions(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (at, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || (trimmed.is_empty() && open.is_none()) {
            continue;
        }
        let (body, goes_on) = match line.trim_end().strip_suffix('\\') {
            Some(body) => (body, true),
            None => (line, false),
        };
        match &mut open {
            Some((_, joined)) => joined.push_str(body),
            None => open = Some((at + 1, body.trim_start().to_owned())),
        }
        if !goes_on {
            found.extend(
                open.take()
                    .map(|(number, joined)| (number, joined.trim_end().to_owned())),
            );
        }
    }
    found.extend(open.map(|(number, joined)| (number, joined.trim_end().to_owned())));
    found
}

/// `ENV`'s pairs: `NAME=VALUE …`, a value in double quotes where it holds spaces, or the
/// old form, `NAME VALUE`.
fn env_pairs(rest: &str) -> Result<Vec<(String, String)>, String> {
    let first = rest.split_whitespace().next().unwrap_or_default();
    if !first.contains('=') {
        let (name, value) = rest
            .split_once(char::is_whitespace)
            .ok_or("ENV names a variable and no value")?;
        if !is_name(name) {
            return Err("ENV names no variable".to_owned());
        }
        return Ok(vec![(name.to_owned(), value.trim().to_owned())]);
    }
    let mut pairs = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Ok(pairs);
        }
        let name: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if !is_name(&name) {
            return Err("ENV's pair names no variable".to_owned());
        }
        let mut value = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() && !quoted {
                break;
            }
            chars.next();
            match c {
                '"' => quoted = !quoted,
                '\\' => value.extend(chars.next()),
                _ => value.push(c),
            }
        }
        if quoted {
            return Err("ENV's value opens a quote it never closes".to_owned());
        }
        pairs.push((name, value));
    }
}

/// A value with its surrounding double quotes taken off.
fn unquoted(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
        .to_owned()
}

/// `value` for the inside of a POSIX shell's double quotes: `$NAME` and `${…}` still
/// expand, and nothing else does (`"`, `\`, a backtick and `$(` are taken as written).
fn in_double_quotes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\\' | '`' => {
                out.push('\\');
                out.push(c);
            }
            '$' if chars.peek() == Some(&'(') => out.push_str("\\$"),
            _ => out.push(c),
        }
    }
    out
}

/// Letters, digits and `_`, not starting with a digit.
fn is_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// One argument as a POSIX shell reads it back.
pub fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_/.:=@%+,-".contains(&byte));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steery's own image, as the runtime sends it (crates/steery/src/machine/image.Dockerfile
    /// and its release label).
    const STEERY: &str = r#"FROM daytonaio/sandbox:0.8.0
USER daytona
RUN curl -fsSL https://claude.ai/install.sh | bash
RUN curl -fsSL https://omp.sh/install | sh -s -- --binary
USER root
RUN case "$(uname -m)" in \
      x86_64|amd64) arch=amd64 ;; \
      aarch64|arm64) arch=aarch64 ;; \
      *) echo "cli-proxy-api has no build for $(uname -m)" >&2; exit 1 ;; \
    esac \
 && curl -fsSL "https://github.com/router-for-me/CLIProxyAPI/releases/download/v8.0.30/CLIProxyAPI_8.0.30_linux_${arch}.tar.gz" \
    | tar xz -C /usr/local/bin cli-proxy-api
USER daytona
LABEL dev.steery.release="7.0.171"
"#;

    /// The script, run by `bash` as Boat runs a setup script, in a home of its own: what
    /// its steps printed, on standard output then standard error.
    fn ran(image: &Image) -> String {
        let dir = std::env::temp_dir().join(format!(
            "steery-boat-image-{}-{}",
            std::process::id(),
            image.digest
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("setup.sh");
        std::fs::write(&script, &image.script).unwrap();
        let out = std::process::Command::new("bash")
            .arg(&script)
            .env("HOME", &dir)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", dir.join("bin").display()),
            )
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    #[test]
    fn steerys_image_builds_each_run_as_its_user_and_names_its_digest() {
        let image = of(STEERY).unwrap();
        assert_eq!(image.name, format!("{PREFIX}{}", image.digest));
        assert_eq!(image.digest.len(), 16);
        let lines: Vec<&str> = image
            .script
            .lines()
            .filter(|line| line.starts_with("step "))
            .collect();
        assert_eq!(lines.len(), 3, "{}", image.script);
        assert!(
            lines[0].starts_with("step 3 \"$HOME\" 'env /bin/sh -c '"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("step 4 \"$HOME\" 'env /bin/sh -c '"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("step 6 \"$HOME\" 'sudo -H env /bin/sh -c '"),
            "{}",
            lines[2]
        );
        // The continuation is one command, as Docker joins it.
        assert!(lines[2].contains("esac  && curl -fsSL"), "{}", lines[2]);
        // Another release is another image.
        assert_ne!(
            of(&STEERY.replace("7.0.171", "7.0.172")).unwrap().name,
            image.name
        );
    }

    /// Each step runs in `bash` as the setup script does, its command reaching `/bin/sh -c`
    /// whole: quotes, `$(…)` and pipes are the step's own, never the script's; an `ENV`
    /// value reads `$NAME` as Docker does, and nothing else in it runs.
    #[test]
    fn a_step_reaches_its_shell_as_written() {
        let image = of(concat!(
            "FROM x\n",
            "ENV GREETING=\"hello there\" B=2 WHERE=$HOME/w TICK=`id`$(id)\n",
            "RUN echo \"$GREETING\" $B \"$WHERE\" \"$TICK\" '$(not run)' | tr a-z A-Z\n",
            "WORKDIR /tmp\n",
            "RUN [\"sh\", \"-c\", \"echo in $PWD\"]\n",
        ))
        .unwrap();
        let said = ran(&image);
        assert!(said.contains("HELLO THERE 2 /"), "{said}");
        assert!(said.contains("/W `ID`$(ID) $(NOT RUN)\n"), "{said}");
        assert!(said.contains("\nin /tmp\n"), "{said}");
        assert!(said.ends_with("steery-image: built\n"), "{said}");
    }

    #[test]
    fn a_failing_step_stops_the_build_and_names_its_line() {
        let said = ran(&of("FROM x\nRUN true\nRUN exit 7\nRUN echo never\n").unwrap());
        assert!(
            said.contains("steery-image: Dockerfile line 3 failed with exit 7"),
            "{said}"
        );
        assert!(!said.contains("never") && !said.contains("built"), "{said}");
    }

    #[test]
    fn what_boat_cannot_build_is_refused_by_its_line() {
        let refused = |text: &str| of(text).unwrap_err();
        assert!(refused("FROM a\nCOPY . /app\n").starts_with("Dockerfile line 2: COPY and ADD"));
        assert!(refused("FROM a AS b\nRUN x\nFROM c\n").contains("line 3: a second FROM"));
        assert!(refused("RUN x\n").contains("line 1: a step before FROM"));
        assert!(refused("FROM a\nRUN --mount=type=cache,target=/x make\n").contains("BuildKit"));
        assert!(refused("FROM a\nFROB x\n").contains("FROB is not a Dockerfile instruction"));
        assert_eq!(refused("# nothing\n"), "the Dockerfile has no FROM");
    }

    #[test]
    fn env_reads_both_its_forms() {
        assert_eq!(
            env_pairs(r#"A=1 B="two words" C=a\ b"#).unwrap(),
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "a b".into())
            ]
        );
        assert_eq!(
            env_pairs("PATH_X /a b").unwrap(),
            vec![("PATH_X".into(), "/a b".into())]
        );
        assert!(env_pairs("A=\"open").is_err());
    }
}
