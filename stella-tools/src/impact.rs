//! Test-impact selection for `run_tests`' `scope: "impacted"` — compute the
//! blast radius of the working-tree change from the code graph's importer
//! edges (issue #334), so the edit→green loop runs only the tests that can
//! see the change instead of the whole suite.
//!
//! The selection is pure over (change set × importer edges): diff the
//! working tree (`git status --porcelain -uall`), walk the graph's
//! reverse-dependency relation transitively from each changed file, and
//! keep every reachable file that looks like a test file — including
//! changed test files themselves. Only languages whose import edges the
//! graph actually resolves participate (relative TS/JS and Python imports,
//! see `stella_graph`'s resolver); a change touching anything else makes
//! the selection untrustworthy, and the answer is then the WHOLE suite with
//! a one-line note. The posture throughout: fail loudly to over-testing,
//! never silently under-test — a skipped test that should have run is a
//! correctness hole (Rust selection is gated on #335's resolved `use`
//! edges).

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use crate::exec;

/// Timeout for the `git status` probe — a local metadata read.
const GIT_STATUS_TIMEOUT_SECS: u64 = 60;

/// The outcome of impact selection. Every arm is loud: `FullSuite` carries
/// the one-line note explaining why selection stood down, and an empty
/// selection is a named answer, never a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImpactSelection {
    /// Test files (root-relative, forward-slash, sorted) that transitively
    /// import a changed file — including changed test files themselves.
    Selected { tests: Vec<String>, changed: usize },
    /// The walk completed over fully-resolved edges and no test file
    /// reaches the change (`changed == 0` means the tree itself is clean).
    NothingImpacted { changed: usize },
    /// Selection cannot be trusted; run everything, prefixed by `note`.
    FullSuite { note: String },
}

/// Extensions whose import edges the graph resolves to real files (relative
/// TS/JS specifiers and Python relative imports — `stella_graph::import`).
/// A changed file outside this set may be depended on through edges the
/// graph cannot see, so its presence stands the whole selection down.
const RESOLVED_EXTS: [&str; 7] = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "py"];

fn extension(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

/// Whether the graph resolves import edges for this file's language.
pub(crate) fn language_resolved(path: &str) -> bool {
    RESOLVED_EXTS.contains(&extension(path))
}

/// Test-file heuristics per ecosystem: `*.test.*` / `*.spec.*` naming or a
/// `__tests__/` directory for TS/JS; `test_*.py` / `*_test.py` naming or a
/// `tests/` directory for Python.
pub(crate) fn is_test_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    // Directory components only — the basename is judged by naming.
    let in_dir = |dir: &str| path.split('/').rev().skip(1).any(|c| c == dir);
    if extension(path) == "py" {
        return base.starts_with("test_") || base.ends_with("_test.py") || in_dir("tests");
    }
    base.contains(".test.") || base.contains(".spec.") || in_dir("__tests__")
}

/// Stella's own workspace state — never a test input, and always present as
/// an untracked/dirty path once the graph index exists, so it must not
/// poison the change set.
fn workspace_state(path: &str) -> bool {
    path == ".stella" || path.starts_with(".stella/")
}

/// Whether `path` is safe to splice into the shell command line `run_tests`
/// composes: a conservative allowlist (alphanumerics plus `/ . _ - @`, no
/// leading `-`), so a repository-controlled filename can never smuggle
/// shell syntax or an option into the runner invocation. Anything else
/// stands selection down — loudly, per the module contract.
pub(crate) fn shell_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('-')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '@'))
}

/// The one-line stand-down note for a changed file whose language has no
/// resolved import edges — named per ecosystem so the reader learns *why*.
fn unresolved_note(path: &str) -> String {
    match extension(path) {
        "rs" => format!(
            "impact selection unavailable for Rust (`{path}`) until import resolution \
             lands (#335) — ran the full suite"
        ),
        "go" => format!(
            "impact selection unavailable for Go (`{path}`): bare imports are indexed \
             unresolved — ran the full suite"
        ),
        _ => format!(
            "impact selection unavailable for `{path}`: no resolved import edges for \
             this file type — ran the full suite"
        ),
    }
}

/// The pre-walk gate: drop Stella state paths, then answer early when the
/// tree is clean or any changed file's language lacks resolved edges.
/// `Err` carries the early [`ImpactSelection`] so callers can gate before
/// paying for a graph open/build.
fn gate(changed: &[String]) -> Result<Vec<String>, ImpactSelection> {
    let relevant: Vec<String> = changed
        .iter()
        .filter(|p| !workspace_state(p))
        .cloned()
        .collect();
    if relevant.is_empty() {
        return Err(ImpactSelection::NothingImpacted { changed: 0 });
    }
    if let Some(unresolved) = relevant.iter().find(|p| !language_resolved(p)) {
        return Err(ImpactSelection::FullSuite {
            note: unresolved_note(unresolved),
        });
    }
    Ok(relevant)
}

