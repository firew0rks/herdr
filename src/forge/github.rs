use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;

use super::gh::Gh;
use crate::api::schema::{
    ForgeCheckContext, ForgeChecks, ForgePullRequest, ForgeRun, ForgeWorkflow,
};

const GH_TIMEOUT: Duration = Duration::from_secs(60);
const PR_PAGE: usize = 25;
const RUN_LIMIT: usize = 30;
/// Rollups are the expensive part of a PR read; small batches keep each query
/// well clear of the gateway timeout that a 90-PR rollup query hits.
const CHECK_BATCH: usize = 10;

/// What one fetch should ask for beyond the light PR list.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub repo: String,
    pub viewer: Option<String>,
    pub check_branches: BTreeSet<String>,
    pub check_prs: BTreeSet<u64>,
    pub workflows: bool,
    /// Ask for merge state and review decision on every open PR, not only the
    /// watched ones. GitHub computes both on read: they cost about 9s per page
    /// of 25, so the pool asks only every few minutes.
    pub details: bool,
}

#[derive(Debug)]
pub(crate) struct Fetched {
    pub viewer: String,
    pub prs: Vec<ForgePullRequest>,
    pub runs: Vec<ForgeRun>,
    pub workflows: Option<Vec<ForgeWorkflow>>,
}

pub(crate) fn fetch(gh: &Gh, plan: &Plan) -> Result<Fetched, String> {
    let (owner, name) = plan
        .repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repo: {}", plan.repo))?;
    let actions = || -> Result<_, String> {
        let runs = runs(gh, &plan.repo)?;
        let workflows = plan
            .workflows
            .then(|| workflows(gh, &plan.repo))
            .transpose()?;
        Ok((runs, workflows))
    };
    // PR pages must run in order; Actions reads overlap them to cut a fetch's
    // wall time.
    std::thread::scope(|scope| {
        let background = std::thread::Builder::new()
            .name("herdr-forge-actions".into())
            .spawn_scoped(scope, actions);
        let (viewer, prs) = pull_requests_with_checks(gh, plan, owner, name)?;
        let (runs, workflows) = match background {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| Err("actions fetch panicked".into()))?,
            Err(_) => actions()?,
        };
        Ok(Fetched {
            viewer,
            prs,
            runs,
            workflows,
        })
    })
}

fn pull_requests_with_checks(
    gh: &Gh,
    plan: &Plan,
    owner: &str,
    name: &str,
) -> Result<(String, Vec<ForgePullRequest>), String> {
    let viewer = match &plan.viewer {
        Some(viewer) => viewer.clone(),
        None => viewer_login(gh)?,
    };
    let mut prs = pull_requests(gh, owner, name, plan.details)?;
    let watched: Vec<u64> = prs
        .iter()
        .filter(|pr| {
            pr.author.as_deref() == Some(viewer.as_str())
                || plan.check_branches.contains(&pr.head)
                || plan.check_prs.contains(&pr.number)
        })
        .map(|pr| pr.number)
        .collect();
    for batch in watched.chunks(CHECK_BATCH) {
        let mut rollups = rollups(gh, owner, name, batch)?;
        for pr in prs.iter_mut() {
            if let Some(rollup) = rollups.remove(&pr.number) {
                if let Some(merge_state) = rollup.merge_state_status.clone() {
                    pr.merge_state = merge_state;
                }
                pr.review_decision.clone_from(&rollup.review_decision);
                pr.checks = Some(rollup.into_checks());
            }
        }
    }
    Ok((viewer, prs))
}

fn run_json<T: serde::de::DeserializeOwned>(gh: &Gh, args: &[String]) -> Result<T, String> {
    let output = gh
        .run(args, None, GH_TIMEOUT)
        .map_err(|err| format!("gh: {err}"))?;
    if !output.ok() {
        let detail = output
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        return Err(format!("gh exited {}: {detail}", output.code));
    }
    serde_json::from_str(&output.stdout)
        .map_err(|err| format!("gh returned unexpected JSON: {err}"))
}

fn graphql<T: serde::de::DeserializeOwned>(
    gh: &Gh,
    query: &str,
    vars: &[(&str, &str)],
) -> Result<T, String> {
    let mut args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={query}"),
    ];
    for (key, value) in vars {
        args.push("-f".into());
        args.push(format!("{key}={value}"));
    }
    let response: GraphqlResponse<T> = run_json(gh, &args)?;
    response
        .data
        .ok_or_else(|| "gh graphql returned no data".to_string())
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
}

