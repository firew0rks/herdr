use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tracing::warn;

use super::gh::Gh;
use super::github::{self, Plan};
use crate::api::schema::{EventData, EventEnvelope, EventKind, ForgeSnapshot};
use crate::api::EventHub;

const BASE: Duration = Duration::from_secs(30);
const BUSY: Duration = Duration::from_secs(15);
/// How long an ask keeps a repo with no open workspace tracked, and how long a
/// PR named by `forge.refresh --pr` keeps its checks fetched.
pub(crate) const INTEREST: Duration = Duration::from_secs(30 * 60);
const WORKFLOWS_EVERY: Duration = Duration::from_secs(10 * 60);
const FIRST_FETCH_WAIT: Duration = Duration::from_secs(60);
const REFRESH_WAIT: Duration = Duration::from_secs(120);

/// Fetch interval: 15s while a watched PR is settling, otherwise 30s, widened
/// once no client has asked for the repo in a while.
pub(crate) fn interval(busy: bool, since_asked: Duration) -> Duration {
    let base = if busy { BUSY } else { BASE };
    let widen = if since_asked >= Duration::from_secs(30 * 60) {
        15
    } else if since_asked >= Duration::from_secs(5 * 60) {
        4
    } else {
        1
    };
    base * widen
}

/// Unknown mergeability only counts on PRs we fetch checks for: GitHub leaves
/// it UNKNOWN on PRs nobody has opened lately, which would pin a busy repo at
/// the fast interval forever.
pub(crate) fn busy(snapshot: &ForgeSnapshot) -> bool {
    snapshot.prs.iter().any(|pr| {
        pr.checks.as_ref().is_some_and(|checks| {
            matches!(checks.state.as_deref(), Some("PENDING" | "EXPECTED"))
                || pr.mergeable == "UNKNOWN"
        })
    })
}

pub(crate) fn valid_repo(repo: &str) -> bool {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| valid_part(owner) && valid_part(name))
}

fn empty(repo: &str) -> ForgeSnapshot {
    ForgeSnapshot {
        repo: repo.into(),
        fetched_at: None,
        stale: false,
        error: None,
        viewer: None,
        prs: Vec::new(),
        runs: Vec::new(),
        workflows: Vec::new(),
    }
}

fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.replace_nanosecond(0)
        .unwrap_or(now)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn same_data(a: &ForgeSnapshot, b: &ForgeSnapshot) -> bool {
    ForgeSnapshot {
        fetched_at: None,
        ..a.clone()
    } == ForgeSnapshot {
        fetched_at: None,
        ..b.clone()
    }
}

#[derive(Default)]
struct Repo {
    snapshot: Option<ForgeSnapshot>,
    in_flight: bool,
    started: u64,
    finished: u64,
    /// The fetch number a refresh is waiting for; a fetch that ends below it
    /// started before the refresh asked, so another one runs.
    want: u64,
    asked: Option<Instant>,
    in_workspace: bool,
    branches: BTreeSet<String>,
    watched: HashMap<u64, Instant>,
    fetched: Option<Instant>,
    due: Option<Instant>,
    workflows_at: Option<Instant>,
}

impl Repo {
    fn wanted(&self, now: Instant) -> bool {
        self.in_workspace
            || self
                .asked
                .is_some_and(|asked| now.duration_since(asked) < INTEREST)
    }
}

#[derive(Default)]
struct Inner {
    repos: HashMap<String, Repo>,
    viewer: Option<String>,
}

pub(crate) struct Pool {
    gh: Gh,
    store: Option<PathBuf>,
    events: Option<EventHub>,
    started_at: Instant,
    inner: Mutex<Inner>,
    done: Condvar,
}

impl Pool {
    pub fn new(gh: Gh, store: Option<PathBuf>, events: Option<EventHub>) -> Arc<Self> {
        let pool = Arc::new(Self {
            gh,
            store,
            events,
            started_at: Instant::now(),
            inner: Mutex::default(),
            done: Condvar::new(),
        });
        pool.load();
        pool
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn load(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(store) else {
            return;
        };
        let mut inner = self.lock();
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_str::<ForgeSnapshot>(&text) else {
                continue;
            };
            if !valid_repo(&snapshot.repo) {
                continue;
            }
            if inner.viewer.is_none() {
                inner.viewer.clone_from(&snapshot.viewer);
            }
            let repo = snapshot.repo.clone();
            inner.repos.entry(repo).or_default().snapshot = Some(snapshot);
        }
    }

    fn persist(&self, snapshot: &ForgeSnapshot) {
        let Some(store) = &self.store else {
            return;
        };
        let file = store.join(format!("{}.json", snapshot.repo.replace('/', "__")));
        let tmp = file.with_extension("json.tmp");
        let result = std::fs::create_dir_all(store)
            .and_then(|()| {
                let json = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
                std::fs::write(&tmp, json)
            })
            .and_then(|()| std::fs::rename(&tmp, &file));
        if let Err(err) = result {
            warn!(repo = %snapshot.repo, %err, "forge: failed to persist snapshot");
        }
    }

