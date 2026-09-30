//! A tripwire for documentation that names code which no longer exists.
//!
//! Under `talker/tests/` for want of a workspace-root crate, but its subject is
//! every crate's `docs/` folder — it walks up from this manifest.
//!
//! **What it proves:** an identifier a document cites in backticks still appears
//! as a whole token somewhere in the workspace's Rust source. **What it does
//! not:** that the identifier is a live item, or that the claim around it is
//! true. A name surviving only in a comment or string passes, because nothing
//! here parses Rust. It catches a rename landing while a document goes on citing
//! the old name; it cannot catch prose that describes a design in the wrong
//! tense, which has to be read for.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Identifiers a document names deliberately *because* they are gone.
///
/// Each entry claims the surrounding prose reads correctly with the identifier
/// absent. Checked for still being needed, below.
const REMOVED_ON_PURPOSE: &[(&str, &str)] = &[
    (
        "min_active_interval",
        "talker TODO names the old symbol beside the one that replaced it \
         (`active_cadence`); the sentence is about the rename",
    ),
    (
        "build_raw_recorder",
        "listener ADR-013 records what the Raw/Display split changed at the time; \
         both taps now begin in the pipeline (ADR-043)",
    ),
    (
        "build_display_recorder",
        "listener ADR-013 history, as for `build_raw_recorder`",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("talker/ has a parent")
        .to_path_buf()
}

/// Every `.md` under a `docs/` folder, plus the working agreement itself.
///
/// Recursive, so "every" stays true the first time a `docs/` folder grows a
/// subdirectory. A guard whose coverage silently depends on the shape of the
/// tree is one that reports success for documents it never opened.
fn documents(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let agreement = root.join("AGENTS.md");
    if agreement.is_file() {
        found.push(agreement);
    }
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        collect_markdown(&entry.path().join("docs"), &mut found);
    }
    found.sort();
    found
}

/// Append every `.md` at or beneath `dir`. A missing directory is not an error:
/// most workspace members have no `docs/` folder at all.
fn collect_markdown(dir: &Path, found: &mut Vec<PathBuf>) {
    for file in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = file.path();
        if path.is_dir() {
            collect_markdown(&path, found);
        } else if path.extension().is_some_and(|e| e == "md") {
            found.push(path);
        }
    }
}

/// Every identifier-shaped token in the workspace's Rust source.
///
/// Whole tokens, not substrings: `foo_bar` must not be satisfied by
/// `foo_bar_baz`, which is often the rename that broke the reference.
///
/// **This file is excluded, and that is load-bearing.** Scanning it made
/// [`REMOVED_ON_PURPOSE`] self-satisfying — an allowlisted name appears here as
/// a string literal, so the presence check succeeded and the exception was never
/// reached. The safeguard had quietly stopped guarding its own exceptions.
fn source_identifiers(root: &Path) -> HashSet<String> {
    fn walk(dir: &Path, out: &mut HashSet<String>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && !path.ends_with("tests/docs.rs")
            {
                if let Ok(text) = fs::read_to_string(&path) {
                    let mut token = String::new();
                    for c in text.chars() {
                        if c.is_alphanumeric() || c == '_' {
                            token.push(c);
                        } else if !token.is_empty() {
                            out.insert(std::mem::take(&mut token));
                        }
                    }
                    if !token.is_empty() {
                        out.insert(token);
                    }
                }
            }
        }
    }
    let mut out = HashSet::new();
    walk(root, &mut out);
    out
}

/// Backticked snake_case names with at least two underscores — long enough to be
/// an item rather than a prose word in backticks.
fn cited_identifiers(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| {
            span.matches('_').count() >= 2
                && span.starts_with(|c: char| c.is_ascii_lowercase())
                && !span.ends_with('_')
                && span
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn documents_do_not_cite_identifiers_the_source_has_dropped() {
    let root = workspace_root();
    let source = source_identifiers(&root);
    let docs = documents(&root);
    assert!(docs.len() > 5, "the walk is not reaching docs/");

    let mut dangling: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for doc in &docs {
        let Ok(text) = fs::read_to_string(doc) else {
            continue;
        };
        let relative = doc
            .strip_prefix(&root)
            .unwrap_or(doc)
            .to_string_lossy()
            .replace('\\', "/");
        for ident in cited_identifiers(&text) {
            // Allowlist first: consulted after the presence check, an exception
            // for a name that appears anywhere for any reason is unreachable.
            let allowed = REMOVED_ON_PURPOSE.iter().any(|(name, _)| *name == ident);
            if allowed || source.contains(&ident) {
                continue;
            }
            let entry = dangling.entry(relative.clone()).or_default();
            if !entry.contains(&ident) {
                entry.push(ident);
            }
        }
    }

    assert!(
        dangling.is_empty(),
        "documents cite identifiers absent from every source file. Fix the \
         reference, or add it to REMOVED_ON_PURPOSE with a reason.\n{dangling:#?}"
    );
}

/// An allowlist entry must still be needed, or it suppresses a real reference to
/// a name that has since come back.
#[test]
fn the_allowlist_holds_only_names_the_source_really_dropped() {
    let source = source_identifiers(&workspace_root());
    let live: Vec<&str> = REMOVED_ON_PURPOSE
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| source.contains(*name))
        .collect();
    assert!(
        live.is_empty(),
        "allowlisted but present; drop it: {live:?}"
    );
}
