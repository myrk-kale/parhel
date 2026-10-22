//! Minimal list of filesystem operations that clones the source minus exclusions.
//!
//! `clonefile(2)` copies a whole directory tree in one call, so the plan only
//! descends into directories that are ancestors of an excluded path; every other
//! entry is cloned whole.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::error::{Error, ErrorCode, Result};
use super::exclusions::Exclusions;

/// Read-only view of the source tree, relative to its root (`""` is the root).
pub trait DirLister {
    /// Entry names of the directory at `relative`, in any order.
    fn list(&self, relative: &Path) -> io::Result<Vec<OsString>>;
}

/// One step of the clone, paths relative to the source/staging roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneOp {
    /// Create an empty directory (an ancestor of an excluded path).
    CreateDir(PathBuf),
    /// `clonefile` this entry (file, symlink or whole directory) as-is.
    Clone(PathBuf),
}

/// Build the plan. Ops are ordered parent-before-child and, within a directory,
/// by entry name, so runs are deterministic. Listing failures map to `clone-failed`.
pub fn plan(tree: &dyn DirLister, exclusions: &Exclusions) -> Result<Vec<CloneOp>> {
    let mut ops = Vec::new();
    let root = list(tree, Path::new(""))?.unwrap_or_default();
    descend(tree, exclusions, Path::new(""), root, &mut ops)?;
    Ok(ops)
}

