use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::gh::Output;

pub(crate) const TTL: Duration = Duration::from_secs(20);

/// Whether `gh <argv>` is a read we may replay from cache. Anything that could
/// write, prompt, or open a browser runs for real.
pub(crate) fn cacheable(argv: &[String]) -> bool {
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    if words
        .iter()
        .any(|word| matches!(*word, "--web" | "-w" | "--watch"))
    {
        return false;
    }
    match words.as_slice() {
        ["api", rest @ ..] => api_is_read(rest),
        ["pr", "view" | "list" | "checks", ..]
        | ["run", "list" | "view", ..]
        | ["repo", "view", ..] => true,
        _ => false,
    }
}

fn api_is_read(args: &[&str]) -> bool {
    let mut endpoint = None;
    let mut method = None;
    let mut has_fields = false;
    let mut query_values = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--method=") {
            method = Some(value.to_string());
        } else if matches!(arg, "-X" | "--method") {
            method = iter.next().map(str::to_string);
        } else if let Some(value) = arg.strip_prefix("-X") {
            method = Some(value.to_string());
        } else if matches!(arg, "--input") || arg.starts_with("--input=") {
            return false;
        } else if let Some(value) = field_value(arg, &mut iter) {
            has_fields = true;
            query_values.push(value);
        } else if !arg.starts_with('-') && endpoint.is_none() {
            endpoint = Some(arg);
        }
    }
    let graphql = endpoint == Some("graphql");
    if graphql {
        // gh sends every graphql call as a POST; only the query text tells a read
        // from a write. A query loaded from a file can't be inspected, so it runs.
        return query_values
            .iter()
            .all(|value| match value.split_once('=') {
                Some(("query", text)) => !text.starts_with('@') && !mentions_mutation(text),
                _ => true,
            });
    }
    match method {
        Some(method) => method.eq_ignore_ascii_case("GET"),
        // Fields without an explicit method make gh default to POST.
        None => !has_fields,
    }
}

fn field_value<'a>(arg: &'a str, rest: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
    for flag in ["-f", "-F", "--field", "--raw-field"] {
        if arg == flag {
            return Some(rest.next().unwrap_or_default());
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(value);
        }
        if flag.len() == 2 && arg.len() > 2 && arg.starts_with(flag) {
            return Some(&arg[2..]);
        }
    }
    None
}

fn mentions_mutation(query: &str) -> bool {
    query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word.eq_ignore_ascii_case("mutation"))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub argv: Vec<String>,
    /// The repo and branch of the caller's cwd: `gh pr view` with no number
    /// answers for the current branch, so two worktrees must not share a reply.
    pub scope: String,
}

#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

#[derive(Default)]
struct SlotState {
    running: bool,
    generation: u64,
    result: Option<(Instant, Output)>,
}

#[derive(Default)]
pub(crate) struct GhCache {
    slots: Mutex<HashMap<Key, Arc<Slot>>>,
}

impl GhCache {
    /// Replays a fresh successful result, joins a run already in flight for the
    /// same key, or runs `run` itself. Returns the output and whether it was cached.
    pub fn get_or_run(&self, key: Key, run: impl FnOnce() -> Output) -> (Output, bool) {
        let slot = {
            let Ok(mut slots) = self.slots.lock() else {
                return (run(), false);
            };
            slots.retain(|_, slot| {
                slot.state.lock().is_ok_and(|state| {
                    state.running
                        || state
                            .result
                            .as_ref()
                            .is_some_and(|(at, _)| at.elapsed() < TTL)
                })
            });
            Arc::clone(slots.entry(key).or_default())
        };

        let Ok(mut state) = slot.state.lock() else {
            return (run(), false);
        };
        if let Some((at, output)) = &state.result {
            if output.ok() && at.elapsed() < TTL && !state.running {
                return (output.clone(), true);
            }
        }
        if state.running {
            let joined = state.generation;
            let Ok(state) = slot
                .done
                .wait_while(state, |state| state.running && state.generation == joined)
            else {
                return (run(), false);
            };
            if let Some((_, output)) = &state.result {
                return (output.clone(), true);
            }
            drop(state);
            return (run(), false);
        }
        state.running = true;
        drop(state);

        let output = run();
        if let Ok(mut state) = slot.state.lock() {
            state.running = false;
            state.generation += 1;
            state.result = Some((Instant::now(), output.clone()));
        }
        slot.done.notify_all();
        (output, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn reads_are_cacheable() {
        for line in [
            "pr view 12 --json state",
            "pr list --json number",
            "pr checks 12",
            "run list -L 5",
            "run view 99",
            "repo view --json name",
            "api repos/o/n/pulls",
            "api -X GET repos/o/n/pulls -f state=open",
            "api --method=get repos/o/n",
            "api graphql -f query={viewer{login}}",
            "api graphql -F owner=o -f query=query{repository(owner:$owner){id}}",
        ] {
            assert!(cacheable(&argv(line)), "{line}");
        }
    }

    #[test]
    fn writes_and_interactive_commands_pass_through() {
        for line in [
            "pr create --fill",
            "pr merge 12",
            "pr view 12 --web",
            "pr checks 12 --watch",
            "run rerun 99",
            "issue list",
            "api -X POST repos/o/n/issues",
            "api --method DELETE repos/o/n/labels/x",
            "api repos/o/n/issues -f title=x",
            "api repos/o/n/issues --input body.json",
            "api graphql -f query=mutation{addStar(input:{starrableId:\"x\"}){clientMutationId}}",
            "api graphql -f query=MUTATION{x}",
            "api graphql -f query=@mutation.graphql",
            "--version",
        ] {
            assert!(!cacheable(&argv(line)), "{line}");
        }
    }

    fn key(scope: &str) -> Key {
        Key {
            argv: argv("pr list"),
            scope: scope.into(),
        }
    }

    fn output(stdout: &str, code: i32) -> Output {
        Output {
            stdout: stdout.into(),
            stderr: String::new(),
            code,
        }
    }

    #[test]
    fn concurrent_callers_share_one_run_and_later_callers_replay_it() {
        let cache = Arc::new(GhCache::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let runs = Arc::clone(&runs);
                std::thread::spawn(move || {
                    cache
                        .get_or_run(key("o/n@main"), || {
                            runs.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(100));
                            output("[]", 0)
                        })
                        .0
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().stdout, "[]");
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let (_, cached) = cache.get_or_run(key("o/n@main"), || panic!("should replay"));
        assert!(cached);
        let (_, cached) = cache.get_or_run(key("o/n@other"), || output("[]", 0));
        assert!(!cached, "a different branch scope is a different key");
    }

    #[test]
    fn failures_are_not_replayed() {
        let cache = GhCache::default();
        cache.get_or_run(key("o/n"), || output("", 1));
        let (out, cached) = cache.get_or_run(key("o/n"), || output("ok", 0));
        assert!(!cached);
        assert_eq!(out.stdout, "ok");
    }
}
