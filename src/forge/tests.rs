use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::gh::Gh;
use super::pool::Pool;
use super::test_support::fake_gh;
use crate::api::schema::{EventData, ForgeSnapshot};
use crate::api::EventHub;

const REPO: &str = "o/n";

const SCRIPT: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
echo "$*" >> "$dir/calls.log"
[ -f "$dir/delay" ] && sleep "$(cat "$dir/delay")"
[ -f "$dir/fail" ] && { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
case "$*" in
  *viewer*) echo '{"data":{"viewer":{"login":"me"}}}' ;;
  *statusCheckRollup*) cat "$dir/checks.json" ;;
  *pullRequests*mergeStateStatus*) cat "$dir/prs.json" ;;
  *pullRequests*) sed -e 's/"mergeStateStatus":"[A-Z]*",//' -e 's/"reviewDecision":[^,]*,//' "$dir/prs.json" ;;
  "run list"*) cat "$dir/runs.json" ;;
  "workflow list"*) cat "$dir/workflows.json" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

const PRS: &str = r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[
 {"number":1,"title":"Mine","url":"https://github.com/o/n/pull/1","author":{"login":"me"},"headRefName":"feat-a","baseRefName":"main","headRefOid":"abc","isDraft":false,"labels":{"nodes":[{"name":"zealot:clean"}]},"mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":"REVIEW_REQUIRED","createdAt":"2026-10-27T10:00:00Z","updatedAt":"2026-10-27T10:30:00Z","closingIssuesReferences":{"nodes":[{"number":123}]}},
 {"number":2,"title":"Theirs","url":"https://github.com/o/n/pull/2","author":{"login":"other"},"headRefName":"feat-b","baseRefName":"main","headRefOid":"def","isDraft":true,"labels":{"nodes":[]},"mergeable":"CONFLICTING","mergeStateStatus":"DIRTY","reviewDecision":null,"createdAt":"2026-10-26T10:00:00Z","updatedAt":"2026-10-26T10:30:00Z","closingIssuesReferences":{"nodes":[]}}
]}}}}"#;

const CHECKS: &str = r#"{"data":{"repository":{"pr1":{"mergeStateStatus":"BLOCKED","reviewDecision":"REVIEW_REQUIRED","commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING","contexts":{"nodes":[
 {"__typename":"CheckRun","name":"CI","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-10-27T10:01:00Z","completedAt":"2026-10-27T10:05:00Z","detailsUrl":"https://ci/1"},
 {"__typename":"StatusContext","context":"zealot","state":"PENDING","createdAt":"2026-10-27T10:02:00Z","targetUrl":null}
]}}}}]}}}}}"#;

const RUNS: &str = r#"[{"databaseId":9,"workflowName":"CI","displayTitle":"Mine","headBranch":"feat-a","event":"pull_request","status":"in_progress","conclusion":"","createdAt":"2026-10-27T10:01:00Z","url":"https://github.com/o/n/actions/runs/9"}]"#;

const WORKFLOWS: &str =
    r#"[{"id":5,"name":"CI","path":".github/workflows/ci.yml","state":"active"}]"#;

struct Fixture {
    bin: PathBuf,
    pool: Arc<Pool>,
    events: EventHub,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let bin = fake_gh(SCRIPT);
        for (file, body) in [
            ("prs.json", PRS),
            ("checks.json", CHECKS),
            ("runs.json", RUNS),
            ("workflows.json", WORKFLOWS),
        ] {
            std::fs::write(bin.join(file), body).unwrap();
        }
        let store = bin.join("store");
        let events = EventHub::default();
        let pool = Pool::new(gh(&bin), Some(store.clone()), Some(events.clone()));
        Self {
            bin,
            pool,
            events,
            store,
        }
    }

    fn set(&self, flag: &str, value: Option<&str>) {
        let path = self.bin.join(flag);
        match value {
            Some(value) => std::fs::write(path, value).unwrap(),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn calls(&self, needle: &str) -> usize {
        std::fs::read_to_string(self.bin.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    }

    fn forge_events(&self) -> Vec<(Option<String>, bool)> {
        self.events
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::ForgeUpdated {
                    fetched_at, stale, ..
                } => Some((fetched_at, stale)),
                _ => None,
            })
            .collect()
    }
}

fn gh(bin: &Path) -> Gh {
    super::test_support::gh_in(bin)
}