fn viewer_login(gh: &Gh) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Data {
        viewer: Login,
    }
    let data: Data = graphql(gh, "query{viewer{login}}", &[])?;
    Ok(data.viewer.login)
}

const PULL_REQUESTS_QUERY: &str = "query($owner:String!,$name:String!,$after:String){\
repository(owner:$owner,name:$name){pullRequests(states:OPEN,first:25,after:$after,\
orderBy:{field:UPDATED_AT,direction:DESC}){pageInfo{hasNextPage endCursor}nodes{\
number title url author{login} headRefName baseRefName headRefOid isDraft \
labels(first:30){nodes{name}} mergeable DETAILS createdAt updatedAt \
closingIssuesReferences(first:10){nodes{number}}}}}}";

fn pull_requests(
    gh: &Gh,
    owner: &str,
    name: &str,
    details: bool,
) -> Result<Vec<ForgePullRequest>, String> {
    #[derive(Deserialize)]
    struct Data {
        repository: Option<Repository>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        pull_requests: Connection<PullRequestNode>,
    }

    let query = PULL_REQUESTS_QUERY.replace(
        "DETAILS",
        if details {
            "mergeStateStatus reviewDecision"
        } else {
            ""
        },
    );
    let mut prs = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut vars = vec![("owner", owner), ("name", name)];
        if let Some(cursor) = after.as_deref() {
            vars.push(("after", cursor));
        }
        let data: Data = graphql(gh, &query, &vars)?;
        let page = data
            .repository
            .ok_or_else(|| format!("repository {owner}/{name} not found"))?
            .pull_requests;
        let fetched = page.nodes.len();
        prs.extend(page.nodes.into_iter().map(PullRequestNode::into_pr));
        match page.page_info {
            Some(info) if info.has_next_page && fetched == PR_PAGE => after = info.end_cursor,
            _ => break,
        }
        if after.is_none() {
            break;
        }
    }
    Ok(prs)
}

fn rollups(
    gh: &Gh,
    owner: &str,
    name: &str,
    numbers: &[u64],
) -> Result<std::collections::HashMap<u64, RollupPr>, String> {
    #[derive(Deserialize)]
    struct Data {
        repository: Option<std::collections::HashMap<String, Option<RollupPr>>>,
    }

    let fields: String = numbers
        .iter()
        .map(|number| {
            format!(
                "pr{number}:pullRequest(number:{number}){{mergeStateStatus reviewDecision commits(last:1){{nodes{{commit{{\
statusCheckRollup{{state contexts(first:100){{nodes{{__typename \
...on CheckRun{{name status conclusion startedAt completedAt detailsUrl}} \
...on StatusContext{{context state createdAt targetUrl}}}}}}}}}}}}}}}}"
            )
        })
        .collect();
    let query = format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{fields}}}}}"
    );
    let data: Data = graphql(gh, &query, &[("owner", owner), ("name", name)])?;
    let repository = data
        .repository
        .ok_or_else(|| format!("repository {owner}/{name} not found"))?;
    Ok(repository
        .into_iter()
        .filter_map(|(alias, pr)| {
            let number = alias.strip_prefix("pr")?.parse().ok()?;
            Some((number, pr?))
        })
        .collect())
}

fn runs(gh: &Gh, repo: &str) -> Result<Vec<ForgeRun>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Run {
        database_id: u64,
        #[serde(default)]
        workflow_name: String,
        #[serde(default)]
        display_title: String,
        #[serde(default)]
        head_branch: String,
        #[serde(default)]
        event: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        conclusion: String,
        #[serde(default)]
        created_at: String,
        #[serde(default)]
        url: String,
    }
    let runs: Vec<Run> = run_json(
        gh,
        &[
            "run".into(),
            "list".into(),
            "-R".into(),
            repo.into(),
            "-L".into(),
            RUN_LIMIT.to_string(),
            "--json".into(),
            "databaseId,workflowName,displayTitle,headBranch,event,status,conclusion,createdAt,url"
                .into(),
        ],
    )?;
    Ok(runs
        .into_iter()
        .map(|run| ForgeRun {
            id: run.database_id,
            workflow: run.workflow_name,
            title: run.display_title,
            branch: run.head_branch,
            event: run.event,
            status: run.status,
            conclusion: non_empty(run.conclusion),
            created_at: run.created_at,
            url: run.url,
        })
        .collect())
}