/// The transitive walk: BFS over the reverse-dependency oracle from every
/// changed file, keeping reachable test files. `importers` answers "which
/// files' imports resolve to this path" (the graph's `importer_paths`);
/// the computation is pure over that relation, so it is testable without
/// git or SQLite. `BTreeSet` makes the selection order deterministic.
fn walk(changed: &[String], importers: &mut dyn FnMut(&str) -> Vec<String>) -> ImpactSelection {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for path in changed {
        if visited.insert(path.clone()) {
            queue.push_back(path.clone());
        }
    }
    while let Some(path) = queue.pop_front() {
        for importer in importers(&path) {
            if visited.insert(importer.clone()) {
                queue.push_back(importer);
            }
        }
    }
    let tests: Vec<String> = visited.into_iter().filter(|p| is_test_file(p)).collect();
    if tests.is_empty() {
        return ImpactSelection::NothingImpacted {
            changed: changed.len(),
        };
    }
    if let Some(unsafe_path) = tests.iter().find(|p| !shell_safe(p)) {
        return ImpactSelection::FullSuite {
            note: format!(
                "impact selection stood down: selected test path `{unsafe_path}` contains \
                 shell-unsafe characters — ran the full suite"
            ),
        };
    }
    ImpactSelection::Selected {
        tests,
        changed: changed.len(),
    }
}

/// [`gate`] + [`walk`] — the whole pure selection over an explicit change
/// set and importer oracle. The seam the unit tests drive.
pub(crate) fn select(
    changed: &[String],
    importers: &mut dyn FnMut(&str) -> Vec<String>,
) -> ImpactSelection {
    match gate(changed) {
        Err(early) => early,
        Ok(relevant) => walk(&relevant, importers),
    }
}

/// Changed paths parsed from `git status --porcelain` output. Rename rows
/// (`R  old -> new`) contribute BOTH sides: importers of the old path still
/// hold edges to it, and the new path carries the content.
fn parse_porcelain(porcelain: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in porcelain.lines() {
        if line.len() < 4 {
            continue;
        }
        let rest = &line[3..];
        if let Some((old, new)) = rest.split_once(" -> ") {
            out.push(old.to_string());
            out.push(new.to_string());
        } else {
            out.push(rest.to_string());
        }
    }
    out
}

/// The working-tree change set: staged + unstaged + untracked, with `-uall`
/// so files inside brand-new directories are listed individually rather
/// than as a collapsed `dir/` row.
async fn changed_files(root: &Path) -> Result<Vec<String>, String> {
    let args: Vec<String> = ["status", "--porcelain", "-uall"]
        .into_iter()
        .map(String::from)
        .collect();
    let (code, output) = exec::run_argv("git", &args, root, GIT_STATUS_TIMEOUT_SECS).await?;
    if code != 0 {
        return Err(format!(
            "`git status --porcelain` exited {code}: {}",
            output.trim()
        ));
    }
    Ok(parse_porcelain(&output))
}

