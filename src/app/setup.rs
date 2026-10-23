//! `git parhel setup`: create (or remove) the per-user store. Linux, root.
//!
//! Every effect on the machine goes through the [`Host`] port, so the steps, their
//! order and what they do when something is already there are unit-tested with a
//! recording fake. The steps are idempotent: each one looks at what exists and does
//! only what is missing, so a run that stopped half way is finished by running it again.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::domain::request::DEFAULT_STORE_SIZE;
use crate::domain::store::{BASELINES_DIR, MARKER_FILE, Marker, WORKTREES_DIR};
use crate::domain::{Error, ErrorCode, Result, SetupRequest};
use crate::ports::{Host, Owner, UserInfo};

/// Where the images live: root's, and immutable once there is one.
const STATE_DIR: &str = "/var/lib/parhel";
/// Parent of the mount points.
const MOUNT_PARENT: &str = "/parhel";
/// Where systemd looks for units an administrator wrote.
const UNIT_DIR: &str = "/etc/systemd/system";

/// What the store looks like after `setup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSummary {
    pub user: String,
    /// Mount point (`/parhel/<user>`).
    pub store: PathBuf,
    pub image: PathBuf,
    /// Size the image claims.
    pub apparent: u64,
    /// Space the image occupies on the host.
    pub allocated: u64,
    /// systemd mount unit name.
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupOutcome {
    Created(StoreSummary),
    /// A healthy store already existed: nothing was changed.
    AlreadySetUp(StoreSummary),
    /// A store that existed was brought back to working order (its unit was stopped, or
    /// its image had lost the protection); its data was not touched.
    Restored(StoreSummary),
    Removed(StoreSummary),
    /// `--remove` with no store: nothing to do.
    NothingToRemove {
        user: String,
    },
}

/// Where everything of one user's store lives.
struct Places {
    user: UserInfo,
    /// Mount point, `/parhel/<user>`.
    root: PathBuf,
    image: PathBuf,
    /// Name systemd gives the mount unit of `root`.
    unit: String,
    unit_file: PathBuf,
    /// What the user owns inside the store.
    owner: Owner,
}

impl Places {
    fn new(host: &dyn Host, user: UserInfo) -> Result<Self> {
        let root = Path::new(MOUNT_PARENT).join(&user.name);
        let image = Path::new(STATE_DIR).join(format!("{}.img", user.name));
        require_tool(host, "systemd-escape", "systemd")?;
        let root_text = root.to_string_lossy().into_owned();
        // A mount unit must be named after its mount point the way systemd spells it
        // (a `-` in the user name becomes `\x2d`): ask systemd instead of guessing.
        let unit = host
            .run("systemd-escape", &["--path", "--suffix=mount", &root_text])?
            .trim()
            .to_owned();
        if Path::new(&unit)
            .extension()
            .is_none_or(|ext| ext != "mount")
            || unit.contains('/')
        {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "`systemd-escape` named the unit of {root_text} `{unit}`, which is not a mount unit"
                ),
            ));
        }
        let unit_file = Path::new(UNIT_DIR).join(&unit);
        Ok(Self {
            owner: Some((user.uid, user.gid)),
            user,
            root,
            image,
            unit,
            unit_file,
        })
    }

    fn marker(&self) -> Marker {
        Marker {
            user: self.user.name.clone(),
            image: self.image.clone(),
            unit: self.unit.clone(),
        }
    }

    fn marker_file(&self) -> PathBuf {
        self.root.join(MARKER_FILE)
    }

    /// The unit name as a shell word (`\x2d` needs quotes).
    fn unit_word(&self) -> String {
        if self.unit.contains('\\') {
            format!("'{}'", self.unit)
        } else {
            self.unit.clone()
        }
    }

    fn summary(&self, host: &dyn Host) -> Result<StoreSummary> {
        let usage = host.image_usage(&self.image)?;
        Ok(StoreSummary {
            user: self.user.name.clone(),
            store: self.root.clone(),
            image: self.image.clone(),
            apparent: usage.apparent,
            allocated: usage.allocated,
            unit: self.unit.clone(),
        })
    }

    /// The mount unit written for this store.
    fn unit_text(&self) -> String {
        format!(
            "[Unit]\n\
             Description=parhel store for {user}\n\
             RequiresMountsFor={STATE_DIR}\n\
             \n\
             [Mount]\n\
             What={image}\n\
             Where={root}\n\
             Type=btrfs\n\
             Options=loop,discard,noatime,nosuid,nodev\n\
             \n\
             [Install]\n\
             WantedBy=local-fs.target\n",
            user = self.user.name,
            image = self.image.display(),
            root = self.root.display(),
        )
    }
}

/// Run `git parhel setup`.
pub fn setup(host: &dyn Host, request: &SetupRequest) -> Result<SetupOutcome> {
    if !host.is_linux() {
        return Err(Error::new(
            ErrorCode::UnsupportedPlatform,
            "`git parhel setup` creates a btrfs store with systemd and only exists on Linux; \
             macOS (APFS) does not need one",
        ));
    }
    if !host.is_root() {
        return Err(not_root(request));
    }
    if !request.remove {
        // Everything the run will start, before it creates anything: a tool that turns out
        // to be missing in the middle leaves an image and a half-made store behind.
        require_tool(host, "mkfs.btrfs", "btrfs-progs")?;
        require_tool(host, "blkid", "util-linux")?;
        require_tool(host, "mount", "util-linux")?;
    }
    if !host.systemd_running() {
        return Err(Error::new(
            ErrorCode::UnsupportedPlatform,
            "the store is a systemd mount unit, and systemd is not running as the init system \
             of this machine (containers and WSL1 usually lack it)",
        ));
    }
    if !request.remove {
        require_btrfs(host)?;
    }
    let places = Places::new(host, account(host, request)?)?;
    if request.remove {
        remove(host, &places)
    } else {
        create(host, &places, request)
    }
}

/// The command to run as root, with the options that were given.
fn sudo_command(request: &SetupRequest) -> String {
    let mut words = vec!["sudo git parhel setup".to_owned()];
    if request.remove {
        words.push("--remove".to_owned());
    } else {
        if request.size != DEFAULT_STORE_SIZE {
            words.push(format!("--size {}", request.size));
        }
        if request.preallocate {
            words.push("--preallocate".to_owned());
        }
    }
    if let Some(user) = &request.user {
        words.push(format!("--user {user}"));
    }
    words.join(" ")
}

