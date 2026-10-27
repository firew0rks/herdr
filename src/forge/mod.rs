//! Forge pool: one cached GitHub snapshot per repo, shared by every client.
//!
//! The server fetches each tracked repo through the `gh` CLI on its own
//! cadence and serves the result over `forge.snapshot`, so plugins and tools
//! stop polling GitHub separately. Off unless `[forge] enabled = true`.

mod gh;
mod gh_cache;
mod github;
mod pool;
#[cfg(all(test, unix))]
mod test_support;
#[cfg(all(test, unix))]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::api::schema::{
    EmptyParams, ErrorBody, ErrorResponse, Method, Request, ResponseResult, SuccessResponse,
};
use crate::api::{ApiRequestMessage, ApiRequestSender, EventHub};

pub(crate) use gh::{Gh, INNER_ENV_VAR};
pub(crate) use gh_cache::cacheable;

const TICK: Duration = Duration::from_secs(1);
const DISCOVER_EVERY: Duration = Duration::from_secs(15);
const REMOTE_TTL: Duration = Duration::from_secs(5 * 60);
const APP_TIMEOUT: Duration = Duration::from_secs(5);
const GH_TIMEOUT: Duration = Duration::from_secs(60);

struct Forge {
    pool: Arc<pool::Pool>,
    gh: Gh,
    gh_cache: gh_cache::GhCache,
    remotes: std::sync::Mutex<Remotes>,
}

static FORGE: OnceLock<Forge> = OnceLock::new();

/// Starts the pool when `[forge] enabled = true`. Later calls are no-ops.
pub(crate) fn start(
    config: &crate::config::ForgeConfig,
    api_tx: ApiRequestSender,
    events: EventHub,
) {
    if !config.enabled || FORGE.get().is_some() {
        return;
    }
    let gh = Gh::from_env();
    let program = gh.program.display().to_string();
    let store = crate::session::data_dir().join("forge");
    let forge = Forge {
        pool: pool::Pool::new(gh.clone(), Some(store), Some(events)),
        gh,
        gh_cache: gh_cache::GhCache::default(),
        remotes: std::sync::Mutex::default(),
    };
    if FORGE.set(forge).is_err() {
        return;
    }
    let spawned = crate::thread_spawn::spawn_named("herdr-forge", move || {
        let Some(forge) = FORGE.get() else {
            return;
        };
        let mut discovered: Option<Instant> = None;
        loop {
            if discovered.is_none_or(|at| at.elapsed() >= DISCOVER_EVERY) {
                discovered = Some(Instant::now());
                if let Some(paths) = workspace_paths(&api_tx) {
                    forge.pool.set_workspaces(forge.workspaces(&paths));
                }
            }
            forge.pool.tick();
            std::thread::sleep(TICK);
        }
    });
    match spawned {
        Ok(_) => info!(gh = %program, "forge pool started"),
        Err(err) => warn!(%err, "forge: could not start the scheduler thread"),
    }
}

/// Answers a `forge.*` request. Runs on the API connection thread, so blocking
/// on a fetch never stalls the app loop.
pub(crate) fn handle(id: String, method: Method) -> String {
    let Some(forge) = FORGE.get() else {
        return error(
            id,
            "forge_disabled",
            "forge is disabled; set [forge] enabled = true and restart the server",
        );
    };
    match method {
        Method::ForgeSnapshot(params) => match forge.repo(params.repo, params.cwd) {
            Ok(repo) => snapshot_response(id, forge.pool.snapshot(&repo)),
            Err(message) => error(id, "invalid_repo", message),
        },
        Method::ForgeRefresh(params) => match forge.repo(params.repo, params.cwd) {
            Ok(repo) => snapshot_response(id, forge.pool.refresh(&repo, params.pr)),
            Err(message) => error(id, "invalid_repo", message),
        },
        Method::ForgeGh(params) => {
            if !cacheable(&params.argv) {
                return error(
                    id,
                    "not_cacheable",
                    "only read-only gh commands go through the forge cache; run gh directly",
                );
            }
            let cwd = params.cwd.map(PathBuf::from);
            let key = gh_cache::Key {
                scope: forge.scope(cwd.as_deref()),
                argv: params.argv.clone(),
            };
            let (output, cached) = forge.gh_cache.get_or_run(key, || {
                forge
                    .gh
                    .run(&params.argv, cwd.as_deref(), GH_TIMEOUT)
                    .unwrap_or_else(|err| gh::Output {
                        stdout: String::new(),
                        stderr: format!("herdr forge: {err}\n"),
                        code: 1,
                    })
            });
            success(
                id,
                ResponseResult::ForgeGh {
                    stdout: output.stdout,
                    stderr: output.stderr,
                    exit_code: output.code,
                    cached,
                },
            )
        }
        _ => error(id, "invalid_request", "not a forge method"),
    }
}

