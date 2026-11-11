//! The agent instructions stay small enough to be read whole, and point only
//! at what exists.
//!
//! `AGENTS.md` is loaded into every agent session before any work, so its cost
//! is paid by every task and a rule buried in it is a rule not followed. The
//! root file holds the rules, a nested one only what differs in its directory,
//! and a skill routes to the docs, the long form, which a session reads when
//! its task needs it. Nothing fails when one of them grows a paragraph at a
//! time, so the budgets are held here:
//!
//! - the root `AGENTS.md`, any other `AGENTS.md`, and the chain from the root
//!   to any directory, which one harness stops reading at `CHAIN_MAX`;
//! - each `SKILL.md` and the `pending.md` beside it, where an invariant waits
//!   for the docs' next sync, so a full one is a sync that is due;
//! - a `CLAUDE.md` link beside each `AGENTS.md` and a `.claude/skills` link to
//!   each skill, so every harness reads the same file;
//! - the root's `## Skills` section naming exactly the skills there are;
//! - every relative link resolving, and every `docs/<section>/<page>` naming a
//!   page of the docs checkout beside this one, when there is one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;

const ROOT_MAX: usize = 8 * 1024;
const NESTED_MAX: usize = 4 * 1024;
const CHAIN_MAX: usize = 32 * 1024;
const SKILL_MAX: usize = 3 * 1024;
const PENDING_MAX: usize = 8 * 1024;

const INSTRUCTIONS: &str = "AGENTS.md";
const ALIAS: &str = "CLAUDE.md";
const SKILLS: &str = ".agents/skills";
const SKILL_LINKS: &str = ".claude/skills";
const SKILLS_HEADING: &str = "## Skills";
const DOCS_ENV: &str = "SLOPOS_DOCS";
const DOCS_SIBLING: &str = "../slopos-docs";
const DOCS_PAGES: &str = "content/docs";

pub fn run(repo: &Repo) -> Result<Report> {
    let docs = std::env::var_os(DOCS_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.root.join(DOCS_SIBLING));
    check(repo, &docs)
}

fn check(repo: &Repo, docs: &Path) -> Result<Report> {
    let mut report = Report::new();
    let root = repo.read(INSTRUCTIONS)?;
    let instructions = repo.files_named(INSTRUCTIONS)?;
    let mut texts: Vec<(String, String)> = Vec::new();

    let mut longest_chain = 0;
    for path in &instructions {
        let text = repo.read(path)?;
        let (max, what) = if path == INSTRUCTIONS {
            (ROOT_MAX, "the root file is loaded into every session")
        } else {
            (NESTED_MAX, "a nested file holds only what differs here")
        };
        over(&mut report, path, text.len(), max, what);
        let chain: usize = instructions
            .iter()
            .filter(|other| dir_of(path).starts_with(dir_of(other)))
            .map(|other| repo.read(other).map_or(0, |t| t.len()))
            .sum();
        longest_chain = longest_chain.max(chain);
        if chain > CHAIN_MAX {
            report.finding(
                path.clone(),
                format!(
                    "the instruction files from the root to here total {chain} bytes, past the \
                     {CHAIN_MAX} a harness reads"
                ),
            );
        }
        let alias = dir_of(path).join(ALIAS);
        if !links_to(&repo.root.join(&alias), Path::new(INSTRUCTIONS)) {
            report.finding(
                alias.to_string_lossy(),
                format!("must be a symlink to `{INSTRUCTIONS}`, so every harness reads one file"),
            );
        }
        texts.push((path.clone(), text));
    }

    let skills = skill_names(repo)?;
    for name in &skills {
        let dir = format!("{SKILLS}/{name}");
        let path = format!("{dir}/SKILL.md");
        let text = repo.read(&path)?;
        over(
            &mut report,
            &path,
            text.len(),
            SKILL_MAX,
            "a skill routes to the docs",
        );
        if let Some(problem) = frontmatter_problem(&text, name) {
            report.finding(path.clone(), problem);
        }
        let link = format!("{SKILL_LINKS}/{name}/SKILL.md");
        let target = format!("../../../{path}");
        if !links_to(&repo.path(&link), Path::new(&target)) {
            report.finding(link, format!("must be a symlink to `{target}`"));
        }
        texts.push((path, text));
        let pending = format!("{dir}/pending.md");
        if repo.exists(&pending) {
            let text = repo.read(&pending)?;
            over(
                &mut report,
                &pending,
                text.len(),
                PENDING_MAX,
                "move what it holds into the docs (the `docs-sync` skill) and empty it",
            );
            texts.push((pending, text));
        }
    }

    let listed = listed_skills(&root);
    for name in skills.difference(&listed) {
        report.finding(
            INSTRUCTIONS,
            format!(
                "skill `{name}` is not named under `{SKILLS_HEADING}`, so no session is told \
                     when to read it"
            ),
        );
    }
    for name in listed.difference(&skills) {
        report.finding(
            INSTRUCTIONS,
            format!("`{SKILLS_HEADING}` names `{name}`, and `{SKILLS}/{name}/SKILL.md` is absent"),
        );
    }

    let pages = docs.join(DOCS_PAGES);
    let resolve_pages = pages.is_dir();
    let mut named_pages = BTreeSet::new();
    for (path, text) in &texts {
        for (line, target) in relative_links(text) {
            if !repo.root.join(dir_of(path)).join(&target).exists() {
                report.at(
                    path,
                    line,
                    format!("link to `{target}` resolves to nothing"),
                );
            }
        }
        for (line, page) in doc_pages(text) {
            if resolve_pages && !pages.join(format!("{page}.mdx")).is_file() {
                report.at(
                    path,
                    line,
                    format!("`docs/{page}` names no page under {}", pages.display()),
                );
            }
            named_pages.insert(page);
        }
    }

    let resolved = if resolve_pages {
        format!("resolved in {}", docs.display())
    } else {
        format!("not resolved, no docs checkout at {}", docs.display())
    };
    report.summary(format!(
        "{} instruction file(s), root {} of {ROOT_MAX} bytes, longest chain {longest_chain} of \
         {CHAIN_MAX}; {} skill(s); {} docs page(s) named, {resolved}",
        instructions.len(),
        root.len(),
        skills.len(),
        named_pages.len(),
    ));
    Ok(report)
}

