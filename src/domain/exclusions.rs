//! Paths of the source worktree that must never be cloned into the destination.
//!
//! Cloning any of these would make the new worktree share git state with another
//! one (the root `.git`, nested linked worktrees, submodule gitfiles) or contain
//! itself (the staging dir when the destination is nested in the source).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Facts gathered from git and the request, all paths absolute and canonical
/// except `submodule_paths`, which are relative to `source_root` (as git reports them).
#[derive(Debug, Clone, Default)]
pub struct ExclusionInput {
    pub source_root: PathBuf,
    /// Staging directory of this run (absolute). Excluded only when inside the source.
    pub staging_dir: Option<PathBuf>,
    /// Every other registered worktree of the repository (absolute), main checkout included.
    pub other_worktrees: Vec<PathBuf>,
    /// Gitlink paths (mode 160000) of the source, relative to `source_root`.
    pub submodule_paths: Vec<PathBuf>,
    /// Whether submodule directories are skipped (policy `reinit`).
    pub exclude_submodules: bool,
}

/// Normalized set of excluded paths, relative to the source root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Exclusions {
    paths: BTreeSet<PathBuf>,
}

impl Exclusions {
    /// Build the set. The root `.git` entry is always excluded; entries outside the
    /// source root are dropped; an entry nested under another excluded entry is
    /// redundant and dropped.
    #[must_use]
    pub fn compute(input: &ExclusionInput) -> Self {
        let root = &input.source_root;
        let nested = |abs: &PathBuf| -> Option<PathBuf> {
            abs.strip_prefix(root)
                .ok()
                .filter(|rel| rel.components().next().is_some())
                .map(Path::to_path_buf)
        };
        let mut candidates: BTreeSet<PathBuf> = BTreeSet::from([PathBuf::from(".git")]);
        candidates.extend(input.staging_dir.iter().filter_map(nested));
        candidates.extend(input.other_worktrees.iter().filter_map(nested));
        if input.exclude_submodules {
            candidates.extend(
                input
                    .submodule_paths
                    .iter()
                    .filter(|p| p.components().next().is_some())
                    .cloned(),
            );
        }
        let paths = candidates
            .iter()
            .filter(|p| !candidates.iter().any(|q| q != *p && p.starts_with(q)))
            .cloned()
            .collect();
        Self { paths }
    }

    /// Whether `relative` itself is excluded (component-wise, not string prefix).
    #[must_use]
    pub fn is_excluded(&self, relative: &Path) -> bool {
        self.paths.iter().any(|p| relative.starts_with(p))
    }

    /// Whether `relative` is a strict ancestor of an excluded path, i.e. the clone
    /// must descend into it instead of cloning it whole.
    #[must_use]
    pub fn is_ancestor_of_excluded(&self, relative: &Path) -> bool {
        self.paths
            .iter()
            .any(|p| p != relative && p.starts_with(relative))
    }

