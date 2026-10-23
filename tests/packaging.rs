//! I-P01: the Homebrew formula renderer refuses a version it would splice into `sed`
//! (a `/` or `&` in it would rewrite the formula, not fill it).

use std::path::Path;
use std::process::Command;

fn render(version: &str) -> std::process::Output {
    let sha = |n: u8| format!("{:0>64}", n);
    Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/render-formula.sh"))
        .args([version, &sha(1), &sha(2), &sha(3)])
        .output()
        .unwrap()
}

// I-P01
#[test]
fn ip01_formula_renderer_validates_the_version() {
    for good in ["0.3.0", "1.2.3-rc.1", "10.20.30"] {
        let out = render(good);
        assert!(
            out.status.success(),
            "{good}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(good),
            "{good} is in the formula"
        );
    }
    for bad in [
        "", "v1.0.0", "1.0/../x", "1&2", "1;rm", "1 2", "1.0\n2.0", ".", "-1",
    ] {
        let out = render(bad);
        assert_eq!(out.status.code(), Some(1), "{bad:?} must be refused");
        assert!(out.stdout.is_empty(), "{bad:?} printed a formula");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("not a version"),
            "{bad:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
