//! I-170..I-176: nothing parhel does may touch another worktree, the repository's
//! shared configuration or refs, beyond the branch and the worktree entry the run
//! itself creates. Every scenario snapshots everything else, byte for byte.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::*;

/// Every byte that belongs to someone other than the run under test.
#[derive(Debug, PartialEq, Eq)]
struct Foreign {
    worktrees: BTreeMap<PathBuf, (BTreeMap<PathBuf, String>, Vec<u8>)>,
    admin: BTreeMap<PathBuf, Vec<u8>>,
    config: Vec<u8>,
    refs: BTreeMap<String, String>,
}

fn read_dir_files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(read_dir_files(&p));
        } else if let Ok(b) = fs::read(&p) {
            out.insert(p, b);
        }
    }
    out
}

/// The repository with four other worktrees, each in a fragile state.
struct Fixture {
    sb: Sandbox,
    src: TestRepo,
    others: Vec<PathBuf>,
}

fn fixture() -> Fixture {
    let sb = Sandbox::new();
    let src = project(&sb);
    let dirty = sb.path("other-dirty");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "dirty",
        dirty.to_str().unwrap(),
    ]);
    fs::write(dirty.join("README.md"), "uncommitted").unwrap();
    fs::write(dirty.join("untracked.txt"), "u").unwrap();
    let broken = sb.path("other-broken-link");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "broken",
        broken.to_str().unwrap(),
    ]);
    fs::write(broken.join(".git"), "gitdir: /nowhere/at/all\n").unwrap();
    let unmounted = sb.path("other-unmounted");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "unmounted",
        unmounted.to_str().unwrap(),
    ]);
    fs::rename(&unmounted, sb.path("other-unmounted.offline")).unwrap();
    let nested = src.path(".worktrees/nested");
    src.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "nested",
        nested.to_str().unwrap(),
    ]);
    Fixture {
        sb,
        src,
        others: vec![dirty, broken, sb_path_unmounted(&unmounted), nested],
    }
}

fn sb_path_unmounted(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".offline");
    s.into()
}

impl Fixture {
    fn foreign(&self, created_branch: Option<&str>, created_admin: Option<&str>) -> Foreign {
        let common = self.src.common_dir();
        let mut worktrees = BTreeMap::new();
        for wt in self.others.iter().chain(std::iter::once(&self.src.root)) {
            let gitfile = fs::read(wt.join(".git")).unwrap_or_default();
            worktrees.insert(wt.clone(), (tree_digest(wt), gitfile));
        }
        let mut admin = read_dir_files(&common.join("worktrees"));
        if let Some(id) = created_admin {
            let own = common.join("worktrees").join(id);
            admin.retain(|p, _| !p.starts_with(&own));
        }
        for name in ["HEAD", "index", "packed-refs"] {
            if let Ok(b) = fs::read(common.join(name)) {
                admin.insert(common.join(name), b);
            }
        }
        let mut refs = self.src.branches();
        if let Some(b) = created_branch {
            refs.remove(b);
        }
        Foreign {
            worktrees,
            admin,
            config: fs::read(common.join("config")).unwrap(),
            refs,
        }
    }
}

fn assert_untouched(before: &Foreign, after: &Foreign) {
    for (wt, state) in &before.worktrees {
        assert_eq!(
            after.worktrees.get(wt),
            Some(state),
            "worktree {} changed",
            wt.display()
        );
    }
    for (p, bytes) in &before.admin {
        assert_eq!(after.admin.get(p), Some(bytes), "{} changed", p.display());
    }
    let added: Vec<_> = after
        .admin
        .keys()
        .filter(|k| !before.admin.contains_key(*k))
        .collect();
    let removed: Vec<_> = before
        .admin
        .keys()
        .filter(|k| !after.admin.contains_key(*k))
        .collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "added {added:?}, removed {removed:?}"
    );
    assert_eq!(after.config, before.config, ".git/config changed");
    assert_eq!(after.refs, before.refs, "refs changed");
}