/// Sorted entries of `dir`, or `None` when it is not a directory (a file that merely
/// sits where an exclusion path would continue).
fn list(tree: &dyn DirLister, dir: &Path) -> Result<Option<Vec<OsString>>> {
    match tree.list(dir) {
        Ok(mut names) => {
            names.sort();
            Ok(Some(names))
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(Error::new(
            ErrorCode::CloneFailed,
            format!("cannot list `{}`: {e}", dir.display()),
        )),
    }
}

fn descend(
    tree: &dyn DirLister,
    exclusions: &Exclusions,
    dir: &Path,
    names: Vec<OsString>,
    ops: &mut Vec<CloneOp>,
) -> Result<()> {
    for name in names {
        let relative = dir.join(&name);
        if exclusions.is_excluded(&relative) {
            continue;
        }
        if exclusions.is_ancestor_of_excluded(&relative) {
            if let Some(children) = list(tree, &relative)? {
                ops.push(CloneOp::CreateDir(relative.clone()));
                descend(tree, exclusions, &relative, children, ops)?;
                continue;
            }
        }
        ops.push(CloneOp::Clone(relative));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;
    use crate::domain::exclusions::ExclusionInput;
    use std::collections::BTreeMap;

    /// In-memory tree: directory path -> entries. Paths absent from the map are leaves.
    struct MemTree(BTreeMap<PathBuf, Vec<OsString>>);

    impl MemTree {
        fn new(dirs: &[(&str, &[&str])]) -> Self {
            Self(
                dirs.iter()
                    .map(|(d, es)| (PathBuf::from(d), es.iter().map(OsString::from).collect()))
                    .collect(),
            )
        }
    }

    impl DirLister for MemTree {
        fn list(&self, relative: &Path) -> io::Result<Vec<OsString>> {
            self.0
                .get(relative)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not a dir"))
        }
    }

    struct FailingTree;
    impl DirLister for FailingTree {
        fn list(&self, _: &Path) -> io::Result<Vec<OsString>> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    fn exclusions(worktrees: &[&str], submodules: &[&str]) -> Exclusions {
        Exclusions::compute(&ExclusionInput {
            source_root: PathBuf::from("/src"),
            staging_dir: None,
            other_worktrees: worktrees.iter().map(PathBuf::from).collect(),
            submodule_paths: submodules.iter().map(PathBuf::from).collect(),
            exclude_submodules: true,
        })
    }

    fn clone(p: &str) -> CloneOp {
        CloneOp::Clone(PathBuf::from(p))
    }
    fn mkdir(p: &str) -> CloneOp {
        CloneOp::CreateDir(PathBuf::from(p))
    }

    // U-PLAN-01: only `.git` excluded -> every top-level entry cloned whole, sorted.
    #[test]
    fn clones_top_level_entries_except_dot_git() {
        let tree = MemTree::new(&[("", &["src", ".git", "README.md", "node_modules"])]);
        let ops = plan(&tree, &exclusions(&[], &[])).unwrap();
        assert_eq!(
            ops,
            vec![clone("README.md"), clone("node_modules"), clone("src")]
        );
    }

    // U-PLAN-02: descends only along the path to an exclusion.
    #[test]
    fn descends_only_into_ancestors_of_exclusions() {
        let tree = MemTree::new(&[
            ("", &[".git", "libs", "app"]),
            ("libs", &["a", "b", "c"]),
            ("libs/a", &["x"]),
        ]);
        let ops = plan(&tree, &exclusions(&[], &["libs/b"])).unwrap();
        assert_eq!(
            ops,
            vec![
                clone("app"),
                mkdir("libs"),
                clone("libs/a"),
                clone("libs/c")
            ]
        );
    }

    // U-PLAN-03: deep exclusion creates every intermediate directory, parent first.
    #[test]
    fn deep_exclusion_creates_intermediate_dirs_in_order() {
        let tree = MemTree::new(&[
            ("", &[".git", "a"]),
            ("a", &["b", "keep"]),
            ("a/b", &["c", "sib"]),
            ("a/b/c", &["deep"]),
        ]);
        let ops = plan(&tree, &exclusions(&["/src/a/b/c/deep"], &[])).unwrap();
        assert_eq!(
            ops,
            vec![
                mkdir("a"),
                mkdir("a/b"),
                mkdir("a/b/c"),
                clone("a/b/sib"),
                clone("a/keep"),
            ]
        );
    }

    // U-PLAN-04: exclusion that does not exist on disk is harmless.
    #[test]
    fn missing_excluded_path_is_harmless() {
        let tree = MemTree::new(&[("", &[".git", "a"])]);
        let ops = plan(&tree, &exclusions(&[], &["gone/sub"])).unwrap();
        assert_eq!(ops, vec![clone("a")]);
    }

    // U-PLAN-05: source with nothing but `.git` -> empty plan.
    #[test]
    fn empty_source_yields_empty_plan() {
        let tree = MemTree::new(&[("", &[".git"])]);
        assert_eq!(
            plan(&tree, &exclusions(&[], &[])).unwrap(),
            Vec::<CloneOp>::new()
        );
    }

    // U-PLAN-06: listing failure maps to clone-failed.
    #[test]
    fn listing_failure_is_clone_failed() {
        let err = plan(&FailingTree, &exclusions(&[], &[])).unwrap_err();
        assert_eq!(err.code, ErrorCode::CloneFailed);
    }

    // U-PLAN-07: names that merely share a prefix with an exclusion are cloned whole.
    #[test]
    fn prefix_lookalikes_are_cloned_whole() {
        let tree = MemTree::new(&[("", &[".git", ".gitignore", "foo", "foobar"])]);
        let ops = plan(&tree, &exclusions(&[], &["foo"])).unwrap();
        assert_eq!(ops, vec![clone(".gitignore"), clone("foobar")]);
    }

    // U-PLAN-P1: applying the plan reproduces exactly the non-excluded leaves.
    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeSet;

        /// Random tree as a set of leaf paths (files) under the root.
        fn leaves() -> impl Strategy<Value = BTreeSet<PathBuf>> {
            prop::collection::btree_set(
                prop::collection::vec("[abc]", 1..4).prop_map(|v| v.iter().collect::<PathBuf>()),
                1..12,
            )
        }

        fn tree_of(leaves: &BTreeSet<PathBuf>) -> MemTree {
            let mut dirs: BTreeMap<PathBuf, BTreeSet<OsString>> = BTreeMap::new();
            dirs.entry(PathBuf::new())
                .or_default()
                .insert(OsString::from(".git"));
            for leaf in leaves {
                let mut parent = PathBuf::new();
                for c in leaf.components() {
                    dirs.entry(parent.clone())
                        .or_default()
                        .insert(c.as_os_str().to_owned());
                    parent.push(c);
                }
            }
            // A path that is both a leaf and a prefix of another leaf is a directory.
            MemTree(
                dirs.into_iter()
                    .map(|(k, v)| (k, v.into_iter().collect()))
                    .collect(),
            )
        }

        proptest! {
            #[test]
            fn plan_covers_exactly_the_non_excluded_leaves(
                leaves in leaves(),
                excluded in prop::collection::vec(
                    prop::collection::vec("[abc]", 1..3).prop_map(|v| v.iter().collect::<PathBuf>()),
                    0..3,
                ),
            ) {
                let tree = tree_of(&leaves);
                let ex = Exclusions::compute(&ExclusionInput {
                    source_root: PathBuf::from("/src"),
                    submodule_paths: excluded,
                    exclude_submodules: true,
                    ..ExclusionInput::default()
                });
                let ops = plan(&tree, &ex).unwrap();
                let cloned: Vec<&PathBuf> = ops.iter().filter_map(|o| match o {
                    CloneOp::Clone(p) => Some(p),
                    CloneOp::CreateDir(_) => None,
                }).collect();
                let real_leaves: Vec<&PathBuf> = leaves.iter().filter(|l| !tree.0.contains_key(*l)).collect();
                for leaf in real_leaves {
                    let covering = cloned.iter().filter(|c| leaf.starts_with(c)).count();
                    if ex.is_excluded(leaf) {
                        prop_assert_eq!(covering, 0, "excluded {:?} cloned", leaf);
                    } else {
                        prop_assert_eq!(covering, 1, "{:?} covered {} times", leaf, covering);
                    }
                }
                for op in &ops {
                    if let CloneOp::CreateDir(d) = op {
                        prop_assert!(ex.is_ancestor_of_excluded(d));
                    }
                }
            }
        }
    }
}
