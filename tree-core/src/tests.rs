use super::*;
use std::collections::BTreeMap;
use std::vec;

/// A root as a map of paths to what they are.
#[derive(Default)]
struct Fake(BTreeMap<String, State>);

impl Fake {
    fn with(paths: &[(&str, State)]) -> Fake {
        Fake(paths.iter().map(|(p, s)| (p.to_string(), *s)).collect())
    }

    fn apply(&mut self, plan: &Plan) {
        for step in &plan.steps {
            match step {
                Step::Remove(path) | Step::RemoveDir(path) => {
                    assert!(self.0.remove(path).is_some(), "{path} was not there");
                }
                Step::MakeDir(path) => {
                    self.0.insert(path.clone(), State::Dir);
                }
                Step::Install { item, path } => {
                    let state = match item.kind {
                        Kind::Dir => State::Dir,
                        Kind::File => State::File,
                        Kind::Link => State::Link,
                    };
                    self.0.insert(path.clone(), state);
                }
            }
        }
    }
}

impl Root for Fake {
    fn state(&mut self, path: &str) -> State {
        self.0.get(path).copied().unwrap_or(State::Absent)
    }

    fn children(&mut self, dir: &str) -> Vec<String> {
        let prefix = format!("{dir}/");
        self.0
            .keys()
            .filter_map(|p| p.strip_prefix(&prefix))
            .filter(|rest| !rest.contains('/'))
            .map(str::to_string)
            .collect()
    }
}

fn manifest(identity: char, items: &[(Kind, &str)]) -> Manifest {
    Manifest {
        identity: core::iter::repeat_n(identity, 64).collect(),
        items: items
            .iter()
            .map(|&(kind, rel)| Item {
                kind,
                size: (kind == Kind::File).then_some(rel.len() as u64),
                rel: rel.to_string(),
            })
            .collect(),
    }
}

const D: Kind = Kind::Dir;
const F: Kind = Kind::File;
const L: Kind = Kind::Link;

#[test]
fn a_manifest_reads_back_as_written() {
    let m = manifest(
        'a',
        &[
            (D, "bin"),
            (F, "bin/tool"),
            (L, "bin/alias"),
            (F, "a b/c d"),
        ],
    );
    assert_eq!(Manifest::parse(&m.render()), Ok(m.clone()));
    assert!(
        m.render()
            .starts_with(&format!("identity {}\n", "a".repeat(64)))
    );
    assert!(m.render().contains("\nf 8 bin/tool\nl - bin/alias\n"));
}

/// An unfinished manifest names its old files without their sizes.
#[test]
fn what_fs_tree_writes_parses() {
    let text = format!(
        "identity {}\nd - bin\nf 4 bin/tool\nl - bin/alias\n",
        "0123456789abcdef".repeat(4)
    );
    let m = Manifest::parse(&text).unwrap();
    assert_eq!(m.items.len(), 3);
    assert_eq!(m.items[1].size, Some(4));
    assert!(m.is_finished());
    let during = Manifest::parse(&format!("identity {UNFINISHED}\nf - bin/old\n")).unwrap();
    assert!(!during.is_finished());
    assert_eq!(during.items[0].size, None);
}

#[test]
fn a_damaged_manifest_says_where() {
    for (text, line) in [
        ("", 1),
        ("identity xyz\n", 1),
        (&*format!("identity {}\nq - x\n", "a".repeat(64)), 2),
        (&*format!("identity {}\nd 3 x\n", "a".repeat(64)), 2),
        (&*format!("identity {}\nd - ../x\n", "a".repeat(64)), 2),
        (&*format!("identity {}\nd - a//b\n", "a".repeat(64)), 2),
    ] {
        assert_eq!(Manifest::parse(text), Err(ParseError { line }), "{text:?}");
    }
}

#[test]
fn a_first_install_makes_the_directories_above_it() {
    let mut root = Fake::with(&[("/usr", State::Dir)]);
    let new = manifest('a', &[(D, "bin"), (F, "bin/tool")]);
    let plan = plan(&mut root, "/usr/local", None, &new).unwrap();
    assert_eq!(plan.steps[0], Step::MakeDir("/usr/local".to_string()));
    root.apply(&plan);
    assert_eq!(root.state("/usr/local/bin/tool"), State::File);
    assert_eq!(plan.done, new);
    assert!(!plan.during.is_finished());
}