fn not_root(request: &SetupRequest) -> Error {
    Error::new(
        ErrorCode::NotRoot,
        format!(
            "`git parhel setup` creates an image, a mount and a systemd unit and must run as \
             root: `{}` (if sudo cannot find git-parhel, run \
             `sudo $(command -v git-parhel) setup ...` instead)",
            sudo_command(request)
        ),
    )
}

fn require_tool(host: &dyn Host, tool: &str, package: &str) -> Result<()> {
    if host.tool_exists(tool) {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::MissingTool,
        format!("`{tool}` is not installed: install the package {package} and run this again"),
    ))
}

/// btrfs must be in the kernel: load the module when it is only available.
fn require_btrfs(host: &dyn Host) -> Result<()> {
    let listed = |host: &dyn Host| -> Result<bool> {
        Ok(host
            .read_text(Path::new("/proc/filesystems"))?
            .unwrap_or_default()
            .lines()
            .any(|line| line.split_whitespace().last() == Some("btrfs")))
    };
    if listed(host)? {
        return Ok(());
    }
    let unsupported = |why: String| {
        Error::new(
            ErrorCode::UnsupportedPlatform,
            format!("this kernel has no btrfs support, which the store needs ({why})"),
        )
    };
    host.run("modprobe", &["btrfs"])
        .map_err(|e| unsupported(e.message))?;
    if listed(host)? {
        Ok(())
    } else {
        Err(unsupported(
            "`modprobe btrfs` did not make it appear".into(),
        ))
    }
}

/// Whose store this is: `--user`, else the user who ran `sudo`. Root is never inferred.
fn account(host: &dyn Host, request: &SetupRequest) -> Result<UserInfo> {
    let name = match &request.user {
        Some(name) => name.clone(),
        None => match host.env_var("SUDO_USER") {
            Some(name) if !name.is_empty() && name != "root" => name,
            _ => {
                return Err(Error::new(
                    ErrorCode::Usage,
                    "cannot tell whose store this is: this looks like a root shell, not a \
                     `sudo` from the account that will use parhel. Run it with sudo from that \
                     account, or name it with `--user <name>`",
                ));
            }
        },
    };
    crate::domain::request::check_user_name(&name)?;
    host.user(&name)?.ok_or_else(|| {
        Error::new(
            ErrorCode::Usage,
            format!("the user `{name}` does not exist on this machine"),
        )
    })
}

// ------------------------------------------------------------------------ creation

/// What a run changed.
#[derive(Default)]
struct Progress {
    /// The image was created or formatted: a new store.
    built: bool,
    /// Anything else was done to an existing one.
    changed: bool,
}

fn create(host: &dyn Host, places: &Places, request: &SetupRequest) -> Result<SetupOutcome> {
    let image_present = host.exists(&places.image);
    let unit_text = host.read_text(&places.unit_file)?;
    let mounted = host.is_mount_point(&places.root);
    if let Some(text) = &unit_text {
        check_unit(places, text)?;
    }
    if mounted && (!image_present || unit_text.is_none()) {
        return Err(Error::new(
            ErrorCode::StoreUnavailable,
            format!(
                "{} is a mount point, but {}: it was not mounted by `git parhel setup`. \
                 Unmount it, or run `{}` to clean up, and run this again",
                places.root.display(),
                if image_present {
                    format!("{} does not exist", places.unit_file.display())
                } else {
                    format!("its image {} does not exist", places.image.display())
                },
                sudo_command(&SetupRequest {
                    remove: true,
                    size: DEFAULT_STORE_SIZE,
                    preallocate: false,
                    user: Some(places.user.name.clone()),
                }),
            ),
        ));
    }

    // A directory that already protects other images cannot take a new one.
    let lifted = !image_present && host.is_immutable(Path::new(STATE_DIR));
    if lifted {
        host.run("chattr", &["-i", STATE_DIR])?;
    }
    let mut progress = Progress::default();
    let built = build(
        host,
        places,
        request,
        &Existing {
            image: image_present,
            unit: unit_text.is_some(),
            mounted,
        },
        &mut progress,
    );
    if built.is_err() && lifted && !host.is_immutable(Path::new(STATE_DIR)) {
        // Whatever stopped the run, the images already there stay protected.
        let _ = host.run("chattr", &["+i", STATE_DIR]);
    }
    built?;

    let summary = places.summary(host)?;
    Ok(if progress.built {
        SetupOutcome::Created(summary)
    } else if progress.changed {
        SetupOutcome::Restored(summary)
    } else {
        SetupOutcome::AlreadySetUp(summary)
    })
}

/// What was there when the run started.
struct Existing {
    image: bool,
    unit: bool,
    mounted: bool,
}