#[test]
fn snapshot_json_matches_the_published_contract() {
    let fixture = Fixture::new();
    let snapshot = fixture.pool.snapshot(REPO);
    let json = serde_json::to_value(&snapshot).unwrap();

    let keys = |value: &serde_json::Value| {
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(&json),
        [
            "error",
            "fetched_at",
            "prs",
            "repo",
            "runs",
            "stale",
            "viewer",
            "workflows"
        ]
    );
    assert_eq!(
        keys(&json["prs"][0]),
        [
            "author",
            "base",
            "checks",
            "closing_issues",
            "created_at",
            "draft",
            "head",
            "head_sha",
            "labels",
            "merge_state",
            "mergeable",
            "number",
            "review_decision",
            "title",
            "updated_at",
            "url"
        ]
    );
    assert_eq!(json["repo"], REPO);
    assert_eq!(json["stale"], false);
    assert_eq!(json["error"], serde_json::Value::Null);
    assert_eq!(json["viewer"], "me");
    assert!(json["fetched_at"].as_str().unwrap().ends_with('Z'));

    let mine = &json["prs"][0];
    assert_eq!(mine["labels"], serde_json::json!(["zealot:clean"]));
    assert_eq!(mine["closing_issues"], serde_json::json!([123]));
    assert_eq!(mine["checks"]["state"], "PENDING");
    assert_eq!(
        mine["checks"]["contexts"][1],
        serde_json::json!({"name":"zealot","status":"PENDING","conclusion":null,
            "started_at":"2026-10-27T10:02:00Z","completed_at":null,"url":null})
    );
    assert_eq!(
        json["prs"][1]["checks"],
        serde_json::Value::Null,
        "rollups are only fetched for the viewer's PRs and checked-out branches"
    );
    assert_eq!(json["prs"][1]["review_decision"], serde_json::Value::Null);
    assert_eq!(
        json["runs"][0],
        serde_json::json!({"id":9,"workflow":"CI","title":"Mine","branch":"feat-a",
            "event":"pull_request","status":"in_progress","conclusion":null,
            "created_at":"2026-10-27T10:01:00Z","url":"https://github.com/o/n/actions/runs/9"})
    );
    assert_eq!(
        json["workflows"][0],
        serde_json::json!({"id":5,"name":"CI","path":".github/workflows/ci.yml","state":"active"})
    );
}

#[test]
fn checked_out_branches_and_watched_prs_get_rollups() {
    let fixture = Fixture::new();
    fixture
        .pool
        .set_workspaces([(REPO.to_string(), ["feat-b".to_string()].into())].into());
    fixture.pool.refresh(REPO, None);
    let rollup_query = std::fs::read_to_string(fixture.bin.join("calls.log")).unwrap();
    assert!(rollup_query.contains("pr1:pullRequest(number:1)"));
    assert!(rollup_query.contains("pr2:pullRequest(number:2)"));
}

#[test]
fn concurrent_first_asks_share_one_fetch() {
    let fixture = Fixture::new();
    fixture.set("delay", Some("0.3"));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let pool = Arc::clone(&fixture.pool);
            std::thread::spawn(move || pool.snapshot(REPO))
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap().prs.len(), 2);
    }
    assert_eq!(fixture.calls("pullRequests"), 1);
}

#[test]
fn refreshes_during_a_fetch_coalesce_into_one_more() {
    let fixture = Fixture::new();
    fixture.pool.snapshot(REPO);
    fixture.set("delay", Some("0.3"));
    let first = {
        let pool = Arc::clone(&fixture.pool);
        std::thread::spawn(move || pool.refresh(REPO, None))
    };
    std::thread::sleep(Duration::from_millis(100));
    let joiners: Vec<_> = (0..4)
        .map(|_| {
            let pool = Arc::clone(&fixture.pool);
            std::thread::spawn(move || pool.refresh(REPO, None))
        })
        .collect();
    first.join().unwrap();
    for joiner in joiners {
        joiner.join().unwrap();
    }
    assert_eq!(
        fixture.calls("pullRequests"),
        3,
        "first ask, the in-flight refresh, then one fetch for everyone who joined late"
    );
}