#[test]
fn a_reinstall_replaces_what_it_installed_and_keeps_what_the_user_added() {
    let old = manifest(
        'a',
        &[(D, "bin"), (F, "bin/tool"), (D, "lib"), (F, "lib/libx.so")],
    );
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/bin", State::Dir),
        ("/usr/local/bin/tool", State::File),
        ("/usr/local/bin/mine", State::File),
        ("/usr/local/lib", State::Dir),
        ("/usr/local/lib/libx.so", State::File),
        ("/usr/local/lib/mine", State::Dir),
    ]);
    let new = manifest('b', &[(D, "bin"), (F, "bin/tool")]);
    let plan = plan(&mut root, "/usr/local", Some(&old), &new).unwrap();
    root.apply(&plan);
    assert_eq!(root.state("/usr/local/bin/tool"), State::File);
    assert_eq!(root.state("/usr/local/bin/mine"), State::File);
    assert_eq!(root.state("/usr/local/lib/libx.so"), State::Absent);
    assert_eq!(
        root.state("/usr/local/lib"),
        State::Dir,
        "a directory still holding the user's is kept"
    );
    assert_eq!(root.state("/usr/local/lib/mine"), State::Dir);
}

#[test]
fn a_directory_the_new_tree_drops_goes_once_it_holds_only_the_old_tree() {
    let old = manifest('a', &[(D, "share"), (D, "share/x"), (F, "share/x/a")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/share", State::Dir),
        ("/usr/local/share/x", State::Dir),
        ("/usr/local/share/x/a", State::File),
    ]);
    let plan = plan(&mut root, "/usr/local", Some(&old), &manifest('b', &[])).unwrap();
    assert_eq!(
        plan.steps,
        [
            Step::Remove("/usr/local/share/x/a".to_string()),
            Step::RemoveDir("/usr/local/share/x".to_string()),
            Step::RemoveDir("/usr/local/share".to_string()),
        ]
    );
}

#[test]
fn what_the_user_put_in_the_way_refuses_the_install() {
    let old = manifest('a', &[(D, "bin"), (F, "bin/tool")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/bin", State::Dir),
        ("/usr/local/bin/tool", State::Dir),
        ("/usr/local/bin/new", State::File),
        ("/usr/local/share", State::File),
    ]);
    let new = manifest('b', &[(D, "bin"), (F, "bin/new"), (D, "share")]);
    let Err(Conflicts(found)) = plan(&mut root, "/usr/local", Some(&old), &new) else {
        panic!("installed over the user's files");
    };
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(found[0].starts_with("/usr/local/bin/tool is a directory"));
    assert!(found[1].starts_with("/usr/local/bin/new is already there"));
    assert!(found[2].starts_with("/usr/local/share is a file"));
}

#[test]
fn a_file_where_the_tree_goes_refuses_the_install() {
    let mut root = Fake::with(&[("/usr", State::File)]);
    let Err(Conflicts(found)) = plan(&mut root, "/usr/local", None, &manifest('a', &[])) else {
        panic!("installed beneath a file");
    };
    assert_eq!(
        found,
        ["/usr is a file, where /usr/local needs a directory"]
    );
}

/// The manifest recorded mid-install names both trees, so an install cut
/// short anywhere is undone by the next.
#[test]
fn an_unfinished_manifest_names_both_trees() {
    let old = manifest('a', &[(D, "bin"), (F, "bin/old")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/bin", State::Dir),
        ("/usr/local/bin/old", State::File),
    ]);
    let new = manifest('b', &[(D, "bin"), (F, "bin/new")]);
    let first = plan(&mut root, "/usr/local", Some(&old), &new).unwrap();
    let rels: Vec<&str> = first.during.items.iter().map(|i| i.rel.as_str()).collect();
    assert_eq!(rels, ["bin", "bin/new", "bin/old"]);
    assert_eq!(first.during.identity, UNFINISHED);
    root.0.remove("/usr/local/bin/old");
    root.0.insert("/usr/local/bin/new".to_string(), State::File);
    let again = plan(&mut root, "/usr/local", Some(&first.during), &new).unwrap();
    root.apply(&again);
    assert_eq!(root.state("/usr/local/bin/new"), State::File);
    assert_eq!(root.state("/usr/local/bin/old"), State::Absent);
}