impl Forge {
    fn repo(&self, repo: Option<String>, cwd: Option<String>) -> Result<String, String> {
        let repo = match repo {
            Some(repo) => repo,
            None => cwd.and_then(|cwd| self.origin(Path::new(&cwd))).ok_or(
                "no repo given and the cwd has no GitHub origin remote; pass --repo owner/name",
            )?,
        };
        if pool::valid_repo(&repo) {
            Ok(repo)
        } else {
            Err(format!("invalid repo {repo:?}; expected owner/name"))
        }
    }

    fn origin(&self, path: &Path) -> Option<String> {
        self.remotes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .origin(path)
    }

    fn scope(&self, cwd: Option<&Path>) -> String {
        let Some(cwd) = cwd else {
            return String::new();
        };
        let repo = self
            .origin(cwd)
            .unwrap_or_else(|| cwd.display().to_string());
        let branch = crate::workspace::git_branch(cwd).unwrap_or_default();
        format!("{repo}@{branch}")
    }

    fn workspaces(&self, paths: &BTreeSet<PathBuf>) -> BTreeMap<String, BTreeSet<String>> {
        let mut repos: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in paths {
            let Some(repo) = self.origin(path) else {
                continue;
            };
            let branches = repos.entry(repo).or_default();
            if let Some(branch) = crate::workspace::git_branch(path) {
                branches.insert(branch);
            }
        }
        repos
    }
}

/// `origin` remotes by checkout path, so discovery doesn't spawn git for every
/// pane on every pass.
#[derive(Default)]
struct Remotes {
    by_path: HashMap<PathBuf, (Instant, Option<String>)>,
}

impl Remotes {
    fn origin(&mut self, path: &Path) -> Option<String> {
        if let Some((at, repo)) = self.by_path.get(path) {
            if at.elapsed() < REMOTE_TTL {
                return repo.clone();
            }
        }
        let output = crate::noninteractive_process::command("git")
            .arg("-C")
            .arg(path)
            .args(["remote", "get-url", "origin"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let repo = output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| github_repo(String::from_utf8_lossy(&output.stdout).trim()));
        self.by_path
            .insert(path.to_path_buf(), (Instant::now(), repo.clone()));
        repo
    }
}

/// `owner/name` from a github.com remote URL in any of git's spellings.
pub(crate) fn github_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    pool::valid_repo(rest).then(|| rest.to_string())
}

/// Checkout paths of every open workspace, read from the app's session snapshot.
fn workspace_paths(api_tx: &ApiRequestSender) -> Option<BTreeSet<PathBuf>> {
    let (respond_to, response) = std::sync::mpsc::channel();
    api_tx
        .send(ApiRequestMessage {
            request: Request {
                id: "forge:discover".into(),
                method: Method::SessionSnapshot(EmptyParams::default()),
            },
            respond_to,
            response_write_complete: None,
        })
        .ok()?;
    let text = response.recv_timeout(APP_TIMEOUT).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(snapshot_paths(&value["result"]["snapshot"]))
}

fn snapshot_paths(snapshot: &serde_json::Value) -> BTreeSet<PathBuf> {
    let checkouts = snapshot["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|workspace| workspace["worktree"]["checkout_path"].as_str());
    let cwds = snapshot["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| pane["cwd"].as_str());
    checkouts.chain(cwds).map(PathBuf::from).collect()
}

fn snapshot_response(id: String, snapshot: crate::api::schema::ForgeSnapshot) -> String {
    success(
        id,
        ResponseResult::ForgeSnapshot {
            snapshot: Box::new(snapshot),
        },
    )
}

fn success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_default()
}

fn error(id: String, code: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    })
    .unwrap_or_default()
}
