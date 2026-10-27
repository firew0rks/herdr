use std::io::Write;

use crate::api::schema::{
    ForgeGhParams, ForgeRefreshParams, ForgeSnapshot, ForgeSnapshotParams, Method, Request,
};

const USAGE: &str = "usage:
  herdr forge snapshot [--repo owner/name] [--json]
  herdr forge refresh [--repo owner/name] [--pr N] [--json]
  herdr forge gh [--] <gh args...>";

pub(super) fn run_forge_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("snapshot") => snapshot_command(&args[1..], false),
        Some("refresh") => snapshot_command(&args[1..], true),
        Some("gh") => gh_command(&args[1..]),
        Some("help" | "--help" | "-h") => {
            eprintln!("{USAGE}");
            Ok(0)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

fn snapshot_command(args: &[String], refresh: bool) -> std::io::Result<i32> {
    let mut repo = None;
    let mut pr = None;
    let mut json = false;
    let args = super::expand_equals_args(args, &["--repo", "--pr"]);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--repo" if iter.len() > 0 => repo = iter.next().cloned(),
            "--pr" if refresh && iter.len() > 0 => {
                pr = iter
                    .next()
                    .map(|value| super::parse_u64_flag("--pr", value))
                    .transpose()?;
            }
            "--json" => json = true,
            _ => {
                eprintln!("{USAGE}");
                return Ok(2);
            }
        }
    }
    let cwd = std::env::current_dir()
        .ok()
        .map(|cwd| cwd.display().to_string());
    let method = if refresh {
        Method::ForgeRefresh(ForgeRefreshParams { repo, cwd, pr })
    } else {
        Method::ForgeSnapshot(ForgeSnapshotParams { repo, cwd })
    };
    let response = super::send_request(&Request {
        id: "cli:forge".into(),
        method,
    })?;
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(1);
    }
    let snapshot = &response["result"]["snapshot"];
    if json {
        println!("{snapshot}");
        return Ok(0);
    }
    match serde_json::from_value::<ForgeSnapshot>(snapshot.clone()) {
        Ok(snapshot) => print_summary(&snapshot),
        Err(err) => {
            eprintln!("unexpected forge snapshot: {err}");
            return Ok(1);
        }
    }
    Ok(0)
}

fn print_summary(snapshot: &ForgeSnapshot) {
    let fetched = snapshot.fetched_at.as_deref().unwrap_or("never");
    println!("{}  fetched {fetched}", snapshot.repo);
    if let Some(error) = &snapshot.error {
        println!("stale: {error}");
    }
    for pr in &snapshot.prs {
        let checks = pr
            .checks
            .as_ref()
            .and_then(|checks| checks.state.as_deref())
            .unwrap_or("-");
        println!(
            "#{:<6} {:<10} {:<8} {} ({})",
            pr.number, pr.merge_state, checks, pr.title, pr.head
        );
    }
    println!(
        "{} open PRs, {} recent runs, {} workflows",
        snapshot.prs.len(),
        snapshot.runs.len(),
        snapshot.workflows.len()
    );
}

/// Backs a `gh` shim: reads replay from the server's cache, everything else
/// (and every read when no server is reachable) runs the real gh here.
fn gh_command(args: &[String]) -> std::io::Result<i32> {
    let argv = args.strip_prefix(&["--".to_string()]).unwrap_or(args);
    let inner = std::env::var_os(crate::forge::INNER_ENV_VAR).is_some();
    // GH_REPO retargets gh away from the cwd's repo, which the cache key can't see.
    let retargeted = std::env::var_os("GH_REPO").is_some();
    if inner || retargeted || !crate::forge::cacheable(argv) {
        return run_gh(argv);
    }
    let request = Request {
        id: "cli:forge.gh".into(),
        method: Method::ForgeGh(ForgeGhParams {
            argv: argv.to_vec(),
            cwd: std::env::current_dir()
                .ok()
                .map(|cwd| cwd.display().to_string()),
        }),
    };
    let Ok(response) = crate::api::client::ApiClient::local().request_value(&request) else {
        return run_gh(argv);
    };
    let result = &response["result"];
    if result["type"] != "forge_gh" {
        return run_gh(argv);
    }
    crate::platform::begin_cli_output();
    std::io::stdout().write_all(result["stdout"].as_str().unwrap_or_default().as_bytes())?;
    std::io::stderr().write_all(result["stderr"].as_str().unwrap_or_default().as_bytes())?;
    Ok(result["exit_code"]
        .as_i64()
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(1))
}

fn run_gh(argv: &[String]) -> std::io::Result<i32> {
    let gh = crate::forge::Gh::from_env();
    crate::platform::end_cli_output();
    let status = std::process::Command::new(&gh.program)
        .args(argv)
        .env("PATH", &gh.path)
        .env(crate::forge::INNER_ENV_VAR, "1")
        .status()?;
    Ok(status.code().unwrap_or(1))
}

pub(super) fn command() -> clap::Command {
    use clap::{Arg, ArgAction, Command};

    let repo = || {
        Arg::new("repo")
            .long("repo")
            .value_name("OWNER/NAME")
            .help("GitHub repo; defaults to the cwd's origin remote")
    };
    let json = || {
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .help("Print the snapshot JSON")
    };
    Command::new("forge")
        .about("Read the server's cached GitHub snapshot")
        .subcommand(
            Command::new("snapshot")
                .about("Print the cached snapshot for a repo")
                .arg(repo())
                .arg(json()),
        )
        .subcommand(
            Command::new("refresh")
                .about("Fetch a repo now and print the new snapshot")
                .arg(repo())
                .arg(
                    Arg::new("pr")
                        .long("pr")
                        .value_name("N")
                        .help("Fetch this PR's checks too"),
                )
                .arg(json()),
        )
        .subcommand(
            Command::new("gh")
                .about("Run gh, replaying read-only commands from the cache")
                .arg(
                    Arg::new("args")
                        .num_args(0..)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .value_name("GH_ARGS"),
                ),
        )
}