#[test]
fn a_failed_fetch_keeps_the_last_good_snapshot() {
    let fixture = Fixture::new();
    let good = fixture.pool.refresh(REPO, None);
    assert!(!good.stale);

    fixture.set("fail", Some(""));
    let failed = fixture.pool.refresh(REPO, None);
    assert!(failed.stale);
    assert!(
        failed.error.as_deref().unwrap().contains("502"),
        "{failed:?}"
    );
    assert_eq!(failed.prs, good.prs);
    assert_eq!(failed.fetched_at, good.fetched_at);

    let persisted: ForgeSnapshot =
        serde_json::from_slice(&std::fs::read(fixture.store.join("o__n.json")).unwrap()).unwrap();
    assert!(!persisted.stale, "only good snapshots reach disk");

    fixture.set("fail", None);
    let recovered = fixture.pool.refresh(REPO, None);
    assert!(!recovered.stale);
    assert_eq!(recovered.error, None);

    let stale_flags: Vec<bool> = fixture
        .forge_events()
        .iter()
        .map(|(_, stale)| *stale)
        .collect();
    assert_eq!(stale_flags, [false, true, false]);
}

#[test]
fn a_first_fetch_failure_reports_an_empty_stale_snapshot() {
    let fixture = Fixture::new();
    fixture.set("fail", Some(""));
    let snapshot = fixture.pool.snapshot(REPO);
    assert!(snapshot.stale);
    assert!(snapshot.prs.is_empty());
    assert_eq!(snapshot.fetched_at, None);
}

#[test]
fn unchanged_data_emits_no_event() {
    let fixture = Fixture::new();
    fixture.pool.refresh(REPO, None);
    fixture.pool.refresh(REPO, None);
    assert_eq!(fixture.forge_events().len(), 1);
}

#[test]
fn a_restart_serves_the_persisted_snapshot_without_fetching() {
    let fixture = Fixture::new();
    let fetched = fixture.pool.refresh(REPO, None);
    let calls = fixture.calls("pullRequests");

    let restarted = Pool::new(gh(&fixture.bin), Some(fixture.store.clone()), None);
    assert_eq!(restarted.snapshot(REPO), fetched);
    assert_eq!(fixture.calls("pullRequests"), calls);
}

#[test]
fn workflows_and_viewer_are_not_refetched_every_cycle() {
    let fixture = Fixture::new();
    fixture.pool.refresh(REPO, None);
    let second = fixture.pool.refresh(REPO, None);
    assert_eq!(fixture.calls("workflow list"), 1);
    assert_eq!(fixture.calls("viewer"), 1);
    assert_eq!(second.workflows.len(), 1, "cached workflows carry over");
}

#[test]
fn remote_urls_resolve_to_owner_and_name() {
    for url in [
        "git@github.com:the-indi-app/project-indi.git",
        "https://github.com/the-indi-app/project-indi",
        "https://github.com/the-indi-app/project-indi.git/",
        "ssh://git@github.com/the-indi-app/project-indi.git",
    ] {
        assert_eq!(
            super::github_repo(url).as_deref(),
            Some("the-indi-app/project-indi"),
            "{url}"
        );
    }
    assert_eq!(super::github_repo("git@gitlab.com:o/n.git"), None);
    assert_eq!(super::github_repo("https://github.com/o/../x"), None);
}

#[test]
fn discovery_reads_checkouts_and_pane_cwds() {
    let snapshot = serde_json::json!({
        "workspaces": [{"worktree": {"checkout_path": "/w/a"}}, {}],
        "panes": [{"cwd": "/w/b"}, {"cwd": "/w/a"}, {}],
    });
    assert_eq!(
        super::snapshot_paths(&snapshot),
        ["/w/a", "/w/b"].map(PathBuf::from).into()
    );
}

#[test]
fn unwatched_prs_keep_swept_merge_details_between_sweeps() {
    let fixture = Fixture::new();
    let swept = fixture.pool.refresh(REPO, None);
    let light = fixture.pool.refresh(REPO, None);
    assert_eq!(
        fixture.calls("mergeStateStatus reviewDecision createdAt"),
        1
    );
    assert_eq!(light.prs[1].merge_state, "DIRTY", "carried from the sweep");
    assert_eq!(
        light.prs[0].merge_state, "BLOCKED",
        "watched PRs read it every fetch"
    );
    assert_eq!(
        light.prs[0].review_decision.as_deref(),
        Some("REVIEW_REQUIRED")
    );
    assert_eq!(light.prs, swept.prs);
}