#[test]
fn a_tree_is_named_for_where_it_goes() {
    assert_eq!(manifest_name("/usr/local"), "usr_local");
    assert_eq!(manifest_name("/srv/ladder"), "srv_ladder");
}

#[test]
fn ancestors_are_every_directory_above_a_path() {
    assert_eq!(ancestors("/usr/local/lib"), vec!["/usr", "/usr/local"]);
    assert!(ancestors("/usr").is_empty());
}

/// A manifest the root's user edited names a file below a symlink they made:
/// a path walk would follow it out of the tree, so the install refuses.
#[test]
fn an_old_entry_below_a_symlink_is_refused() {
    let old = manifest('a', &[(D, "bin"), (F, "bin/tool"), (F, "evil/x")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/bin", State::Dir),
        ("/usr/local/bin/tool", State::File),
        ("/usr/local/evil", State::Link),
        ("/usr/local/evil/x", State::File),
    ]);
    let new = manifest('b', &[(D, "bin"), (F, "bin/tool")]);
    let conflicts = plan(&mut root, "/usr/local", Some(&old), &new).unwrap_err();
    assert_eq!(
        conflicts.0,
        ["/usr/local/evil is a symlink; the last install put a directory there"]
    );
}

#[test]
fn a_new_entry_below_a_file_is_refused() {
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/lib", State::File),
    ]);
    let new = manifest('b', &[(D, "lib"), (F, "lib/x")]);
    let conflicts = plan(&mut root, "/usr/local", None, &new).unwrap_err();
    assert_eq!(
        conflicts.0,
        ["/usr/local/lib is a file, where the new tree has a directory"]
    );
}

/// A symlink the last install made, which the new tree makes a directory, is
/// removed before anything goes below it.
#[test]
fn a_link_the_last_install_made_gives_way_to_a_directory() {
    let old = manifest('a', &[(L, "lib")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/lib", State::Link),
    ]);
    let new = manifest('b', &[(D, "lib"), (F, "lib/x")]);
    let steps = plan(&mut root, "/usr/local", Some(&old), &new).unwrap();
    root.apply(&steps);
    assert_eq!(root.state("/usr/local/lib"), State::Dir);
    assert_eq!(root.state("/usr/local/lib/x"), State::File);
}

/// An install cut short while a file became a directory: the unfinished
/// manifest names the path as both, and the redo takes it as either.
#[test]
fn a_redo_takes_a_path_whose_kind_was_changing_as_either() {
    let old = manifest('a', &[(F, "share")]);
    let new = manifest('b', &[(D, "share"), (F, "share/doc")]);
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/share", State::File),
    ]);
    let first = plan(&mut root, "/usr/local", Some(&old), &new).unwrap();
    for left in [State::Absent, State::Dir] {
        let mut cut = Fake::with(&[("/usr", State::Dir), ("/usr/local", State::Dir)]);
        if left == State::Dir {
            cut.0.insert("/usr/local/share".to_string(), State::Dir);
        }
        let again = plan(&mut cut, "/usr/local", Some(&first.during), &new).unwrap();
        cut.apply(&again);
        assert_eq!(cut.state("/usr/local/share/doc"), State::File, "{left:?}");
    }
}

/// An unfinished manifest can name a file twice, with and without its size;
/// it is removed once.
#[test]
fn a_path_named_twice_is_removed_once() {
    let mut old = manifest('a', &[(F, "bin/tool")]);
    old.items.push(Item {
        kind: F,
        size: None,
        rel: "bin/tool".to_string(),
    });
    let mut root = Fake::with(&[
        ("/usr", State::Dir),
        ("/usr/local", State::Dir),
        ("/usr/local/bin", State::Dir),
        ("/usr/local/bin/tool", State::File),
    ]);
    let steps = plan(&mut root, "/usr/local", Some(&old), &manifest('b', &[])).unwrap();
    root.apply(&steps);
    assert_eq!(root.state("/usr/local/bin/tool"), State::Absent);
}