/// The effectful wrapper: diff the working tree, open (or build) the code
/// graph, and run the pure selection. Every hard failure — git missing,
/// graph store unopenable, a read error mid-walk — degrades to the full
/// suite WITH a note: `scope:"impacted"` never turns a runnable suite into
/// an error, and never narrows silently.
pub(crate) async fn select_impacted(root: &Path) -> ImpactSelection {
    let changed = match changed_files(root).await {
        Ok(changed) => changed,
        Err(e) => {
            return ImpactSelection::FullSuite {
                note: format!(
                    "impact selection unavailable (cannot diff the working tree: {e}) — \
                     ran the full suite"
                ),
            };
        }
    };
    // Gate before the graph: a clean tree or an unresolvable language never
    // pays for an index open/build.
    let relevant = match gate(&changed) {
        Ok(relevant) => relevant,
        Err(early) => return early,
    };
    let graph = match crate::graph::open_or_build(root) {
        Ok(graph) => graph,
        Err(e) => {
            return ImpactSelection::FullSuite {
                note: format!(
                    "impact selection unavailable (cannot open the code graph: {e}) — \
                     ran the full suite"
                ),
            };
        }
    };
    // A store read error mid-walk would make importers look empty — an
    // under-selection — so it is recorded and turned into a loud stand-down
    // instead of being swallowed.
    let mut read_error: Option<String> = None;
    let mut importers = |path: &str| match graph.importer_paths(Path::new(path)) {
        Ok(list) => list,
        Err(e) => {
            read_error = Some(e.to_string());
            Vec::new()
        }
    };
    let selection = select(&relevant, &mut importers);
    graph.shutdown();
    if let Some(e) = read_error {
        return ImpactSelection::FullSuite {
            note: format!(
                "impact selection unavailable (code-graph read failed: {e}) — ran the \
                 full suite"
            ),
        };
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A map-backed importer oracle for the pure walk.
    fn oracle(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(to, froms)| {
                (
                    to.to_string(),
                    froms.iter().map(|f| f.to_string()).collect(),
                )
            })
            .collect()
    }

    fn run_select(changed: &[&str], edges: &[(&str, &[&str])]) -> ImpactSelection {
        let map = oracle(edges);
        let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
        let mut importers = |path: &str| map.get(path).cloned().unwrap_or_default();
        select(&changed, &mut importers)
    }

    #[test]
    fn test_file_heuristics_per_ecosystem() {
        for test in [
            "a.test.ts",
            "src/b.spec.js",
            "src/__tests__/c.tsx",
            "tests/test_api.py",
            "pkg/util_test.py",
            "pkg/tests/helpers.py",
            "test_main.py",
        ] {
            assert!(is_test_file(test), "{test} is a test file");
        }
        for not_test in [
            "src/x.ts",
            "src/testing.ts",
            "contest.py",
            "src/latest.js",
            "protester.py",
        ] {
            assert!(!is_test_file(not_test), "{not_test} is not a test file");
        }
    }

    /// The issue-#334 selection contract at the pure layer: test A reaches
    /// changed X transitively, test B only reaches unrelated Y — A is
    /// selected, B is not, and a changed test file selects itself.
    #[test]
    fn walk_selects_transitive_importer_tests_and_not_unrelated_ones() {
        let edges: &[(&str, &[&str])] = &[
            ("src/x.ts", &["src/mid.ts"]),
            ("src/mid.ts", &["a.test.ts"]),
            ("src/y.ts", &["b.test.ts"]),
        ];
        match run_select(&["src/x.ts"], edges) {
            ImpactSelection::Selected { tests, changed } => {
                assert_eq!(tests, vec!["a.test.ts".to_string()]);
                assert_eq!(changed, 1);
            }
            other => panic!("expected a selection, got {other:?}"),
        }
        // A changed test file is itself in the impacted set.
        match run_select(&["b.test.ts"], edges) {
            ImpactSelection::Selected { tests, .. } => {
                assert_eq!(tests, vec!["b.test.ts".to_string()]);
            }
            other => panic!("expected the changed test itself, got {other:?}"),
        }
    }

    #[test]
    fn clean_tree_and_unreached_changes_are_named_not_silent() {
        assert_eq!(
            run_select(&[], &[]),
            ImpactSelection::NothingImpacted { changed: 0 }
        );
        // Stella's own state never counts as a change.
        assert_eq!(
            run_select(&[".stella/private/codegraph.db"], &[]),
            ImpactSelection::NothingImpacted { changed: 0 }
        );
        // A resolved-language change no test reaches: nothing impacted,
        // with the change count carried for the caller's message.
        assert_eq!(
            run_select(&["src/x.ts"], &[]),
            ImpactSelection::NothingImpacted { changed: 1 }
        );
    }

    #[test]
    fn unresolved_languages_stand_down_to_the_full_suite_loudly() {
        match run_select(&["src/lib.rs"], &[]) {
            ImpactSelection::FullSuite { note } => {
                assert!(note.contains("Rust"), "{note}");
                assert!(note.contains("#335"), "{note}");
                assert!(note.contains("full suite"), "{note}");
            }
            other => panic!("Rust must stand down loudly: {other:?}"),
        }
        match run_select(&["pkg/main.go"], &[]) {
            ImpactSelection::FullSuite { note } => {
                assert!(note.contains("Go"), "{note}");
            }
            other => panic!("Go must stand down loudly: {other:?}"),
        }
        // One unresolvable file poisons the whole set — a mixed change is
        // exactly where partial selection would silently under-test.
        match run_select(&["src/x.ts", "config.toml"], &[]) {
            ImpactSelection::FullSuite { note } => {
                assert!(note.contains("config.toml"), "{note}");
            }
            other => panic!("mixed change must stand down: {other:?}"),
        }
    }

    #[test]
    fn shell_unsafe_selected_paths_stand_down_instead_of_composing() {
        let edges: &[(&str, &[&str])] = &[("src/x.ts", &["evil;rm -rf.test.ts"])];
        match run_select(&["src/x.ts"], edges) {
            ImpactSelection::FullSuite { note } => {
                assert!(note.contains("shell-unsafe"), "{note}");
            }
            other => panic!("unsafe path must stand down: {other:?}"),
        }
        assert!(shell_safe("src/a-b_c.test.ts"));
        assert!(shell_safe("@scope/pkg/a.test.ts"));
        assert!(!shell_safe("-rf.test.ts"), "option-shaped");
        assert!(!shell_safe("a b.test.ts"), "whitespace");
        assert!(!shell_safe("a$(x).test.ts"), "substitution");
    }

    #[test]
    fn porcelain_parse_handles_renames_and_short_lines() {
        let out = " M src/x.ts\n?? new.test.ts\nR  old.ts -> moved.ts\n\nX\n";
        assert_eq!(
            parse_porcelain(out),
            vec![
                "src/x.ts".to_string(),
                "new.test.ts".to_string(),
                "old.ts".to_string(),
                "moved.ts".to_string(),
            ]
        );
    }
}