fn build(
    host: &dyn Host,
    places: &Places,
    request: &SetupRequest,
    existing: &Existing,
    progress: &mut Progress,
) -> Result<()> {
    // The image: created when missing, formatted when it holds no filesystem at all
    // (an earlier run stopped before `mkfs`), never touched when it holds one.
    if !existing.image {
        host.create_dir(Path::new(STATE_DIR), 0o700, None)?;
        host.create_image(&places.image, request.size, request.preallocate)?;
        progress.built = true;
    }
    match host.filesystem_of(&places.image)? {
        None => {
            let label = format!("parhel-{}", places.user.name);
            let image = places.image.to_string_lossy();
            host.run("mkfs.btrfs", &["-q", "-L", &label, &image])?;
            progress.built = true;
        }
        Some(kind) if kind == "btrfs" => {}
        Some(kind) => {
            return Err(Error::new(
                ErrorCode::StoreUnavailable,
                format!(
                    "{} already holds {kind}, not btrfs: `git parhel setup` never formats an \
                     image that has data. Move it away, or remove it yourself, and run this again",
                    places.image.display()
                ),
            ));
        }
    }

    // The mount point (underneath, until the image is mounted over it) and the unit.
    if !existing.mounted && !host.exists(&places.root) {
        host.create_dir(Path::new(MOUNT_PARENT), 0o755, None)?;
        host.create_dir(&places.root, 0o755, None)?;
    }
    if !existing.unit {
        host.write_file(
            &places.unit_file,
            places.unit_text().as_bytes(),
            0o644,
            None,
        )?;
        host.run("systemctl", &["daemon-reload"])?;
        progress.changed = true;
    }
    if !existing.mounted {
        let unit = places.unit_word();
        host.run("systemctl", &["enable", "--now", &places.unit])
            .map_err(|e| {
                Error::new(
                    e.code,
                    format!(
                        "{}\nthe store did not mount: `systemctl status {unit}` and \
                         `journalctl -xeu {unit}` say why",
                        e.message
                    ),
                )
            })?;
        if !host.is_mount_point(&places.root) {
            return Err(Error::new(
                ErrorCode::StoreUnavailable,
                format!(
                    "{} is not mounted after starting {unit}: `systemctl status {unit}` says why",
                    places.root.display()
                ),
            ));
        }
        progress.changed = true;
    }

    // Inside the mounted filesystem: the user's directories, then the marker (last, so
    // that a marker means a finished layout).
    if !marker_is_in_place(host, places)? {
        host.create_dir(&places.root, 0o700, places.owner)?;
        for dir in [BASELINES_DIR, WORKTREES_DIR] {
            host.create_dir(&places.root.join(dir), 0o700, places.owner)?;
        }
        host.write_file(
            &places.marker_file(),
            places.marker().render().as_bytes(),
            0o644,
            places.owner,
        )?;
        progress.changed = true;
    }

    if !host.is_immutable(Path::new(STATE_DIR)) {
        protect(host, places)?;
        progress.changed = true;
    }
    Ok(())
}

/// A unit that exists must be this user's: never overwrite somebody's.
fn check_unit(places: &Places, text: &str) -> Result<()> {
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    };
    let (what, whence) = (value("What="), value("Where="));
    if what == places.image.to_string_lossy() && whence == places.root.to_string_lossy() {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::StoreUnavailable,
        format!(
            "{} exists but mounts `{what}` at `{whence}`, not {} at {}: move it away and run \
             this again",
            places.unit_file.display(),
            places.image.display(),
            places.root.display()
        ),
    ))
}

/// Whether the store's own marker is there and is this user's. A marker that belongs to
/// somebody else is an error: the filesystem is not ours to lay out.
fn marker_is_in_place(host: &dyn Host, places: &Places) -> Result<bool> {
    let Some(text) = host.read_text(&places.marker_file())? else {
        return Ok(false);
    };
    let found = Marker::parse(&text)?;
    if found == places.marker() {
        Ok(true)
    } else {
        Err(Error::new(
            ErrorCode::StoreUnavailable,
            format!(
                "{} says this store is for `{}` (image {}, unit {}), not for `{}`: \
                 `git parhel setup` does not take over another store",
                places.marker_file().display(),
                found.user,
                found.image.display(),
                found.unit,
                places.user.name
            ),
        ))
    }
}

/// The command that creates (or completes) this user's store with the defaults.
fn rerun_command(places: &Places) -> String {
    sudo_command(&SetupRequest {
        size: DEFAULT_STORE_SIZE,
        preallocate: false,
        user: Some(places.user.name.clone()),
        remove: false,
    })
}

/// `chattr +i` on the directory that holds the images, and the check that it works.
/// The store is usable either way; when it cannot be protected the error says so.
fn protect(host: &dyn Host, places: &Places) -> Result<()> {
    let image = places.image.display();
    let unprotected = |why: String| {
        Error::new(
            ErrorCode::Io,
            format!(
                "the store is ready, but its image {image} is NOT protected from deletion: \
                 {why}. Anything that runs `rm` as root, a cleanup script included, would \
                 delete every worktree in the store with it. Fix that and run `{}` again: it \
                 only adds the protection",
                rerun_command(places)
            ),
        )
    };
    if !host.tool_exists("chattr") {
        return Err(Error::new(
            ErrorCode::MissingTool,
            format!(
                "the store is ready, but its image {image} is NOT protected from deletion: \
                 `chattr` is not installed (package e2fsprogs). Anything that runs `rm` as \
                 root would delete every worktree in the store with it. Install e2fsprogs and \
                 run `{}` again: it only adds the protection",
                rerun_command(places)
            ),
        ));
    }
    host.run("chattr", &["+i", STATE_DIR])
        .map_err(|e| unprotected(format!("`chattr +i {STATE_DIR}` failed: {}", e.message)))?;
    host.verify_immutable(Path::new(STATE_DIR), &places.image)
        .map_err(|e| unprotected(e.message))
}

// ------------------------------------------------------------------------ removal

