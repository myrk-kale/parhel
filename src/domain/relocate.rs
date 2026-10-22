//! Relocation of files that embed the source worktree's absolute path.
//!
//! Ignored files are cloned as they are, so anything that recorded the source's
//! absolute path (script shebangs, generated configs, absolute symlinks, path
//! files of virtual environments...) would keep pointing at the source. The copy
//! is treated as if the directory had been moved: occurrences of the source path
//! are replaced by the destination path. This is language-agnostic: no tool is
//! special-cased.
//!
//! Only text is rewritten. A binary that mentions the source is reported, never
//! edited, because changing the length of an embedded path corrupts it.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Files larger than this are not examined: paths live in small text files, while
/// large files are build artifacts and data that would make `add` slow to scan.
pub const MAX_RELOCATE_BYTES: u64 = 1 << 20;

/// Outcome of examining one file's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewrite {
    /// No reference to the source.
    Unchanged,
    /// Text that referenced the source; the new contents.
    Rewritten(Vec<u8>),
    /// A binary (contains NUL bytes) that references the source: left untouched.
    BinaryReference,
}

/// Replaces the source worktree's path by the destination's.
#[derive(Debug, Clone)]
pub struct Relocation {
    needles: Vec<Vec<u8>>,
    replacement: Vec<u8>,
    /// `/<relative path>` of each excluded location.
    excluded: Vec<Vec<u8>>,
}

impl Relocation {
    /// `source_root` and `destination` are canonical. On macOS, paths under
    /// `/private/var`, `/private/tmp` and `/private/etc` are also recorded by tools
    /// without the `/private` prefix; both spellings are recognized.
    #[must_use]
    pub fn new(source_root: &Path, destination: &Path) -> Self {
        let canonical = source_root.as_os_str().as_bytes().to_vec();
        let mut needles = vec![canonical.clone()];
        if let Some(rest) = canonical.strip_prefix(b"/private") {
            if [&b"/var/"[..], b"/tmp/", b"/etc/"]
                .iter()
                .any(|p| rest.starts_with(p))
            {
                needles.push(rest.to_vec());
            }
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        Self {
            needles,
            replacement: destination.as_os_str().as_bytes().to_vec(),
            excluded: Vec::new(),
        }
    }

    /// Start of the next whole-path occurrence at or after `from`, and its length.
    fn next_match(&self, content: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut i = from;
        while i < content.len() {
            let Some(offset) = content[i..].iter().position(|&b| b == b'/') else {
                break;
            };
            i += offset;
            let starts_path = i == 0
                || content[..i].ends_with(b"://")
                || !(continues_name(content[i - 1]) || content[i - 1] == b'/');
            if starts_path {
                for needle in &self.needles {
                    let end = i + needle.len();
                    if content[i..].starts_with(needle)
                        && content.get(end).is_none_or(|&b| !continues_name(b))
                    {
                        if self.leads_into_excluded(&content[end..]) {
                            break;
                        }
                        return Some((i, needle.len()));
                    }
                }
            }
            i += 1;
        }
        None
    }

    /// Leave references into these source-relative paths unchanged (they are not
    /// part of the copy).
    #[must_use]
    pub fn excluding<'a>(mut self, paths: impl IntoIterator<Item = &'a Path>) -> Self {
        for path in paths {
            let mut bytes = b"/".to_vec();
            bytes.extend_from_slice(path.as_os_str().as_bytes());
            self.excluded.push(bytes);
        }
        self
    }

    /// Whether the path continuing at `rest` (right after the source prefix) goes
    /// into an excluded location.
    fn leads_into_excluded(&self, mut rest: &[u8]) -> bool {
        // Lexically skip `/.` segments and doubled slashes (`<src>/./.git`).
        loop {
            if rest.starts_with(b"/./") || rest == b"/." {
                rest = &rest[2..];
            } else if rest.starts_with(b"//") {
                rest = &rest[1..];
            } else {
                break;
            }
        }
        // `<src>/..` leaves the source: what it names is not part of the copy.
        if rest.starts_with(b"/..") && rest.get(3).is_none_or(|&b| b == b'/') {
            return true;
        }
        self.excluded.iter().any(|ex| {
            rest.starts_with(ex) && rest.get(ex.len()).is_none_or(|&b| !continues_name(b))
        })
    }