fn over(report: &mut Report, path: &str, len: usize, max: usize, why: &str) {
    if len > max {
        report.finding(
            path,
            format!("{len} bytes, over the {max} it may hold: {why}"),
        );
    }
}

fn dir_of(path: &str) -> &Path {
    Path::new(path).parent().unwrap_or(Path::new(""))
}

fn links_to(link: &Path, target: &Path) -> bool {
    std::fs::read_link(link).is_ok_and(|to| to == target)
}

fn skill_names(repo: &Repo) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let dir = repo.path(SKILLS);
    if !dir.is_dir() {
        return Ok(names);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

/// What is wrong with a skill's frontmatter, which is all a session sees of
/// the skill before it decides to read it.
fn frontmatter_problem(text: &str, name: &str) -> Option<String> {
    let Some(body) = text.strip_prefix("---\n") else {
        return Some("does not open with a `---` frontmatter block".into());
    };
    let Some(end) = body.find("\n---") else {
        return Some("frontmatter block is never closed".into());
    };
    let front = &body[..end];
    let named = front
        .lines()
        .find_map(|l| l.strip_prefix("name:"))
        .map(str::trim);
    if named != Some(name) {
        return Some(format!(
            "frontmatter `name` must be the directory's, `{name}`"
        ));
    }
    let described = front.split_once("description:").is_some_and(|(_, rest)| {
        rest.lines()
            .take_while(|l| l.starts_with(' ') || l.is_empty() || rest.starts_with(l))
            .any(|l| !l.trim().trim_start_matches(['>', '|', '-']).is_empty())
    });
    if !described {
        return Some("frontmatter has no `description`, which is how a session finds it".into());
    }
    None
}

/// The skill names the root file's skills section lists: the code spans in
/// the first cell of its table rows.
fn listed_skills(root: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut inside = false;
    for line in root.lines() {
        if line.starts_with("## ") {
            inside = line.trim_end() == SKILLS_HEADING;
            continue;
        }
        if !inside {
            continue;
        }
        let Some(cell) = line.strip_prefix('|').and_then(|l| l.split('|').next()) else {
            continue;
        };
        names.extend(code_spans(cell).map(str::to_string));
    }
    names
}

fn code_spans(line: &str) -> impl Iterator<Item = &str> {
    line.split('`').skip(1).step_by(2)
}

/// `(line, page)` for every code span of the form `docs/<section>/<page>`.
fn doc_pages(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for span in code_spans(line) {
            let Some(page) = span.strip_prefix("docs/") else {
                continue;
            };
            let slug = |s: &str| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            };
            if page.split('/').count() == 2 && page.split('/').all(slug) {
                out.push((n + 1, page.to_string()));
            }
        }
    }
    out
}

