use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// Points at the real `gh` when a `gh` shim sits earlier on PATH.
const GH_ENV_VAR: &str = "HERDR_FORGE_GH";
/// Set on every `gh` the forge runs, so a shim can tell it must not call back in.
pub(crate) const INNER_ENV_VAR: &str = "HERDR_FORGE_INNER";

const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Directories searched after PATH: the server starts with a minimal PATH that
/// rarely includes where package managers put `gh`.
fn fallback_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/opt/homebrew/bin")];
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

pub(crate) fn search_path(path: Option<&OsStr>, home: Option<&Path>) -> OsString {
    let mut dirs: Vec<PathBuf> = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    for dir in fallback_dirs(home) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

pub(crate) fn resolve_in(search: &OsStr) -> Option<PathBuf> {
    let name = format!("gh{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(search)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// The `gh` binary and the PATH it runs with.
#[derive(Debug, Clone)]
pub(crate) struct Gh {
    pub program: PathBuf,
    pub path: OsString,
}

impl Gh {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = search_path(std::env::var_os("PATH").as_deref(), home.as_deref());
        let program = std::env::var_os(GH_ENV_VAR)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| resolve_in(&path))
            .unwrap_or_else(|| PathBuf::from("gh"));
        Self { program, path }
    }

    pub fn run(
        &self,
        args: &[String],
        cwd: Option<&Path>,
        timeout: Duration,
    ) -> io::Result<Output> {
        let mut command = crate::noninteractive_process::command(&self.program);
        command
            .args(args)
            .env("PATH", &self.path)
            .env(INNER_ENV_VAR, "1")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let readers = drain(child.stdout.take())
            .and_then(|stdout| drain(child.stderr.take()).map(|stderr| (stdout, stderr)));
        let (stdout, stderr) = match readers {
            Ok(readers) => readers,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        };
        let code = wait(&mut child, timeout)?;
        Ok(Output {
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
            code,
        })
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> io::Result<std::thread::JoinHandle<String>> {
    // Reading on our own thread keeps a chatty child from blocking on a full pipe.
    crate::thread_spawn::spawn_named("herdr-forge-pipe", move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn wait(child: &mut Child, timeout: Duration) -> io::Result<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(-1));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("gh timed out after {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_path_appends_fallbacks_once() {
        let path = std::env::join_paths(["/usr/bin", "/usr/local/bin"]).unwrap();
        let joined = search_path(Some(&path), Some(Path::new("/home/me")));
        let dirs: Vec<_> = std::env::split_paths(&joined).collect();
        assert_eq!(
            dirs,
            [
                "/usr/bin",
                "/usr/local/bin",
                "/opt/homebrew/bin",
                "/home/me/.local/bin"
            ]
            .map(PathBuf::from)
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_gh_from_the_home_fallback() {
        let bin = super::super::test_support::fake_gh("#!/bin/sh\nsleep 5\n");
        let home = bin.parent().unwrap().parent().unwrap();
        let search = search_path(Some(OsStr::new("/nonexistent")), Some(home));
        assert_eq!(resolve_in(&search), Some(bin.join("gh")));
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_gh_is_killed_at_the_timeout() {
        let bin = super::super::test_support::fake_gh("#!/bin/sh\nsleep 5\n");
        let gh = super::super::test_support::gh_in(&bin);
        let err = gh.run(&[], None, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