    /// The byte strings that may start a reference to the source (for a fast
    /// pre-filter; [`Self::rewrite`] decides).
    #[must_use]
    pub fn needles(&self) -> &[Vec<u8>] {
        &self.needles
    }

    /// Rewrite every whole-path occurrence of the source in `content`.
    ///
    /// An occurrence must end at a path boundary (end of input, `/`, or a byte that
    /// cannot continue a file name), so `/w/src2` or `/w/src.bak` never match
    /// `/w/src`. It must also start a path: not preceded by a file-name byte or by
    /// `/`, except after `://` (as in `file:///w/src`).
    #[must_use]
    pub fn rewrite(&self, content: &[u8]) -> Rewrite {
        let Some(first) = self.next_match(content, 0) else {
            return Rewrite::Unchanged;
        };
        if content.contains(&0) {
            return Rewrite::BinaryReference;
        }
        let mut out = Vec::with_capacity(content.len() + self.replacement.len());
        let (mut copied, mut next) = (0, Some(first));
        while let Some((start, len)) = next {
            out.extend_from_slice(&content[copied..start]);
            out.extend_from_slice(&self.replacement);
            copied = start + len;
            next = self.next_match(content, copied);
        }
        out.extend_from_slice(&content[copied..]);
        Rewrite::Rewritten(out)
    }

    /// New target for an absolute symlink into the source, if it is one.
    #[must_use]
    pub fn rewrite_link(&self, target: &Path) -> Option<PathBuf> {
        let bytes = target.as_os_str().as_bytes();
        let needle = self.needles.iter().find(|n| {
            bytes.starts_with(n)
                && bytes.get(n.len()).is_none_or(|&b| b == b'/')
                && !self.leads_into_excluded(&bytes[n.len()..])
        })?;
        let mut out = self.replacement.clone();
        out.extend_from_slice(&bytes[needle.len()..]);
        Some(PathBuf::from(OsStr::from_bytes(&out)))
    }
}