/// `(line, target)` for every markdown link to a path, fragment removed.
fn relative_links(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find("](") {
            rest = &rest[at + 2..];
            let Some(end) = rest.find(')') else { break };
            let target = rest[..end].split('#').next().unwrap_or_default();
            let external = target.contains("://") || target.starts_with("mailto:");
            if !target.is_empty() && !external {
                out.push((n + 1, target.to_string()));
            }
            rest = &rest[end..];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const ROOT: &str = "# Rules\n\nRead [the plan](plans/README.md).\n\n## Skills\n\n\
        | Skill | Touching |\n|---|---|\n| `storage` | `fs/` and `ext4-core` |\n\n## After\n\n\
        | `other` | not a skill |\n";
    const SKILL: &str = "---\nname: storage\ndescription: >\n  The block layer. Use when \
        changing `fs/`.\n---\n\n- `docs/architecture/storage`: how a write lands\n";

    fn link(tree: &Tree, link: &str, target: &str) {
        let path = tree.repo.path(link);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    fn sound() -> Tree {
        let tree = Tree::new(&[
            ("AGENTS.md", ROOT),
            ("plans/README.md", ""),
            ("fs/AGENTS.md", "Bump the dcache generation.\n"),
            (".agents/skills/storage/SKILL.md", SKILL),
            ("_docs/content/docs/architecture/storage.mdx", ""),
        ]);
        link(&tree, "CLAUDE.md", "AGENTS.md");
        link(&tree, "fs/CLAUDE.md", "AGENTS.md");
        link(
            &tree,
            ".claude/skills/storage/SKILL.md",
            "../../../.agents/skills/storage/SKILL.md",
        );
        tree
    }

    fn scan(tree: &Tree) -> Report {
        check(&tree.repo, &tree.repo.path("_docs")).unwrap()
    }

    fn messages(report: &Report) -> Vec<String> {
        let mut out: Vec<String> = report
            .findings
            .iter()
            .map(|f| format!("{}: {}", f.location, f.message))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_sound_tree_passes_and_says_what_it_held() {
        let report = scan(&sound());
        assert!(report.passed(), "{:?}", messages(&report));
        assert!(
            report.summary.contains("2 instruction file(s)")
                && report.summary.contains("1 skill(s)")
                && report.summary.contains("1 docs page(s) named, resolved"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn without_a_docs_checkout_pages_are_named_but_not_resolved() {
        let tree = sound();
        let report = check(&tree.repo, &tree.repo.path("absent")).unwrap();
        assert!(report.passed(), "{:?}", messages(&report));
        assert!(
            report.summary.contains("not resolved"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn each_file_is_held_to_its_own_budget() {
        let tree = sound();
        tree.write("AGENTS.md", &format!("{ROOT}{}", "x".repeat(ROOT_MAX)));
        tree.write("fs/AGENTS.md", &"x".repeat(NESTED_MAX + 1));
        tree.write(
            ".agents/skills/storage/SKILL.md",
            &format!("{SKILL}{}", "x".repeat(SKILL_MAX)),
        );
        tree.write(
            ".agents/skills/storage/pending.md",
            &"x".repeat(PENDING_MAX + 1),
        );
        let found = messages(&scan(&tree));
        assert_eq!(found.len(), 4, "{found:?}");
        for (path, max) in [
            (".agents/skills/storage/SKILL.md", SKILL_MAX),
            (".agents/skills/storage/pending.md", PENDING_MAX),
            ("AGENTS.md", ROOT_MAX),
            ("fs/AGENTS.md", NESTED_MAX),
        ] {
            assert!(
                found.iter().any(|f| f.starts_with(&format!("{path}: "))
                    && f.contains(&format!("over the {max}"))),
                "{path}: {found:?}"
            );
        }
    }

    #[test]
    fn files_at_their_budget_pass() {
        let tree = sound();
        tree.write("fs/AGENTS.md", &"x".repeat(NESTED_MAX));
        tree.write(
            ".agents/skills/storage/pending.md",
            &"x".repeat(PENDING_MAX),
        );
        let report = scan(&tree);
        assert!(report.passed(), "{:?}", messages(&report));
    }

    #[test]
    fn a_chain_past_what_a_harness_reads_fails_where_it_crosses() {
        let tree = sound();
        let mut dir = String::from("fs");
        for depth in 0..CHAIN_MAX / NESTED_MAX {
            dir.push_str(&format!("/d{depth}"));
            tree.write(&format!("{dir}/AGENTS.md"), &"x".repeat(NESTED_MAX));
            link(&tree, &format!("{dir}/CLAUDE.md"), "AGENTS.md");
        }
        let found = messages(&scan(&tree));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("fs/d0/d1/d2/d3/d4/d5/d6/d7/AGENTS.md: ")
                && found[0].contains("total"),
            "{found:?}"
        );
    }

    #[test]
    fn an_alias_that_is_a_copy_or_absent_fails() {
        let tree = sound();
        std::fs::remove_file(tree.repo.path("CLAUDE.md")).unwrap();
        tree.write("CLAUDE.md", ROOT);
        std::fs::remove_file(tree.repo.path("fs/CLAUDE.md")).unwrap();
        std::fs::remove_file(tree.repo.path(".claude/skills/storage/SKILL.md")).unwrap();
        let found = messages(&scan(&tree));
        assert_eq!(
            found
                .iter()
                .map(|f| f.split(':').next().unwrap())
                .collect::<Vec<_>>(),
            [
                ".claude/skills/storage/SKILL.md",
                "CLAUDE.md",
                "fs/CLAUDE.md"
            ],
            "{found:?}"
        );
    }

    #[test]
    fn a_skill_is_found_by_its_frontmatter() {
        let tree = sound();
        for (text, expect) in [
            ("name: storage\n", "does not open"),
            ("---\nname: storage\ndescription: x\n", "never closed"),
            (
                "---\nname: disks\ndescription: x\n---\n",
                "must be the directory's",
            ),
            ("---\nname: storage\n---\n", "no `description`"),
            (
                "---\nname: storage\ndescription: >\n---\n",
                "no `description`",
            ),
        ] {
            tree.write(".agents/skills/storage/SKILL.md", text);
            let found = messages(&scan(&tree));
            assert_eq!(found.len(), 1, "{text:?}: {found:?}");
            assert!(found[0].contains(expect), "{text:?}: {found:?}");
        }
        tree.write(
            ".agents/skills/storage/SKILL.md",
            "---\nname: storage\ndescription: One line.\n---\n",
        );
        assert!(scan(&tree).passed());
    }

    #[test]
    fn the_root_names_exactly_the_skills_there_are() {
        let tree = sound();
        tree.write(
            ".agents/skills/orphan/SKILL.md",
            "---\nname: orphan\ndescription: x\n---\n",
        );
        link(
            &tree,
            ".claude/skills/orphan/SKILL.md",
            "../../../.agents/skills/orphan/SKILL.md",
        );
        tree.write(
            "AGENTS.md",
            &ROOT.replace("| `storage` |", "| `storage`, `ghost` |"),
        );
        let found = messages(&scan(&tree));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("names `ghost`"), "{found:?}");
        assert!(
            found[1].contains("skill `orphan` is not named"),
            "{found:?}"
        );
    }

    #[test]
    fn a_link_or_a_docs_page_that_names_nothing_fails_at_its_line() {
        let tree = sound();
        tree.write(
            "fs/AGENTS.md",
            "See [the guide](GUIDE.md#top), [upstream](https://example.org/x) and\n\
             `docs/architecture/missing`; `docs/architecture/storage` is fine, and\n\
             `docs/a/b/c`, `docs/Architecture/x` and `scripts/docs/x` are not pages.\n",
        );
        let found = messages(&scan(&tree));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].starts_with("fs/AGENTS.md:1: link to `GUIDE.md`"),
            "{found:?}"
        );
        assert!(
            found[1].starts_with("fs/AGENTS.md:2: `docs/architecture/missing`"),
            "{found:?}"
        );
    }
}