// I-170: successful add.
#[test]
fn i170_successful_add_touches_nothing_else() {
    let f = fixture();
    let before = f.foreign(None, None);
    let dest = f.sb.path("feat");
    assert_success(&parhel(
        &f.sb.env,
        &f.src.root,
        &add_args(&dest, &["--new-branch", "feat"]),
    ));
    assert_untouched(&before, &f.foreign(Some("feat"), Some("feat")));
}

// I-171: refused add.
#[test]
fn i171_refused_add_touches_nothing() {
    let f = fixture();
    f.src.write("README.md", "dirty source");
    let before = f.foreign(None, None);
    let out = parhel(
        &f.sb.env,
        &f.src.root,
        &add_args(&f.sb.path("feat"), &["--new-branch", "feat"]),
    );
    assert_error(&out, "dirty-source");
    assert_untouched(&before, &f.foreign(None, None));
}

// I-172: failure at every step, then rollback.
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i172_failed_add_touches_nothing() {
    for fp in parhel::failpoint::ADD_FAILPOINTS {
        let f = fixture();
        let before = f.foreign(None, None);
        let out = parhel_with_failpoints(
            &f.sb.env,
            &f.src.root,
            &add_args(&f.sb.path("feat"), &["--new-branch", "feat"]),
            &format!("{fp}=error"),
        );
        assert_error(&out, "failpoint");
        assert_untouched(&before, &f.foreign(None, None));
    }
}

// I-173: crash, then doctor --fix (rolled back and published cases).
#[cfg_attr(not(feature = "failpoints"), ignore = "requires --features failpoints")]
#[test]
fn i173_doctor_fix_after_crash_touches_nothing_else() {
    for (fp, published) in [("before_rename", false), ("after_rename", true)] {
        let f = fixture();
        let before = f.foreign(None, None);
        let dest = f.sb.path("feat");
        let out = parhel_with_failpoints(
            &f.sb.env,
            &f.src.root,
            &add_args(&dest, &["--new-branch", "feat"]),
            &format!("{fp}=kill"),
        );
        assert_eq!(out.status.code(), None);
        parhel(&f.sb.env, &f.src.root, &["doctor", "--fix"]);
        let after = if published {
            f.foreign(Some("feat"), Some("feat"))
        } else {
            f.foreign(None, None)
        };
        assert_untouched(&before, &after);
    }
}

// I-174: relocation and gitfile detaching only write inside the new worktree.
#[test]
fn i174_relocation_touches_nothing_else() {
    let f = fixture();
    f.src
        .write("target/env.sh", format!("ROOT={}\n", f.src.root.display()));
    std::os::unix::fs::symlink(f.src.path("target"), f.src.path("node_modules/self")).unwrap();
    let before = f.foreign(None, None);
    assert_success(&parhel(
        &f.sb.env,
        &f.src.root,
        &add_args(&f.sb.path("feat"), &["--new-branch", "feat"]),
    ));
    assert_untouched(&before, &f.foreign(Some("feat"), Some("feat")));
}

// I-175: submodule reinit does not rewrite the shared configuration.
#[test]
fn i175_submodule_reinit_keeps_shared_config() {
    let f = fixture();
    let lib = f.sb.repo("lib");
    f.src.add_submodule(&lib, "libs/lib");
    let before = f.foreign(None, None);
    let out = parhel(
        &f.sb.env,
        &f.src.root,
        &add_args(
            &f.sb.path("feat"),
            &["--new-branch", "feat", "--submodules=reinit"],
        ),
    );
    assert_success(&out);
    assert_untouched(&before, &f.foreign(Some("feat"), Some("feat")));
}

// I-176: doctor --fix never deletes a folder that merely looks like staging.
#[test]
fn i176_doctor_keeps_unknown_staging_lookalikes() {
    let f = fixture();
    let lookalike = f.sb.path(".parhel-staging-mine");
    fs::create_dir(&lookalike).unwrap();
    fs::write(lookalike.join("important.txt"), "not parhel's").unwrap();
    parhel(&f.sb.env, &f.src.root, &["doctor", "--fix"]);
    assert_eq!(
        fs::read_to_string(lookalike.join("important.txt")).unwrap(),
        "not parhel's"
    );
}
