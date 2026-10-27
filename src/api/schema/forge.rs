use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeSnapshotParams {
    /// GitHub `owner/name`. Defaults to the `origin` remote of `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeRefreshParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Fetch this PR's checks too, and keep watching them for a while.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeGhParams {
    /// Arguments after `gh`.
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeSnapshot {
    pub repo: String,
    pub fetched_at: Option<String>,
    pub stale: bool,
    pub error: Option<String>,
    pub viewer: Option<String>,
    pub prs: Vec<ForgePullRequest>,
    pub runs: Vec<ForgeRun>,
    pub workflows: Vec<ForgeWorkflow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgePullRequest {
    pub number: u64,
    pub title: String,
    pub author: Option<String>,
    pub url: String,
    pub head: String,
    pub base: String,
    pub head_sha: String,
    pub draft: bool,
    pub labels: Vec<String>,
    pub mergeable: String,
    pub merge_state: String,
    pub review_decision: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closing_issues: Vec<u64>,
    pub checks: Option<ForgeChecks>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeChecks {
    pub state: Option<String>,
    pub contexts: Vec<ForgeCheckContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeCheckContext {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeRun {
    pub id: u64,
    pub workflow: String,
    pub title: String,
    pub branch: String,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgeWorkflow {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub state: String,
}