    /// Excluded paths in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(root: &str) -> ExclusionInput {
        ExclusionInput {
            source_root: PathBuf::from(root),
            ..ExclusionInput::default()
        }
    }

    fn listed(ex: &Exclusions) -> Vec<String> {
        ex.iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    // U-EXCL-01
    #[test]
    fn root_dot_git_is_always_excluded() {
        let ex = Exclusions::compute(&input("/w/repo"));
        assert_eq!(listed(&ex), vec![".git"]);
        assert!(ex.is_excluded(Path::new(".git")));
    }

    // U-EXCL-02
    #[test]
    fn staging_dir_inside_source_is_excluded() {
        let mut i = input("/w/repo");
        i.staging_dir = Some(PathBuf::from("/w/repo/.worktrees/.parhel-staging-ab"));
        let ex = Exclusions::compute(&i);
        assert!(ex.is_excluded(Path::new(".worktrees/.parhel-staging-ab")));
        assert!(ex.is_ancestor_of_excluded(Path::new(".worktrees")));
    }

    // U-EXCL-03
    #[test]
    fn staging_dir_outside_source_is_ignored() {
        let mut i = input("/w/repo");
        i.staging_dir = Some(PathBuf::from("/w/.parhel-staging-ab"));
        assert_eq!(listed(&Exclusions::compute(&i)), vec![".git"]);
    }

    // U-EXCL-04
    #[test]
    fn nested_linked_worktrees_are_excluded_and_outside_ones_ignored() {
        let mut i = input("/w/repo");
        i.other_worktrees = vec![
            PathBuf::from("/w/repo/.worktrees/other"),
            PathBuf::from("/w/sibling"),
            PathBuf::from("/w/repo"), // the source itself is listed by git too
        ];
        let ex = Exclusions::compute(&i);
        assert_eq!(listed(&ex), vec![".git", ".worktrees/other"]);
    }

    // U-EXCL-05: the main checkout containing a linked source must not exclude everything.
    #[test]
    fn worktree_that_contains_the_source_is_not_an_exclusion() {
        let mut i = input("/w/main/.worktrees/linked");
        i.other_worktrees = vec![PathBuf::from("/w/main")];
        assert_eq!(listed(&Exclusions::compute(&i)), vec![".git"]);
    }

    // U-EXCL-06
    #[test]
    fn submodules_excluded_only_under_reinit_policy() {
        let mut i = input("/w/repo");
        i.submodule_paths = vec![PathBuf::from("libs/a"), PathBuf::from("b")];
        assert_eq!(listed(&Exclusions::compute(&i)), vec![".git"]);
        i.exclude_submodules = true;
        assert_eq!(
            listed(&Exclusions::compute(&i)),
            vec![".git", "b", "libs/a"]
        );
    }

    // U-EXCL-07
    #[test]
    fn matching_is_component_wise_not_string_prefix() {
        let mut i = input("/w/repo");
        i.submodule_paths = vec![PathBuf::from("foo")];
        i.exclude_submodules = true;
        let ex = Exclusions::compute(&i);
        assert!(ex.is_excluded(Path::new("foo")));
        assert!(!ex.is_excluded(Path::new("foobar")));
        assert!(!ex.is_excluded(Path::new(".gitignore")));
        assert!(!ex.is_ancestor_of_excluded(Path::new("fo")));
    }

    // U-EXCL-08
    #[test]
    fn excluded_path_is_not_its_own_ancestor_and_children_are_not_ancestors() {
        let mut i = input("/w/repo");
        i.submodule_paths = vec![PathBuf::from("libs/a")];
        i.exclude_submodules = true;
        let ex = Exclusions::compute(&i);
        assert!(ex.is_ancestor_of_excluded(Path::new("libs")));
        assert!(!ex.is_ancestor_of_excluded(Path::new("libs/a")));
        assert!(!ex.is_ancestor_of_excluded(Path::new("libs/a/src")));
        assert!(
            ex.is_excluded(Path::new("libs/a/src")),
            "descendants of excluded are excluded"
        );
    }

    // U-EXCL-09
    #[test]
    fn nested_exclusions_are_collapsed() {
        let mut i = input("/w/repo");
        i.other_worktrees = vec![
            PathBuf::from("/w/repo/wt"),
            PathBuf::from("/w/repo/wt/inner"),
        ];
        assert_eq!(listed(&Exclusions::compute(&i)), vec![".git", "wt"]);
    }

    // U-EXCL-P1..P2
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn rel() -> impl Strategy<Value = PathBuf> {
            prop::collection::vec("[ab]{1,2}", 1..4).prop_map(|v| v.iter().collect())
        }

        proptest! {
            #[test]
            fn excluded_and_ancestor_are_mutually_exclusive(
                subs in prop::collection::vec(rel(), 0..6),
                probe in rel(),
            ) {
                let ex = Exclusions::compute(&ExclusionInput {
                    source_root: PathBuf::from("/r"),
                    submodule_paths: subs,
                    exclude_submodules: true,
                    ..ExclusionInput::default()
                });
                prop_assert!(!(ex.is_excluded(&probe) && ex.is_ancestor_of_excluded(&probe)));
            }

            #[test]
            fn every_input_is_excluded_and_set_has_no_nesting(
                subs in prop::collection::vec(rel(), 0..6),
            ) {
                let ex = Exclusions::compute(&ExclusionInput {
                    source_root: PathBuf::from("/r"),
                    submodule_paths: subs.clone(),
                    exclude_submodules: true,
                    ..ExclusionInput::default()
                });
                for s in &subs {
                    prop_assert!(ex.is_excluded(s));
                }
                let all: Vec<_> = ex.iter().collect();
                for a in &all {
                    for b in &all {
                        prop_assert!(a == b || !b.starts_with(a), "{:?} nested in {:?}", b, a);
                    }
                }
            }
        }
    }
}