fn remove(host: &dyn Host, places: &Places) -> Result<SetupOutcome> {
    let unit_text = host.read_text(&places.unit_file)?;
    let image_present = host.exists(&places.image);
    let mut mounted = host.is_mount_point(&places.root);
    let root_present = host.exists(&places.root);
    if !image_present && unit_text.is_none() && !mounted && !root_present {
        return Ok(SetupOutcome::NothingToRemove {
            user: places.user.name.clone(),
        });
    }
    if let Some(text) = &unit_text {
        check_unit(places, text)?;
    }

    // The worktrees are the user's work. They can only be seen with the store mounted:
    // a stopped one is mounted to look, and put back if it is not to be removed.
    let mut started_to_look = false;
    if !mounted && unit_text.is_some() && image_present {
        host.run("systemctl", &["start", &places.unit])
            .map_err(|e| {
                Error::new(
                    ErrorCode::StoreUnavailable,
                    format!(
                        "cannot look for worktrees in the store because it does not mount: {}",
                        e.message
                    ),
                )
            })?;
        mounted = host.is_mount_point(&places.root);
        started_to_look = true;
    }
    let worktrees = if mounted {
        worktrees_in(host, &places.root)?
    } else {
        Vec::new()
    };
    if !worktrees.is_empty() {
        if started_to_look {
            let _ = host.run("systemctl", &["stop", &places.unit]);
        }
        let list = worktrees.iter().fold(String::new(), |mut list, w| {
            let _ = write!(list, "\n  {}", w.display());
            list
        });
        return Err(Error::new(
            ErrorCode::StoreInUse,
            format!(
                "the store for {} still holds {} worktree(s):{list}\nremove each one with \
                 `git worktree remove <path>` from its repository (then `git parhel doctor \
                 --fix` clears the symlinks left behind) and run this again",
                places.user.name,
                worktrees.len()
            ),
        ));
    }

    let usage = if image_present {
        host.image_usage(&places.image).ok()
    } else {
        None
    };

    // Stop using the image, then forget the unit.
    if unit_text.is_some() {
        host.run("systemctl", &["disable", "--now", &places.unit])?;
        host.remove_file(&places.unit_file)?;
        host.run("systemctl", &["daemon-reload"])?;
    } else if mounted {
        host.run("umount", &[&places.root.to_string_lossy()])?;
    }

    // The directory is immutable: lift it to delete, and put it back for the others.
    let state_dir = Path::new(STATE_DIR);
    let lifted = host.is_immutable(state_dir);
    if lifted {
        host.run("chattr", &["-i", STATE_DIR])?;
    }
    let deleted = (|| -> Result<()> {
        if image_present {
            host.remove_file(&places.image)?;
        }
        if root_present {
            host.remove_dir(&places.root)?;
        }
        Ok(())
    })();
    if lifted && other_images(host, state_dir)? {
        host.run("chattr", &["+i", STATE_DIR])?;
    }
    deleted?;

    Ok(SetupOutcome::Removed(StoreSummary {
        user: places.user.name.clone(),
        store: places.root.clone(),
        image: places.image.clone(),
        apparent: usage.map_or(0, |u| u.apparent),
        allocated: usage.map_or(0, |u| u.allocated),
        unit: places.unit.clone(),
    }))
}

/// Whether any image is left in the directory that protects them.
fn other_images(host: &dyn Host, dir: &Path) -> Result<bool> {
    Ok(host
        .list_dir(dir)?
        .iter()
        .any(|name| name.to_string_lossy().ends_with(".img")))
}