fn workflows(gh: &Gh, repo: &str) -> Result<Vec<ForgeWorkflow>, String> {
    #[derive(Deserialize)]
    struct Workflow {
        id: u64,
        #[serde(default)]
        name: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        state: String,
    }
    let workflows: Vec<Workflow> = run_json(
        gh,
        &[
            "workflow".into(),
            "list".into(),
            "-R".into(),
            repo.into(),
            "-L".into(),
            "200".into(),
            "--json".into(),
            "id,name,path,state".into(),
        ],
    )?;
    Ok(workflows
        .into_iter()
        .map(|workflow| ForgeWorkflow {
            id: workflow.id,
            name: workflow.name,
            path: workflow.path,
            state: workflow.state,
        })
        .collect())
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
    #[serde(default)]
    page_info: Option<PageInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

#[derive(Deserialize)]
struct Number {
    number: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    author: Option<Login>,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    is_draft: bool,
    labels: Option<Connection<Name>>,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    merge_state_status: String,
    review_decision: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    closing_issues_references: Option<Connection<Number>>,
}

impl PullRequestNode {
    fn into_pr(self) -> ForgePullRequest {
        ForgePullRequest {
            number: self.number,
            title: self.title,
            author: self.author.map(|author| author.login),
            url: self.url,
            head: self.head_ref_name,
            base: self.base_ref_name,
            head_sha: self.head_ref_oid,
            draft: self.is_draft,
            labels: self
                .labels
                .map(|labels| labels.nodes.into_iter().map(|label| label.name).collect())
                .unwrap_or_default(),
            mergeable: self.mergeable,
            merge_state: self.merge_state_status,
            review_decision: self.review_decision,
            created_at: self.created_at,
            updated_at: self.updated_at,
            closing_issues: self
                .closing_issues_references
                .map(|issues| issues.nodes.into_iter().map(|issue| issue.number).collect())
                .unwrap_or_default(),
            checks: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollupPr {
    merge_state_status: Option<String>,
    review_decision: Option<String>,
    commits: Connection<RollupCommitNode>,
}

#[derive(Deserialize)]
struct RollupCommitNode {
    commit: RollupCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollupCommit {
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    state: Option<String>,
    contexts: Connection<RollupContext>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum RollupContext {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        started_at: Option<String>,
        completed_at: Option<String>,
        details_url: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: String,
        created_at: Option<String>,
        target_url: Option<String>,
    },
}

impl RollupPr {
    fn into_checks(self) -> ForgeChecks {
        let rollup = self
            .commits
            .nodes
            .into_iter()
            .next_back()
            .and_then(|node| node.commit.status_check_rollup);
        let Some(rollup) = rollup else {
            return ForgeChecks {
                state: None,
                contexts: Vec::new(),
            };
        };
        ForgeChecks {
            state: rollup.state,
            contexts: rollup
                .contexts
                .nodes
                .into_iter()
                .map(RollupContext::into_context)
                .collect(),
        }
    }
}

impl RollupContext {
    fn into_context(self) -> ForgeCheckContext {
        match self {
            Self::CheckRun {
                name,
                status,
                conclusion,
                started_at,
                completed_at,
                details_url,
            } => ForgeCheckContext {
                name,
                status,
                conclusion,
                started_at,
                completed_at,
                url: details_url,
            },
            // Commit statuses have no run lifecycle; map them onto the CheckRun
            // vocabulary so consumers handle one shape.
            Self::StatusContext {
                context,
                state,
                created_at,
                target_url,
            } => {
                let pending = matches!(state.as_str(), "PENDING" | "EXPECTED");
                ForgeCheckContext {
                    name: context,
                    status: if pending { "PENDING" } else { "COMPLETED" }.into(),
                    conclusion: (!pending).then_some(state),
                    started_at: created_at.clone(),
                    completed_at: if pending { None } else { created_at },
                    url: target_url,
                }
            }
        }
    }
}