    /// The repo's current snapshot. The first ask for a repo with nothing
    /// cached waits for its first fetch.
    pub fn snapshot(self: &Arc<Self>, repo: &str) -> ForgeSnapshot {
        let now = Instant::now();
        let mut inner = self.lock();
        let entry = inner.repos.entry(repo.into()).or_default();
        entry.asked = Some(now);
        // An idle repo's next fetch may be minutes out; being asked again
        // brings it back to the normal interval.
        if let (Some(fetched), Some(snapshot)) = (entry.fetched, &entry.snapshot) {
            let soon = fetched + interval(busy(snapshot), Duration::ZERO);
            entry.due = Some(entry.due.map_or(soon, |due| due.min(soon)));
        }
        if let Some(snapshot) = &entry.snapshot {
            return snapshot.clone();
        }
        if !entry.in_flight {
            self.start_fetch(&mut inner, repo);
        }
        let target = inner.repos[repo].started;
        self.wait_for(inner, repo, target, FIRST_FETCH_WAIT)
    }

    /// Fetches now, joining a fetch only if it started after this call, and
    /// returns the result.
    pub fn refresh(self: &Arc<Self>, repo: &str, pr: Option<u64>) -> ForgeSnapshot {
        let now = Instant::now();
        let mut inner = self.lock();
        let entry = inner.repos.entry(repo.into()).or_default();
        entry.asked = Some(now);
        if let Some(pr) = pr {
            entry.watched.insert(pr, now);
        }
        let target = entry.started + 1;
        entry.want = entry.want.max(target);
        if !entry.in_flight {
            self.start_fetch(&mut inner, repo);
        }
        self.wait_for(inner, repo, target, REFRESH_WAIT)
    }

    fn wait_for(
        &self,
        inner: MutexGuard<'_, Inner>,
        repo: &str,
        target: u64,
        timeout: Duration,
    ) -> ForgeSnapshot {
        let (inner, _) = self
            .done
            .wait_timeout_while(inner, timeout, |inner| {
                inner.repos.get(repo).is_some_and(|r| r.finished < target)
            })
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = inner.repos.get(repo);
        match entry.and_then(|entry| entry.snapshot.clone()) {
            Some(snapshot) => snapshot,
            None => ForgeSnapshot {
                error: Some("timed out waiting for the first fetch".into()),
                stale: true,
                ..empty(repo)
            },
        }
    }

    /// Repos with an open workspace, each with the branches checked out in it.
    pub fn set_workspaces(&self, workspaces: BTreeMap<String, BTreeSet<String>>) {
        let mut inner = self.lock();
        for (repo, entry) in inner.repos.iter_mut() {
            if !workspaces.contains_key(repo) {
                entry.in_workspace = false;
                entry.branches.clear();
            }
        }
        for (repo, branches) in workspaces {
            let entry = inner.repos.entry(repo).or_default();
            entry.in_workspace = true;
            entry.branches = branches;
        }
    }

    /// Starts every fetch that is due.
    pub fn tick(self: &Arc<Self>) {
        let now = Instant::now();
        let mut inner = self.lock();
        let due: Vec<String> = inner
            .repos
            .iter()
            .filter(|(_, r)| r.wanted(now) && !r.in_flight && r.due.is_none_or(|due| due <= now))
            .map(|(repo, _)| repo.clone())
            .collect();
        for repo in due {
            self.start_fetch(&mut inner, &repo);
        }
    }

    fn start_fetch(self: &Arc<Self>, inner: &mut Inner, repo: &str) {
        let Some(entry) = inner.repos.get_mut(repo) else {
            return;
        };
        entry.in_flight = true;
        entry.started += 1;
        let pool = Arc::clone(self);
        let name = repo.to_string();
        if let Err(err) =
            crate::thread_spawn::spawn_named("herdr-forge-fetch", move || pool.run_fetch(&name))
        {
            warn!(%repo, %err, "forge: could not start fetch thread");
            entry.in_flight = false;
            entry.finished = entry.started;
            entry.due = Some(Instant::now() + BASE);
            self.done.notify_all();
        }
    }

    fn run_fetch(self: &Arc<Self>, repo: &str) {
        let plan = {
            let now = Instant::now();
            let mut inner = self.lock();
            let viewer = inner.viewer.clone();
            let entry = inner.repos.entry(repo.into()).or_default();
            entry
                .watched
                .retain(|_, at| now.duration_since(*at) < INTEREST);
            Plan {
                repo: repo.into(),
                viewer,
                check_branches: entry.branches.clone(),
                check_prs: entry.watched.keys().copied().collect(),
                workflows: entry
                    .workflows_at
                    .is_none_or(|at| now.duration_since(at) >= WORKFLOWS_EVERY),
            }
        };
        let result = github::fetch(&self.gh, &plan);
        self.finish_fetch(repo, plan.workflows, result);
    }