/// Bytes that can continue a file name, so a match ending before one is only a
/// prefix of a longer name (`/w/src2`, `/w/src.bak`, non-ASCII names).
fn continues_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'~' | b'@') || b >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn rel() -> Relocation {
        Relocation::new(Path::new("/w/src"), Path::new("/w/feat"))
    }

    fn text(r: &Rewrite) -> String {
        match r {
            Rewrite::Rewritten(b) => String::from_utf8(b.clone()).unwrap(),
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    // U-REL-01: the forms a recorded path takes in real files.
    #[rstest]
    #[case::shebang("#!/w/src/.venv/bin/python3\n", "#!/w/feat/.venv/bin/python3\n")]
    #[case::assignment("VIRTUAL_ENV=\"/w/src/.venv\"\n", "VIRTUAL_ENV=\"/w/feat/.venv\"\n")]
    #[case::json(
        "{\"root\":\"/w/src\",\"bin\":\"/w/src/bin\"}",
        "{\"root\":\"/w/feat\",\"bin\":\"/w/feat/bin\"}"
    )]
    #[case::file_url("url = file:///w/src/pkg\n", "url = file:///w/feat/pkg\n")]
    #[case::path_list("/w/src/a:/w/src/b\n", "/w/feat/a:/w/feat/b\n")]
    #[case::whole_line("/w/src\n", "/w/feat\n")]
    #[case::at_end("cd /w/src", "cd /w/feat")]
    fn rewrites_recorded_paths(#[case] input: &str, #[case] want: &str) {
        assert_eq!(text(&rel().rewrite(input.as_bytes())), want);
    }

    // U-REL-02: similar-looking paths that are not the source.
    #[rstest]
    #[case::sibling_suffix("/w/src2/x")]
    #[case::backup("/w/src.bak/x")]
    #[case::dash("/w/src-old/x")]
    #[case::nested_elsewhere("/other/w/src/x")]
    #[case::relative("w/src/x")]
    #[case::unicode_continuation("/w/srcé/x")]
    #[case::nothing("no paths here")]
    fn leaves_other_paths_alone(#[case] input: &str) {
        assert_eq!(rel().rewrite(input.as_bytes()), Rewrite::Unchanged);
    }

    // U-REL-03: macOS spells the same directory two ways.
    #[test]
    fn recognizes_private_var_alias() {
        let r = Relocation::new(
            Path::new("/private/var/t/src"),
            Path::new("/private/var/t/feat"),
        );
        assert_eq!(text(&r.rewrite(b"/var/t/src/a")), "/private/var/t/feat/a");
        assert_eq!(
            text(&r.rewrite(b"/private/var/t/src/a")),
            "/private/var/t/feat/a"
        );
    }

    // U-REL-04: no double replacement when the destination contains the source.
    #[test]
    fn destination_containing_source_is_not_rewritten_twice() {
        let r = Relocation::new(Path::new("/w/src"), Path::new("/w/src/.worktrees/feat"));
        assert_eq!(text(&r.rewrite(b"/w/src/x")), "/w/src/.worktrees/feat/x");
    }

    // U-REL-05: binaries are reported, never edited.
    #[test]
    fn binary_with_reference_is_reported() {
        let mut content = b"\x7fELF\0\0".to_vec();
        content.extend_from_slice(b"/w/src/lib\0");
        assert_eq!(rel().rewrite(&content), Rewrite::BinaryReference);
        assert_eq!(
            rel().rewrite(b"\0\x01binary without path"),
            Rewrite::Unchanged
        );
    }

    // U-REL-06
    #[rstest]
    #[case::inside("/w/src/node_modules/x", Some("/w/feat/node_modules/x"))]
    #[case::root("/w/src", Some("/w/feat"))]
    #[case::sibling("/w/src2/x", None)]
    #[case::outside("/usr/bin/python3", None)]
    #[case::relative("../src/x", None)]
    fn relocates_absolute_symlinks_into_the_source(
        #[case] target: &str,
        #[case] want: Option<&str>,
    ) {
        assert_eq!(
            rel().rewrite_link(Path::new(target)),
            want.map(PathBuf::from)
        );
    }

    // U-REL-07: paths into what the copy does not contain (the source's `.git`,
    // other worktrees, submodules, .parhelignore) keep pointing at the source:
    // rewritten, they would name something that does not exist in the copy.
    #[test]
    fn leaves_references_leads_into_excluded_paths() {
        let r = rel().excluding([Path::new(".git"), Path::new("libs/sub")]);
        assert_eq!(r.rewrite(b"/w/src/.git/objects\n"), Rewrite::Unchanged);
        assert_eq!(r.rewrite(b"/w/src/libs/sub/x"), Rewrite::Unchanged);
        assert_eq!(text(&r.rewrite(b"/w/src/.gitignore")), "/w/feat/.gitignore");
        assert_eq!(
            text(&r.rewrite(b"/w/src/libs/subway")),
            "/w/feat/libs/subway"
        );
        assert_eq!(
            text(&r.rewrite(b"/w/src/.git/x /w/src/a")),
            "/w/src/.git/x /w/feat/a",
            "only the excluded occurrence is kept"
        );
        assert_eq!(
            r.rewrite(b"/w/src/./.git/objects"),
            Rewrite::Unchanged,
            "dot segment"
        );
        assert_eq!(
            r.rewrite(b"/w/src//.git/objects"),
            Rewrite::Unchanged,
            "double slash"
        );
        assert_eq!(
            r.rewrite(b"/w/src/../shared/x"),
            Rewrite::Unchanged,
            "leaves the source"
        );
        assert_eq!(r.rewrite_link(Path::new("/w/src/.git/objects")), None);
        assert_eq!(
            r.rewrite_link(Path::new("/w/src/a")),
            Some(PathBuf::from("/w/feat/a"))
        );
    }

    // U-REL-P1: after a rewrite, no whole-path occurrence of the source remains.
    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn rewritten_text_never_references_the_source(
                chunks in prop::collection::vec(
                    prop_oneof![
                        Just("/w/src".to_owned()),
                        Just("/w/src/".to_owned()),
                        Just("/w/src2".to_owned()),
                        "[ a-z\\n\\\"=:/._-]{0,8}",
                    ],
                    0..12,
                )
            ) {
                let input = chunks.concat();
                if let Rewrite::Rewritten(out) = rel().rewrite(input.as_bytes()) {
                    prop_assert_eq!(rel().rewrite(&out), Rewrite::Unchanged);
                }
            }
        }
    }
}