/// The worktrees in the store: `worktrees/<repository>/<name>`. Staging directories
/// belong to an `add` and `doctor`, and baselines are a cache; neither counts.
fn worktrees_in(host: &dyn Host, root: &Path) -> Result<Vec<PathBuf>> {
    let base = root.join(WORKTREES_DIR);
    let mut found = Vec::new();
    for repository in host.list_dir(&base)? {
        if repository
            .to_string_lossy()
            .starts_with(crate::domain::paths::STAGING_PREFIX)
        {
            continue;
        }
        let dir = base.join(&repository);
        for name in host.list_dir(&dir)? {
            found.push(dir.join(name));
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fakes::{FakeHost, HostCall, UNIT_DIR, p};
    use crate::domain::request::DEFAULT_STORE_SIZE;
    use crate::ports::ImageUsage;
    use rstest::rstest;
    use std::path::Path;

    const IMG: &str = "/var/lib/parhel/alice.img";
    const ROOT: &str = "/parhel/alice";
    const UNIT: &str = "parhel-alice.mount";
    const UNIT_FILE: &str = "/etc/systemd/system/parhel-alice.mount";
    const MARKER_TEXT: &str =
        "format=1\nuser=alice\nimage=/var/lib/parhel/alice.img\nunit=parhel-alice.mount\n";

    fn request() -> SetupRequest {
        SetupRequest {
            size: DEFAULT_STORE_SIZE,
            preallocate: false,
            user: None,
            remove: false,
        }
    }

    fn removal() -> SetupRequest {
        SetupRequest {
            remove: true,
            ..request()
        }
    }

    fn assert_no_mutation(h: &FakeHost) {
        assert!(
            h.calls().is_empty(),
            "expected no side effects: {:#?}",
            h.lines()
        );
    }

    /// The machine as `setup` leaves it for alice, built by hand (so tests of what
    /// happens afterwards do not depend on the code under test).
    fn installed(h: &FakeHost) {
        for dir in [
            "/var/lib/parhel",
            "/parhel",
            ROOT,
            "/parhel/alice/baselines",
            "/parhel/alice/worktrees",
        ] {
            h.dirs.borrow_mut().insert(p(dir));
        }
        h.images.borrow_mut().insert(
            p(IMG),
            ImageUsage {
                apparent: DEFAULT_STORE_SIZE,
                allocated: 4096,
            },
        );
        h.formatted.borrow_mut().insert(p(IMG));
        h.files.borrow_mut().insert(
            p(UNIT_FILE),
            "[Unit]\nDescription=parhel store for alice\n\n[Mount]\nWhat=/var/lib/parhel/alice.img\n\
             Where=/parhel/alice\nType=btrfs\nOptions=loop,discard,noatime,nosuid,nodev\n\n[Install]\n\
             WantedBy=local-fs.target\n"
                .into(),
        );
        h.files
            .borrow_mut()
            .insert(p("/parhel/alice/.parhel-store"), MARKER_TEXT.into());
        h.mounted.borrow_mut().insert(p(ROOT));
        h.immutable.borrow_mut().insert(p("/var/lib/parhel"));
    }

    fn expected_setup() -> Vec<&'static str> {
        vec![
            "mkdir /var/lib/parhel",
            "image /var/lib/parhel/alice.img 107374182400 sparse",
            "run mkfs.btrfs -q -L parhel-alice /var/lib/parhel/alice.img",
            "mkdir /parhel",
            "mkdir /parhel/alice",
            "write /etc/systemd/system/parhel-alice.mount",
            "run systemctl daemon-reload",
            "run systemctl enable --now parhel-alice.mount",
            "mkdir /parhel/alice",
            "mkdir /parhel/alice/baselines",
            "mkdir /parhel/alice/worktrees",
            "write /parhel/alice/.parhel-store",
            "run chattr +i /var/lib/parhel",
            "verify-immutable /var/lib/parhel /var/lib/parhel/alice.img",
        ]
    }

    // U-SET-01: off Linux the command does not exist, and nothing is attempted. Real
    // (the platform guard is pure).
    #[test]
    fn refuses_off_linux() {
        let h = FakeHost {
            linux: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform);
        assert_no_mutation(&h);
    }

    // U-SET-02: without root, say which command to run; for removal, the removal.
    #[rstest]
    #[case::setup(request(), "sudo git parhel setup")]
    #[case::remove(removal(), "sudo git parhel setup --remove")]
    fn requires_root_and_names_the_sudo_command(#[case] req: SetupRequest, #[case] command: &str) {
        let h = FakeHost {
            root: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotRoot, "{err}");
        assert!(err.message.contains(command), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-03: no mkfs.btrfs: name the program and the package, touch nothing.
    #[test]
    fn missing_mkfs_names_the_package() {
        let h = FakeHost::default().without_tool("mkfs.btrfs");
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains("mkfs.btrfs"), "{}", err.message);
        assert!(err.message.contains("btrfs-progs"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-03b: every program setup runs is checked before anything is created: a
    // `blkid` or `mount` that turns out to be missing in the middle would leave an image
    // (and a half-made store) behind.
    #[rstest]
    #[case::blkid("blkid", "util-linux")]
    #[case::mount("mount", "util-linux")]
    fn missing_tools_are_found_before_anything_is_created(
        #[case] tool: &str,
        #[case] package: &str,
    ) {
        let h = FakeHost::default().without_tool(tool);
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains(tool), "{}", err.message);
        assert!(err.message.contains(package), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-09b: the user name ends up in paths, a unit file and a mount unit name; one
    // that systemd would expand (`%`) or that is not an account name is refused, whether
    // it was typed (`--user`) or inferred from `$SUDO_USER`.
    #[rstest]
    #[case::percent("a%b")]
    #[case::uppercase("Alice")]
    #[case::space("a b")]
    #[case::backslash("a\\b")]
    #[case::dots("../etc")]
    fn account_names_that_become_something_else_are_refused(#[case] name: &str) {
        let mut h = FakeHost::default();
        h.users.push(crate::ports::UserInfo {
            name: name.into(),
            uid: 1500,
            gid: 1500,
        });
        h.env.insert("SUDO_USER".into(), name.into());
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert_no_mutation(&h);
        let typed = SetupRequest {
            user: Some(name.into()),
            ..request()
        };
        assert_eq!(setup(&h, &typed).unwrap_err().code, ErrorCode::Usage);
        assert_no_mutation(&h);
    }

    // U-SET-04: btrfs in the kernel but not loaded: load it and go on.
    #[test]
    fn loads_the_btrfs_module_when_it_is_only_loadable() {
        let h = FakeHost::default();
        h.files
            .borrow_mut()
            .insert(p("/proc/filesystems"), "nodev\tsysfs\n\text4\n".into());
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.lines().first().map(String::as_str),
            Some("run modprobe btrfs")
        );
        assert!(
            h.lines()
                .contains(&"run systemctl enable --now parhel-alice.mount".to_owned())
        );
    }

    // U-SET-05: ... and if it cannot be loaded, stop before creating anything.
    #[test]
    fn kernel_without_btrfs_is_unsupported() {
        let h = FakeHost::default().failing("modprobe", ErrorCode::Io);
        h.files
            .borrow_mut()
            .insert(p("/proc/filesystems"), "nodev\tsysfs\n\text4\n".into());
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform, "{err}");
        assert!(err.message.contains("btrfs"), "{}", err.message);
        assert_eq!(
            h.lines(),
            vec!["run modprobe btrfs"],
            "only the attempt to load it"
        );
    }

    // U-SET-06
    #[test]
    fn no_systemd_is_unsupported() {
        let h = FakeHost {
            systemd: false,
            ..FakeHost::default()
        };
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedPlatform, "{err}");
        assert!(err.message.contains("systemd"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-07
    #[test]
    fn unknown_user_is_a_usage_error() {
        let h = FakeHost::default();
        let req = SetupRequest {
            user: Some("nobody-here".into()),
            ..request()
        };
        let err = setup(&h, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert!(err.message.contains("nobody-here"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-08: the store belongs to whoever ran sudo, unless told otherwise.
    #[test]
    fn the_user_defaults_to_the_sudo_user() {
        let h = FakeHost::default();
        let SetupOutcome::Created(summary) = setup(&h, &request()).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "alice");
        let h = FakeHost::default();
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "bob");
        assert_eq!(summary.store, p("/parhel/bob"));
    }

    // U-SET-09: root is never inferred as the store's owner (a root shell, `sudo -i`),
    // only chosen with an explicit `--user root`.
    #[rstest]
    #[case::root_shell(None)]
    #[case::sudo_from_root(Some("root"))]
    fn inferred_root_is_refused(#[case] sudo_user: Option<&str>) {
        let mut h = FakeHost::default();
        h.env.clear();
        if let Some(user) = sudo_user {
            h.env.insert("SUDO_USER".into(), user.into());
        }
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Usage, "{err}");
        assert!(err.message.contains("--user"), "{}", err.message);
        assert_no_mutation(&h);
    }

    #[test]
    fn explicit_root_is_honored() {
        let mut h = FakeHost::default();
        h.env.clear();
        let req = SetupRequest {
            user: Some("root".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(summary.user, "root");
        assert_eq!(
            h.attrs.borrow()[Path::new("/parhel/root")],
            (0o700, Some((0, 0)))
        );
    }

    // U-SET-10: the steps, in order. Every one is checked by the next.
    #[test]
    fn creates_the_store_step_by_step() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        assert_eq!(h.lines(), expected_setup());
    }

    // U-SET-11: who owns what, and who can read it.
    #[test]
    fn modes_and_owners() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let attrs = h.attrs.borrow();
        let alice = Some((1000, 1000));
        assert_eq!(attrs[Path::new("/var/lib/parhel")], (0o700, None));
        assert_eq!(attrs[Path::new(IMG)], (0o600, None));
        assert_eq!(attrs[Path::new("/parhel")], (0o755, None));
        assert_eq!(attrs[Path::new(UNIT_FILE)], (0o644, None));
        // The mounted filesystem's root, its directories and the marker are the user's.
        assert_eq!(attrs[Path::new(ROOT)], (0o700, alice));
        assert_eq!(attrs[Path::new("/parhel/alice/baselines")].1, alice);
        assert_eq!(attrs[Path::new("/parhel/alice/worktrees")].1, alice);
        assert_eq!(attrs[Path::new("/parhel/alice/.parhel-store")].1, alice);
    }

    // U-SET-12: the mount unit.
    #[test]
    fn writes_a_mount_unit_that_survives_reboots() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let unit = h.text(UNIT_FILE).expect("the unit exists");
        let lines: Vec<&str> = unit.lines().collect();
        for line in [
            "What=/var/lib/parhel/alice.img",
            "Where=/parhel/alice",
            "Type=btrfs",
            "Options=loop,discard,noatime,nosuid,nodev",
            "WantedBy=local-fs.target",
        ] {
            assert!(lines.contains(&line), "unit lacks `{line}`:\n{unit}");
        }
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Description=") && l.contains("parhel")),
            "{unit}"
        );
        let at = |header: &str| lines.iter().position(|l| *l == header).unwrap();
        assert!(at("[Unit]") < at("[Mount]") && at("[Mount]") < at("[Install]"));
        let section = |key: &str| lines.iter().position(|l| l.starts_with(key)).unwrap();
        assert!(at("[Mount]") < section("What=") && section("Options=") < at("[Install]"));
        assert!(at("[Install]") < section("WantedBy="));
    }

    // U-SET-13: the unit is named the way systemd names it (`systemd-escape`), even
    // for user names with a dash, and everything that refers to it uses that name.
    #[test]
    fn unit_name_comes_from_systemd_escape() {
        let mut h = FakeHost::default();
        h.users.push(crate::ports::UserInfo {
            name: "mary-ann".into(),
            uid: 1002,
            gid: 1002,
        });
        let req = SetupRequest {
            user: Some("mary-ann".into()),
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        let unit = "parhel-mary\\x2dann.mount";
        assert_eq!(summary.unit, unit);
        assert!(
            h.text(&format!("{UNIT_DIR}/{unit}")).is_some(),
            "{:?}",
            h.lines()
        );
        assert!(
            h.lines()
                .contains(&format!("run systemctl enable --now {unit}"))
        );
        let marker = h.text("/parhel/mary-ann/.parhel-store").unwrap();
        assert!(marker.contains(&format!("unit={unit}\n")), "{marker}");
    }

    // U-SET-14: the marker `add` looks for.
    #[test]
    fn marker_names_format_user_image_and_unit() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.text("/parhel/alice/.parhel-store").as_deref(),
            Some(MARKER_TEXT)
        );
    }

    // U-SET-15: size and allocation.
    #[rstest]
    #[case::default(None, false, 100u64 << 30, "sparse")]
    #[case::size(Some(512u64 << 20), false, 512u64 << 20, "sparse")]
    #[case::preallocated(Some(1u64 << 30), true, 1u64 << 30, "allocated")]
    fn image_size_and_allocation(
        #[case] size: Option<u64>,
        #[case] preallocate: bool,
        #[case] bytes: u64,
        #[case] kind: &str,
    ) {
        let h = FakeHost::default();
        let req = SetupRequest {
            size: size.unwrap_or(DEFAULT_STORE_SIZE),
            preallocate,
            ..request()
        };
        setup(&h, &req).unwrap();
        assert!(
            h.lines().contains(&format!("image {IMG} {bytes} {kind}")),
            "{:?}",
            h.lines()
        );
    }

    // U-SET-16: running it again changes nothing and says so.
    #[test]
    fn rerun_is_a_no_op() {
        let h = FakeHost::default();
        let SetupOutcome::Created(first) = setup(&h, &request()).unwrap() else {
            panic!("expected a new store");
        };
        h.clear_log();
        let SetupOutcome::AlreadySetUp(again) = setup(&h, &request()).unwrap() else {
            panic!("expected the existing store");
        };
        assert_no_mutation(&h);
        assert_eq!(again, first);
    }

    // U-SET-17: a store that was stopped is started again, nothing else.
    #[test]
    fn stopped_store_is_started_not_recreated() {
        let h = FakeHost::default();
        installed(&h);
        h.mounted.borrow_mut().clear();
        setup(&h, &request()).unwrap();
        assert_eq!(
            h.lines(),
            vec!["run systemctl enable --now parhel-alice.mount"]
        );
    }

    // U-SET-18: an image that exists is somebody's data: never formatted again, never
    // truncated, even when the rest is missing.
    #[test]
    fn existing_image_is_never_reformatted() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        h.immutable.borrow_mut().clear();
        setup(&h, &request()).unwrap();
        let lines = h.lines();
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("image ") || l.contains("mkfs")),
            "{lines:?}"
        );
        assert!(lines.contains(&"run systemctl enable --now parhel-alice.mount".to_owned()));
        assert!(h.is_mount_point(Path::new(ROOT)));
    }

    // U-SET-19: no chattr: the store works but its image is unprotected, and setup
    // says so loudly (an error) instead of printing success.
    #[test]
    fn missing_chattr_fails_loudly_but_leaves_a_working_store() {
        let h = FakeHost::default().without_tool("chattr");
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(
            err.message.contains("chattr") && err.message.contains("e2fsprogs"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains(IMG),
            "names what is unprotected: {}",
            err.message
        );
        let lines = h.lines();
        assert!(lines.contains(&"run systemctl enable --now parhel-alice.mount".to_owned()));
        assert!(h.text("/parhel/alice/.parhel-store").is_some(), "usable");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("disable") || l.starts_with("rm")),
            "nothing torn down: {lines:?}"
        );
    }

    // U-SET-20: a filesystem that refuses the attribute, or does not honor it, leaves
    // the image deletable: reported as an error that names the image, store kept.
    #[rstest]
    #[case::chattr_fails(FakeHost::default().failing("chattr", ErrorCode::Io))]
    #[case::attribute_not_honored(FakeHost {
        chattr_does_nothing: true,
        ..FakeHost::default()
    })]
    fn unprotected_image_is_reported(#[case] h: FakeHost) {
        let err = setup(&h, &request()).unwrap_err();
        assert!(err.message.contains(IMG), "{}", err.message);
        assert!(err.message.contains("protect"), "{}", err.message);
        let lines = h.lines();
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("disable") || l.starts_with("rm")),
            "the store stays usable: {lines:?}"
        );
        assert!(h.text("/parhel/alice/.parhel-store").is_some());
    }

    // U-SET-21: the summary.
    #[test]
    fn summary_reports_paths_size_and_allocation() {
        let h = FakeHost::default();
        let req = SetupRequest {
            size: 512 << 20,
            ..request()
        };
        let SetupOutcome::Created(summary) = setup(&h, &req).unwrap() else {
            panic!("expected a new store");
        };
        assert_eq!(
            summary,
            StoreSummary {
                user: "alice".into(),
                store: p(ROOT),
                image: p(IMG),
                apparent: 512 << 20,
                allocated: 4096,
                unit: UNIT.into(),
            }
        );
    }

    // U-SET-02b: the command to run names the options that were given.
    #[test]
    fn the_sudo_command_carries_the_options() {
        let h = FakeHost {
            root: false,
            ..FakeHost::default()
        };
        let req = SetupRequest {
            size: 512 << 20,
            preallocate: true,
            user: Some("bob".into()),
            remove: false,
        };
        let err = setup(&h, &req).unwrap_err();
        assert!(
            err.message
                .contains("sudo git parhel setup --size 536870912 --preallocate --user bob"),
            "{}",
            err.message
        );
    }

    // U-SET-22: a second user's image goes into the directory that protects the first
    // one: lifted for the new image, protected again after.
    #[test]
    fn a_second_users_store_lifts_and_restores_the_protection() {
        let h = FakeHost::default();
        installed(&h);
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        assert!(matches!(setup(&h, &req).unwrap(), SetupOutcome::Created(_)));
        let lines = h.lines();
        assert_eq!(
            lines.first().map(String::as_str),
            Some("run chattr -i /var/lib/parhel")
        );
        let at = |line: &str| lines.iter().position(|l| l == line).unwrap();
        assert!(
            at("run chattr -i /var/lib/parhel")
                < at("image /var/lib/parhel/bob.img 107374182400 sparse")
        );
        assert_eq!(
            &lines[lines.len() - 2..],
            [
                "run chattr +i /var/lib/parhel",
                "verify-immutable /var/lib/parhel /var/lib/parhel/bob.img"
            ]
        );
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
        assert!(
            h.exists(Path::new(IMG)),
            "the first user's image is untouched"
        );
        assert!(h.is_mount_point(Path::new(ROOT)));
    }

    // U-SET-22b: ... also when the second store fails half way.
    #[test]
    fn a_failed_run_does_not_leave_the_images_unprotected() {
        let h = FakeHost::default().failing("mkfs.btrfs", ErrorCode::Io);
        installed(&h);
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        setup(&h, &req).unwrap_err();
        assert_eq!(
            h.lines().last().map(String::as_str),
            Some("run chattr +i /var/lib/parhel")
        );
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    // U-SET-23: an image without any signature is what an interrupted setup leaves: it
    // is formatted, not recreated. One that holds another filesystem is data: refused.
    #[test]
    fn a_blank_image_is_formatted_and_a_foreign_one_is_refused() {
        let h = FakeHost::default();
        installed(&h);
        h.formatted.borrow_mut().clear();
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        h.immutable.borrow_mut().clear();
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Created(_)
        ));
        let lines = h.lines();
        assert!(lines.contains(&format!("run mkfs.btrfs -q -L parhel-alice {IMG}")));
        assert!(!lines.iter().any(|l| l.starts_with("image ")), "{lines:?}");

        let h = FakeHost::default();
        installed(&h);
        h.formatted.borrow_mut().clear();
        h.foreign.borrow_mut().insert(p(IMG), "ext4".into());
        h.mounted.borrow_mut().clear();
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(
            err.message.contains("ext4") && err.message.contains(IMG),
            "{}",
            err.message
        );
        assert_no_mutation(&h);
    }

    // U-SET-24: a store that lost its protection gets it back on a re-run, and nothing else.
    #[test]
    fn rerun_restores_a_lost_protection_only() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        h.immutable.borrow_mut().clear();
        h.clear_log();
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Restored(_)
        ));
        assert_eq!(
            h.lines(),
            vec![
                "run chattr +i /var/lib/parhel",
                "verify-immutable /var/lib/parhel /var/lib/parhel/alice.img"
            ]
        );
    }

    // U-SET-25: what is already there and is not this store's is never overwritten or
    // mounted over: a mount whose image is gone, a unit for something else, a marker
    // of another store.
    #[test]
    fn foreign_leftovers_are_refused_untouched() {
        // Mounted, image deleted behind it.
        let h = FakeHost::default();
        installed(&h);
        h.images.borrow_mut().remove(&p(IMG));
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains("--remove"), "{}", err.message);
        assert_no_mutation(&h);

        // A unit of that name that mounts something else.
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(
            p(UNIT_FILE),
            "[Mount]\nWhat=/elsewhere.img\nWhere=/parhel/alice\n".into(),
        );
        h.mounted.borrow_mut().clear();
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains("/elsewhere.img"), "{}", err.message);
        assert_no_mutation(&h);

        // A marker that says the filesystem is somebody else's.
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(
            p("/parhel/alice/.parhel-store"),
            "format=1\nuser=bob\nimage=/var/lib/parhel/bob.img\nunit=parhel-bob.mount\n".into(),
        );
        h.mounted.borrow_mut().clear();
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains("bob"), "{}", err.message);
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.starts_with("write ") || l.contains("mkfs")),
            "{:?}",
            h.lines()
        );
    }

    // U-SET-26: a store can be set up again after it was removed.
    #[test]
    fn setup_after_remove_builds_the_store_again() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        setup(&h, &removal()).unwrap();
        h.clear_log();
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Created(_)
        ));
        assert_eq!(h.lines(), expected_setup());
    }

    // ------------------------------------------------------------------- removal

    // U-SET-30: worktrees in the store are not ours to destroy: refuse, naming them.
    #[test]
    fn remove_refuses_while_worktrees_live_in_the_store() {
        let h = FakeHost::default();
        installed(&h);
        let id = "/parhel/alice/worktrees/7005185596c2e32bd0688ebe72725f92";
        for dir in [id.to_owned(), format!("{id}/feat"), format!("{id}/fix-bug")] {
            h.dirs.borrow_mut().insert(p(&dir));
        }
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(
            err.message.contains(&format!("{id}/feat")),
            "{}",
            err.message
        );
        assert!(
            err.message.contains(&format!("{id}/fix-bug")),
            "{}",
            err.message
        );
        assert_no_mutation(&h);
    }

    // U-SET-31: baselines are a cache; only worktrees block the removal.
    #[test]
    fn remove_ignores_baselines_and_empty_worktree_directories() {
        let h = FakeHost::default();
        installed(&h);
        h.dirs.borrow_mut().insert(p("/parhel/alice/baselines/abc"));
        h.dirs.borrow_mut().insert(p("/parhel/alice/worktrees/abc"));
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
    }

    // U-SET-32: the removal, in the order that matters: stop using the image, make the
    // directory writable, delete, tidy.
    #[test]
    fn remove_takes_the_store_down_in_order() {
        let h = FakeHost::default();
        installed(&h);
        let SetupOutcome::Removed(summary) = setup(&h, &removal()).unwrap() else {
            panic!("expected removal");
        };
        assert_eq!(
            h.lines(),
            vec![
                "run systemctl disable --now parhel-alice.mount",
                "rm /etc/systemd/system/parhel-alice.mount",
                "run systemctl daemon-reload",
                "run chattr -i /var/lib/parhel",
                "rm /var/lib/parhel/alice.img",
                "rmdir /parhel/alice",
            ]
        );
        assert_eq!(summary.user, "alice");
        assert_eq!(summary.unit, UNIT);
        assert!(
            !h.exists(Path::new(IMG))
                && !h.exists(Path::new(UNIT_FILE))
                && !h.exists(Path::new(ROOT))
        );
    }

    // U-SET-33: other users' images stay protected.
    #[test]
    fn remove_protects_the_directory_again_while_other_images_remain() {
        let h = FakeHost::default();
        installed(&h);
        h.images.borrow_mut().insert(
            p("/var/lib/parhel/bob.img"),
            ImageUsage {
                apparent: 1,
                allocated: 1,
            },
        );
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines().last().map(String::as_str),
            Some("run chattr +i /var/lib/parhel")
        );
        assert!(h.exists(Path::new("/var/lib/parhel/bob.img")));
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    #[test]
    fn remove_leaves_the_directory_unprotected_when_it_is_empty() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        assert!(
            !h.lines()
                .iter()
                .any(|l| l == "run chattr +i /var/lib/parhel")
        );
        assert!(!h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    // U-SET-34: removing what is not there is fine, and does nothing.
    #[test]
    fn remove_is_idempotent() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        h.clear_log();
        assert_eq!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::NothingToRemove {
                user: "alice".into()
            }
        );
        assert_no_mutation(&h);
    }

    // U-SET-35: removal follows the same user rules as creation.
    #[test]
    fn remove_infers_the_user_like_creation() {
        let mut h = FakeHost::default();
        h.env.clear();
        assert_eq!(setup(&h, &removal()).unwrap_err().code, ErrorCode::Usage);
        assert_no_mutation(&h);
    }

    // U-SET-36: a mount unit that is gone but a mounted image is not "nothing": the
    // pieces that exist are cleaned up, one by one.
    #[test]
    fn remove_cleans_up_a_partial_store() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        setup(&h, &removal()).unwrap();
        assert!(!h.exists(Path::new(IMG)));
        assert!(
            !h.lines().iter().any(|l| l.contains("disable")),
            "{:?}",
            h.lines()
        );
    }

    // U-SET-37: the removal deletes parhel's unit, image and mount point and nothing else
    // that happens to live next to them.
    #[test]
    fn remove_touches_only_parhels_paths() {
        let h = FakeHost::default();
        installed(&h);
        h.files
            .borrow_mut()
            .insert(p("/etc/systemd/system/other.mount"), "x".into());
        h.files
            .borrow_mut()
            .insert(p("/var/lib/parhel/notes.txt"), "mine".into());
        setup(&h, &removal()).unwrap();
        for call in h.calls() {
            if let HostCall::RemoveFile(path) | HostCall::RemoveDir(path) = call {
                assert!(
                    path == p(UNIT_FILE) || path == p(IMG) || path == p(ROOT),
                    "removed {}",
                    path.display()
                );
            }
        }
    }

    // U-SET-38: a stopped store is mounted to look for worktrees (they cannot be seen
    // otherwise), and put back down when they are there.
    #[test]
    fn remove_looks_inside_a_stopped_store() {
        let h = FakeHost::default();
        installed(&h);
        h.dirs.borrow_mut().insert(p("/parhel/alice/worktrees/abc"));
        h.dirs
            .borrow_mut()
            .insert(p("/parhel/alice/worktrees/abc/feat"));
        h.mounted.borrow_mut().clear();
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(
            err.message.contains("/parhel/alice/worktrees/abc/feat"),
            "{}",
            err.message
        );
        assert_eq!(
            h.lines(),
            vec![
                "run systemctl start parhel-alice.mount",
                "run systemctl stop parhel-alice.mount"
            ]
        );
        assert!(!h.is_mount_point(Path::new(ROOT)), "left as it was found");
    }

    // U-SET-39: a mount that no parhel unit owns is unmounted before its image goes.
    #[test]
    fn remove_unmounts_a_mount_without_a_unit() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines().first().map(String::as_str),
            Some("run umount /parhel/alice")
        );
        assert!(!h.exists(Path::new(IMG)) && !h.exists(Path::new(ROOT)));
    }

    // U-SET-40: staging directories of an `add` are not worktrees.
    #[test]
    fn remove_does_not_count_staging_directories() {
        let h = FakeHost::default();
        installed(&h);
        h.dirs
            .borrow_mut()
            .insert(p("/parhel/alice/worktrees/.parhel-staging-1"));
        h.dirs
            .borrow_mut()
            .insert(p("/parhel/alice/worktrees/.parhel-staging-1/node_modules"));
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
    }
}