    fn finish_fetch(
        self: &Arc<Self>,
        repo: &str,
        asked_workflows: bool,
        result: Result<github::Fetched, String>,
    ) {
        let now = Instant::now();
        let mut inner = self.lock();
        if let Ok(fetched) = &result {
            inner.viewer = Some(fetched.viewer.clone());
        }
        let entry = inner.repos.entry(repo.into()).or_default();
        let old = entry.snapshot.take();
        let new = match result {
            Ok(fetched) => {
                if asked_workflows {
                    entry.workflows_at = Some(now);
                }
                entry.fetched = Some(now);
                ForgeSnapshot {
                    repo: repo.into(),
                    fetched_at: Some(now_rfc3339()),
                    stale: false,
                    error: None,
                    viewer: Some(fetched.viewer),
                    prs: fetched.prs,
                    runs: fetched.runs,
                    workflows: fetched.workflows.unwrap_or_else(|| {
                        old.as_ref()
                            .map(|old| old.workflows.clone())
                            .unwrap_or_default()
                    }),
                }
            }
            Err(error) => {
                warn!(%repo, %error, "forge: fetch failed; keeping the last snapshot");
                ForgeSnapshot {
                    stale: true,
                    error: Some(error),
                    ..old.clone().unwrap_or_else(|| empty(repo))
                }
            }
        };
        let changed = old.as_ref().is_none_or(|old| !same_data(old, &new));
        let since_asked = now.duration_since(entry.asked.unwrap_or(self.started_at));
        entry.due = Some(now + interval(busy(&new), since_asked));
        let persist = (!new.stale).then(|| new.clone());
        let event = changed.then(|| EventEnvelope {
            event: EventKind::ForgeUpdated,
            data: EventData::ForgeUpdated {
                repo: repo.into(),
                fetched_at: new.fetched_at.clone(),
                stale: new.stale,
            },
        });
        entry.snapshot = Some(new);
        entry.in_flight = false;
        entry.finished = entry.started;
        if entry.want > entry.started {
            self.start_fetch(&mut inner, repo);
        }
        drop(inner);

        // Waiters wake only after disk and the event stream agree with what
        // they are about to return.
        if let Some(snapshot) = persist {
            self.persist(&snapshot);
        }
        if let (Some(events), Some(event)) = (&self.events, event) {
            events.push(event);
        }
        self.done.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ForgeChecks, ForgePullRequest};

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn interval_widens_with_idleness_and_tightens_while_busy() {
        assert_eq!(interval(false, Duration::ZERO), Duration::from_secs(30));
        assert_eq!(interval(true, Duration::ZERO), Duration::from_secs(15));
        assert_eq!(interval(false, 4 * MIN), Duration::from_secs(30));
        assert_eq!(interval(false, 5 * MIN), Duration::from_secs(120));
        assert_eq!(interval(true, 5 * MIN), Duration::from_secs(60));
        assert_eq!(interval(false, 29 * MIN), Duration::from_secs(120));
        assert_eq!(interval(false, 30 * MIN), Duration::from_secs(450));
        assert_eq!(interval(true, 30 * MIN), Duration::from_secs(225));
    }

    fn pr(mergeable: &str, checks: Option<&str>) -> ForgePullRequest {
        ForgePullRequest {
            number: 1,
            title: String::new(),
            author: None,
            url: String::new(),
            head: String::new(),
            base: String::new(),
            head_sha: String::new(),
            draft: false,
            labels: Vec::new(),
            mergeable: mergeable.into(),
            merge_state: String::new(),
            review_decision: None,
            created_at: String::new(),
            updated_at: String::new(),
            closing_issues: Vec::new(),
            checks: checks.map(|state| ForgeChecks {
                state: Some(state.into()),
                contexts: Vec::new(),
            }),
        }
    }

    #[test]
    fn busy_means_a_watched_pr_is_still_settling() {
        let with = |prs| ForgeSnapshot {
            prs,
            ..empty("o/n")
        };
        assert!(busy(&with(vec![pr("MERGEABLE", Some("PENDING"))])));
        assert!(busy(&with(vec![pr("UNKNOWN", Some("SUCCESS"))])));
        assert!(!busy(&with(vec![pr("MERGEABLE", Some("SUCCESS"))])));
        assert!(
            !busy(&with(vec![pr("UNKNOWN", None)])),
            "unwatched PRs sit at UNKNOWN for days"
        );
    }

    #[test]
    fn repo_names_cannot_escape_the_store() {
        assert!(valid_repo("the-indi-app/project-indi"));
        assert!(valid_repo("o/n.js"));
        for bad in ["o", "o/", "/n", "o/n/x", "../n", "o/..", "o/n m", "o\\n/x"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}
