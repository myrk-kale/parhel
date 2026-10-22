//! Man pages are generated from the clap definitions and committed under `man/`
//! (Homebrew and the installer ship them, so `git parhel --help` works: git turns
//! it into `man git-parhel`). This test fails when they drift from the CLI.
//!
//! Regenerate with: `PARHEL_UPDATE_MAN=1 cargo test --test man`

use std::fs;
use std::path::Path;

use clap::CommandFactory;
use parhel::cli::Cli;

fn pages() -> Vec<(String, Vec<u8>)> {
    let mut cli = Cli::command();
    cli.build();
    let mut pages = Vec::new();
    let mut render = |name: &str, cmd: clap::Command| {
        let mut buf = Vec::new();
        clap_mangen::Man::new(cmd.name(&*Box::leak(name.to_owned().into_boxed_str())))
            .manual("Git Manual")
            .source(format!("parhel {}", env!("CARGO_PKG_VERSION")))
            .render(&mut buf)
            .unwrap();
        pages.push((format!("{name}.1"), buf));
    };
    for sub in cli.get_subcommands().filter(|s| s.get_name() != "help") {
        render(&format!("git-parhel-{}", sub.get_name()), sub.clone());
    }
    render("git-parhel", cli);
    pages
}

#[test]
fn man_pages_are_up_to_date() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("man");
    let update = std::env::var_os("PARHEL_UPDATE_MAN").is_some();
    for (name, content) in pages() {
        let path = dir.join(&name);
        if update {
            fs::create_dir_all(&dir).unwrap();
            fs::write(&path, &content).unwrap();
        } else {
            let committed = fs::read(&path).unwrap_or_default();
            assert!(
                committed == content,
                "man/{name} is stale; run `PARHEL_UPDATE_MAN=1 cargo test --test man`"
            );
        }
    }
}

#[test]
fn every_page_documents_its_options() {
    let pages = pages();
    let add = &pages
        .iter()
        .find(|(n, _)| n == "git-parhel-add.1")
        .unwrap()
        .1;
    let add = String::from_utf8_lossy(add);
    for flag in [
        "from",
        "branch",
        "new\\-branch",
        "detach",
        "allow\\-dirty",
        "fallback",
        "submodules",
    ] {
        assert!(add.contains(flag), "git-parhel-add.1 lacks --{flag}");
    }
    let top = String::from_utf8_lossy(&pages.iter().find(|(n, _)| n == "git-parhel.1").unwrap().1)
        .into_owned();
    assert!(
        top.contains("git\\-parhel\\-add(1)") || top.contains("add"),
        "top page lists subcommands"
    );
}
