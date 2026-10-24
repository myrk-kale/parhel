//! `git parhel setup`: create (or remove) the per-user store. Linux, root.
//!
//! Every effect on the machine goes through the [`Host`] port, so the steps, their
//! order and what they do when something is already there are unit-tested with a
//! recording fake. The steps are idempotent: each one looks at what exists and does
//! only what is missing, so a run that stopped half way is finished by running it again.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::domain::request::DEFAULT_STORE_SIZE;
use crate::domain::store::{
    BASELINES_DIR, LINKS_FILE, LOCK_FILE, MARKER_FILE, Marker, REMOVING_MARK, WORKTREES_DIR,
};
use crate::domain::{Error, ErrorCode, Result, SetupRequest};
use crate::ports::{FileLock, Host, MountBacking, Owner, ToolState, UserInfo};

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
    // A `systemctl` that is there but not trusted is not "no systemd".
    if matches!(host.tool_state("systemctl"), ToolState::Untrusted(_)) {
        require_tool(host, "systemctl", "systemd")?;
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
        remove(host, &places, request.discard_contents)
    } else {
        create(host, &places, request)
    }
}

/// The command to run as root, with the options that were given.
fn sudo_command(request: &SetupRequest) -> String {
    let mut words = vec!["sudo git parhel setup".to_owned()];
    if request.remove {
        words.push("--remove".to_owned());
        if request.discard_contents {
            words.push("--discard-contents".to_owned());
        }
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
    match host.tool_state(tool) {
        ToolState::Usable => Ok(()),
        ToolState::Missing => Err(Error::new(
            ErrorCode::MissingTool,
            format!("`{tool}` is not installed: install the package {package} and run this again"),
        )),
        ToolState::Untrusted(why) => Err(Error::new(
            ErrorCode::MissingTool,
            format!(
                "`{tool}` cannot be run as root: {why}. As root, programs are taken only from \
                 /usr/sbin, /usr/bin, /sbin and /bin, and each must be owned by root and not \
                 writable by anyone else, like the directories above it; install the package \
                 {package} and run this again"
            ),
        )),
    }
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
                    discard_contents: false,
                }),
            ),
        ));
    }

    if mounted {
        verify_mount(host, places)?;
    }

    let mut progress = Progress::default();
    build(
        host,
        places,
        request,
        &Existing {
            image: image_present,
            unit: unit_text.is_some(),
            mounted,
        },
        &mut progress,
    )?;

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
    let created = !existing.image;
    if created {
        new_image(host, places)?;
        progress.built = true;
    }
    // A mounted image is in use - and has a filesystem, whatever `blkid` makes of it. One
    // just made, or that an interrupted run left empty, has none.
    let found = if existing.mounted {
        Some("btrfs".to_owned())
    } else if created || host.image_usage(&places.image)?.apparent == 0 {
        None
    } else {
        host.filesystem_of(&places.image)?
    };
    match found {
        None => {
            // Sized now, with the directory protected again: preallocating a big image
            // takes long, and it can be done to a file in an immutable directory.
            host.size_image(&places.image, request.size, request.preallocate)?;
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
        // What is there now is the unit's doing - unless something else was mounted over it.
        verify_mount(host, places)?;
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
    ensure_store_lock(host, places, progress)?;

    if !host.is_immutable(Path::new(STATE_DIR)) {
        protect(host, places)?;
        progress.changed = true;
    }
    Ok(())
}

/// The store-wide lock file is there, the user's, and does not say the store is going away.
///
/// Made when missing (a new store, or one set up before the lock existed) - never replaced:
/// a run that holds the lock on the old file would be outside the new one. And a store whose
/// removal died after sealing it (killed, power lost) is closed to `add` for good until this
/// takes the mark out. A file that is there and unmarked costs nothing: it is only read.
fn ensure_store_lock(host: &dyn Host, places: &Places, progress: &mut Progress) -> Result<()> {
    let path = places.root.join(LOCK_FILE);
    let sealed = match host.read_text(&path)? {
        None => false,
        Some(text) if text.starts_with(REMOVING_MARK) => true,
        Some(_) => return Ok(()),
    };
    let Some(lock) = host.lock_store(&path, places.owner)? else {
        // Somebody holds it: it exists. Only a sealed store needs more than that.
        if sealed {
            return Err(Error::new(
                ErrorCode::StoreInUse,
                format!(
                    "the store for {} is marked as going away and a run holds its lock right \
                     now (a `setup --remove` that has not finished, or an `add` reading the \
                     mark); wait a moment and run this again",
                    places.user.name
                ),
            ));
        }
        return Ok(());
    };
    if sealed {
        host.set_store_sealed(&path, false)?;
    }
    drop(lock);
    progress.changed = true;
    Ok(())
}

/// The filesystem mounted at the store's place must be the store's image. Whatever else is
/// there is not ours: it is not laid out (a directory, a marker, a `chown`), not protected on
/// its behalf and not unmounted.
fn verify_mount(host: &dyn Host, places: &Places) -> Result<()> {
    match host.mount_backing(&places.root, &places.image)? {
        MountBacking::NotMounted | MountBacking::Image => Ok(()),
        MountBacking::Other(what) => Err(Error::new(
            ErrorCode::StoreUnavailable,
            format!(
                "{} is a mount point of {what}, not of {}: `git parhel setup` does not lay \
                 out, protect or unmount what it did not mount. Unmount it (or move it \
                 elsewhere) and run this again; nothing was changed",
                places.root.display(),
                places.image.display()
            ),
        )),
    }
}

/// Make the image file, exclusively. `/var/lib/parhel` is immutable once there is an
/// image, which forbids creating entries even for root: when it protects other users'
/// images it is opened for this one call and closed again at once, with SIGINT, SIGTERM
/// and SIGHUP held back for the moment (a Ctrl-C there would leave every image
/// unprotected). Sizing and formatting come after, with the directory protected.
fn new_image(host: &dyn Host, places: &Places) -> Result<()> {
    let state_dir = Path::new(STATE_DIR);
    let make = || {
        host.create_dir(state_dir, 0o700, None)
            .and_then(|()| host.create_image(&places.image))
    };
    if !host.is_immutable(state_dir) {
        // The first image: nothing to protect yet, the protection comes at the end.
        return make();
    }
    host.with_signals_deferred(&mut || {
        host.run("chattr", &["-i", STATE_DIR])?;
        let image = make();
        // Whatever happened.
        host.run("chattr", &["+i", STATE_DIR])
            .map_err(|e| lost_protection(&e.message))?;
        image
    })?;
    host.verify_immutable(state_dir, &places.image)
        .map_err(|e| lost_protection(&e.message))
}

/// The immutable directory that holds the other users' images could not be made immutable
/// again after a new image was added: all of them can be deleted until it is.
fn lost_protection(why: &str) -> Error {
    Error::new(
        ErrorCode::Io,
        format!(
            "{STATE_DIR} is NOT protected from deletion any more, and the images in it are not \
             either: {why}. Run `sudo chattr +i {STATE_DIR}` yourself, then run `git parhel \
             setup` again"
        ),
    )
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
        discard_contents: false,
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
    let chattr = match host.tool_state("chattr") {
        ToolState::Usable => None,
        ToolState::Missing => Some("`chattr` is not installed (package e2fsprogs)".to_owned()),
        ToolState::Untrusted(why) => Some(format!("`chattr` cannot be run as root: {why}")),
    };
    if let Some(problem) = chattr {
        return Err(Error::new(
            ErrorCode::MissingTool,
            format!(
                "the store is ready, but its image {image} is NOT protected from deletion: \
                 {problem}. Anything that runs `rm` as root would delete every worktree in the \
                 store with it. Install e2fsprogs (in /usr/bin or /usr/sbin) and run `{}` \
                 again: it only adds the protection",
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

/// How the contents of the store were reached, and what to undo about it.
enum Way {
    /// Mounted already: nothing to undo.
    Mounted,
    /// Its unit was started to look: stop it again unless the store is going away.
    Started,
    /// The image is mounted read-only on this private directory: unmount it.
    Probe(PathBuf),
}

/// The marker of a store being looked at.
#[derive(Debug, PartialEq, Eq)]
enum MarkerState {
    Ours,
    /// There is none.
    Missing,
    /// There is one that is not this user's (or not a marker at all).
    Foreign(String),
}

/// What is in a store being removed.
struct Contents {
    marker: MarkerState,
    /// Names in the store root that are not parhel's.
    extras: Vec<OsString>,
    /// Whether the store root holds anything at all.
    empty: bool,
    /// Worktrees, as paths under the store's mount point.
    worktrees: Vec<PathBuf>,
    /// Symlinks the registry says were made for the store's worktrees, once each.
    links: Vec<PathBuf>,
}

/// What looking at the store came to.
struct Looked {
    way: Option<Way>,
    /// `Err(why)` when the contents could not be reached: nothing to judge.
    contents: std::result::Result<Option<Contents>, String>,
    /// The store-wide lock, held exclusively since before the store was looked at, when the
    /// store is mounted at its place (and so can be in use). Not for an image mounted
    /// read-only on a private directory: see [`look`].
    lock: Option<FileLock>,
}

fn remove(host: &dyn Host, places: &Places, discard: bool) -> Result<SetupOutcome> {
    let unit_text = host.read_text(&places.unit_file)?;
    let image_present = host.exists(&places.image);
    let mounted = host.is_mount_point(&places.root);
    let root_present = host.exists(&places.root);
    if !image_present && unit_text.is_none() && !mounted && !root_present {
        return Ok(SetupOutcome::NothingToRemove {
            user: places.user.name.clone(),
        });
    }
    if let Some(text) = &unit_text {
        check_unit(places, text)?;
    }
    if mounted {
        verify_mount(host, places)?;
    }

    // The worktrees are the user's work, and so is whatever else is in there. Looked at
    // before anything is deleted: through the mount if it is up, by starting the unit if
    // there is one, or by mounting the image read-only on a private directory when the
    // unit is gone. A store that is mounted is looked at under its lock, held exclusively.
    let mut looked = look(host, places, unit_text.is_some(), image_present, mounted)?;
    let verdict = judge(places, &looked, discard);
    match (&looked.way, &verdict) {
        (Some(Way::Probe(dir)), _) => {
            let unmounted = host.umount(dir);
            if verdict.is_ok() {
                unmounted?;
            }
        }
        (Some(Way::Started), Err(_)) => {
            // An open file keeps a mount busy: the lock goes first.
            drop(looked.lock.take());
            let _ = host.run("systemctl", &["stop", &places.unit]);
        }
        _ => {}
    }
    verdict?;
    if let Ok(Some(contents)) = &looked.contents {
        announce_links(host, &contents.links);
    }

    let usage = if image_present {
        host.image_usage(&places.image).ok()
    } else {
        None
    };

    // From here the store goes away. The lock cannot be held through the unmount - an open
    // file keeps the mount busy, and `umount` would refuse - so the store is sealed first:
    // the lock file says it is going away, which an `add` that takes the lock after this
    // reads and does not start. (One that took it before is in the lock's way, and `look`
    // would not have got it.)
    let sealed = match looked.lock.take() {
        Some(lock) => {
            seal(host, places, lock)?;
            true
        }
        None => false,
    };
    let taken_down = take_down(
        host,
        places,
        &Existing {
            image: image_present,
            unit: unit_text.is_some(),
            mounted,
        },
        root_present,
        sealed,
    );
    if let Err(e) = taken_down {
        return Err(if sealed { reopen(host, places, e) } else { e });
    }

    Ok(SetupOutcome::Removed(StoreSummary {
        user: places.user.name.clone(),
        store: places.root.clone(),
        image: places.image.clone(),
        apparent: usage.map_or(0, |u| u.apparent),
        allocated: usage.map_or(0, |u| u.allocated),
        unit: places.unit.clone(),
    }))
}

/// Mark the store as going away in its lock file, which `lock` holds, and let go of the lock.
fn seal(host: &dyn Host, places: &Places, lock: FileLock) -> Result<()> {
    let path = places.root.join(LOCK_FILE);
    let sealed = host.set_store_sealed(&path, true);
    if sealed.is_err() {
        // Whatever was written, while the lock is still ours.
        let _ = host.set_store_sealed(&path, false);
    }
    drop(lock);
    sealed
}

/// Stop using the image and delete it: the unit (or the bare mount) goes first, then the
/// image and the mount point. `sealed`: the store is closed to `add` while this runs.
fn take_down(
    host: &dyn Host,
    places: &Places,
    existing: &Existing,
    root_present: bool,
    sealed: bool,
) -> Result<()> {
    if sealed {
        crate::failpoint!("store_remove_sealed");
    }
    // Stop using the image, then forget the unit.
    if existing.unit {
        host.run("systemctl", &["disable", "--now", &places.unit])?;
        host.remove_file(&places.unit_file)?;
        host.run("systemctl", &["daemon-reload"])?;
    } else if existing.mounted {
        host.umount(&places.root)?;
    }

    // The directory is immutable: lift it to delete, and put it back for the others.
    let state_dir = Path::new(STATE_DIR);
    let lifted = host.is_immutable(state_dir);
    if lifted {
        host.run("chattr", &["-i", STATE_DIR])?;
    }
    let deleted = (|| -> Result<()> {
        if existing.image {
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
    deleted
}

/// A removal that failed after the store was sealed opens it again, if it is still there to
/// be opened, so that it goes on working; the error is the real one. When the mark cannot
/// be taken out (a run holds the lock) it says so, and what to do.
fn reopen(host: &dyn Host, places: &Places, error: Error) -> Error {
    // Only the store that is mounted at its place: where it is not, nothing is made.
    if !matches!(
        host.mount_backing(&places.root, &places.image),
        Ok(MountBacking::Image)
    ) {
        return error;
    }
    let path = places.root.join(LOCK_FILE);
    let reopened = match host.lock_store(&path, places.owner) {
        Ok(Some(lock)) => {
            let done = host.set_store_sealed(&path, false).is_ok();
            drop(lock);
            done
        }
        _ => false,
    };
    if reopened {
        return error;
    }
    Error::new(
        error.code,
        format!(
            "{}\nthe store is still closed to `git parhel add` (a removal marked it as going \
             away): `sudo git parhel setup` opens it again",
            error.message
        ),
    )
}

/// Reach the contents of the store, whichever way works, and read them.
///
/// `Err` is for what is wrong with the machine (an unreadable directory); a store that
/// cannot be reached is reported in the result, for the caller to refuse or, with
/// `--discard-contents`, to go on.
///
/// A store that is mounted at its place can be in use, so it is looked at under its
/// lock, taken exclusively and without waiting before anything is read: whoever works in
/// it - an `add`, a `doctor --fix` - holds the lock shared, and if anyone does, a run is
/// in progress and the removal stops (`store-in-use`) instead of deleting the image under
/// it. The lock is the way the check "no worktrees" and the deletion stay one decision: no
/// run can publish a worktree between them.
///
/// An image whose unit is gone is not mounted at its place but read-only on a private
/// directory of root's (mode 0700, under `/run`), which no user can enter; `add` finds a
/// store by its marker at `/parhel/<user>`, never there. No run can be using it, so there
/// is nothing to take a lock on - and no lock file is made on a mount point with nothing
/// mounted. (Someone mounting the image at the store's place by hand, as root, while this
/// runs is outside what a lock can guard.)
fn look(
    host: &dyn Host,
    places: &Places,
    has_unit: bool,
    image_present: bool,
    mounted: bool,
) -> Result<Looked> {
    let unreachable = |why: String| {
        Ok(Looked {
            way: None,
            contents: Err(why),
            lock: None,
        })
    };
    let (way, at) = if mounted {
        (Way::Mounted, places.root.clone())
    } else if !image_present {
        // A unit or a mount point and no image: there is nothing to look at.
        return Ok(Looked {
            way: None,
            contents: Ok(None),
            lock: None,
        });
    } else if has_unit {
        if let Err(e) = host.run("systemctl", &["start", &places.unit]) {
            return unreachable(format!("its unit does not start: {}", e.message));
        }
        if !host.is_mount_point(&places.root) {
            return unreachable("its unit started but it is not mounted".to_owned());
        }
        (Way::Started, places.root.clone())
    } else {
        // The unit is gone (deleted by hand): the image is mounted read-only on a private
        // directory - unless it holds nothing to look at, or something that is not ours.
        require_tool(host, "mount", "util-linux")?;
        require_tool(host, "umount", "util-linux")?;
        require_tool(host, "blkid", "util-linux")?;
        match host.filesystem_of(&places.image)? {
            None => {
                return Ok(Looked {
                    way: None,
                    contents: Ok(None),
                    lock: None,
                });
            }
            Some(kind) if kind == "btrfs" => {}
            Some(kind) => {
                return unreachable(format!("it holds {kind}, not btrfs"));
            }
        }
        match host.mount_readonly(&places.image) {
            Ok(dir) => (Way::Probe(dir.clone()), dir),
            Err(e) => return unreachable(format!("it does not mount: {}", e.message)),
        }
    };
    // As found: a unit started to look is stopped again, a private mount unmounted. The lock
    // is let go of before, an open file keeps a mount busy.
    let put_back = |way: &Way, lock: Option<FileLock>| {
        drop(lock);
        match way {
            Way::Probe(dir) => {
                let _ = host.umount(dir);
            }
            Way::Started => {
                let _ = host.run("systemctl", &["stop", &places.unit]);
            }
            Way::Mounted => {}
        }
    };
    let lock = if matches!(way, Way::Probe(_)) {
        None
    } else {
        match lock_for_removal(host, places) {
            Ok(lock) => Some(lock),
            Err(e) => {
                put_back(&way, None);
                return Err(e);
            }
        }
    };
    match inspect_under_lock(host, places, &at) {
        Ok(contents) => Ok(Looked {
            way: Some(way),
            contents: Ok(Some(contents)),
            lock,
        }),
        Err(e) => {
            put_back(&way, lock);
            Err(e)
        }
    }
}

/// [`inspect`], with the lock taken (a test freezes the removal here to start an `add`).
fn inspect_under_lock(host: &dyn Host, places: &Places, at: &Path) -> Result<Contents> {
    crate::failpoint!("store_remove_locked");
    inspect(host, places, at)
}

/// The store-wide lock, exclusively and without waiting: `store-in-use` when a run holds it.
fn lock_for_removal(host: &dyn Host, places: &Places) -> Result<FileLock> {
    let path = places.root.join(LOCK_FILE);
    let taken = host.lock_store(&path, places.owner).map_err(|e| {
        // The lock file is the user's, in a directory that is theirs: a link or a FIFO
        // planted there is refused, and the way out is to delete it.
        Error::new(
            e.code,
            format!(
                "{}\nthe store's lock file is the user's: delete `{}` and run this again",
                e.message,
                path.display()
            ),
        )
    })?;
    taken.ok_or_else(|| {
        Error::new(
            ErrorCode::StoreInUse,
            format!(
                "the store for {} is in use: a run is in progress (a `git parhel add`, a `git \
                 parhel doctor --fix` or another `setup --remove`; it holds the store's lock), \
                 so nothing was looked at or deleted. Wait for it to finish and run this again",
                places.user.name
            ),
        )
    })
}

/// Read the store whose root is `at` (its mount point, or a private mount of its image).
fn inspect(host: &dyn Host, places: &Places, at: &Path) -> Result<Contents> {
    let names = host.list_dir(at)?;
    let marker = match host.read_text(&at.join(MARKER_FILE))? {
        None => MarkerState::Missing,
        Some(text) => match Marker::parse(&text) {
            Ok(found) if found == places.marker() => MarkerState::Ours,
            Ok(found) => MarkerState::Foreign(format!(
                "it says the store is for `{}` (image {}, unit {})",
                found.user,
                found.image.display(),
                found.unit
            )),
            Err(e) => MarkerState::Foreign(e.message),
        },
    };
    let own = [
        MARKER_FILE,
        BASELINES_DIR,
        WORKTREES_DIR,
        LINKS_FILE,
        LOCK_FILE,
    ];
    let extras = names
        .iter()
        .filter(|name| !own.iter().any(|o| name.as_encoded_bytes() == o.as_bytes()))
        .cloned()
        .collect();
    // The paths are shown where they will be once the store is mounted.
    let worktrees = worktrees_in(host, at)?
        .into_iter()
        .map(|w| places.root.join(w.strip_prefix(at).unwrap_or(&w)))
        .collect();
    Ok(Contents {
        marker,
        extras,
        empty: names.is_empty(),
        worktrees,
        links: registered_links(host, &at.join(LINKS_FILE)),
    })
}

/// The symlinks named in the registry (`link TAB target` lines), once each. Best effort:
/// the registry is the user's file, and a store is being removed anyway.
fn registered_links(host: &dyn Host, registry: &Path) -> Vec<PathBuf> {
    let Ok(Some(text)) = host.read_text(registry) else {
        return Vec::new();
    };
    let mut links: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        if let Some((link, _)) = line.split_once('\t') {
            let link = PathBuf::from(link);
            if !link.as_os_str().is_empty() && !links.contains(&link) {
                links.push(link);
            }
        }
    }
    links
}

/// What to do about what was found: `Ok` to go on, or the refusal.
fn judge(places: &Places, looked: &Looked, discard: bool) -> Result<()> {
    let user = &places.user.name;
    let by_hand = format!(
        "to look yourself: `sudo mount -t btrfs -o loop,ro,nodev,nosuid,noexec {} /mnt`, \
         `ls -la /mnt`, `sudo umount /mnt`; to delete it anyway: `sudo git parhel setup \
         --remove --user {user} --discard-contents`",
        places.image.display()
    );
    let contents = match &looked.contents {
        Ok(None) => return Ok(()),
        Ok(Some(contents)) => contents,
        Err(_) if discard => return Ok(()),
        Err(why) => {
            return Err(Error::new(
                ErrorCode::StoreUnavailable,
                format!(
                    "cannot look inside {}: {why}, so it cannot be known whether it holds \
                     worktrees or anything else of value. For now nothing was deleted; {by_hand}",
                    places.image.display()
                ),
            ));
        }
    };
    // Work in progress: never discarded by the flag.
    if !contents.worktrees.is_empty() {
        let list = contents
            .worktrees
            .iter()
            .fold(String::new(), |mut list, w| {
                let _ = write!(list, "\n  {}", w.display());
                list
            });
        return Err(Error::new(
            ErrorCode::StoreInUse,
            format!(
                "the store for {user} still holds {} worktree(s):{list}\nremove each one with \
                 `git worktree unlock <path>` and `git worktree remove <path>` from its \
                 repository (then `git parhel doctor --fix` clears the symlinks left behind) \
                 and run this again. For now nothing was deleted",
                contents.worktrees.len()
            ),
        ));
    }
    if discard {
        return Ok(());
    }
    let marker_trouble = match &contents.marker {
        MarkerState::Ours => None,
        MarkerState::Missing if contents.empty => None,
        MarkerState::Missing => Some("it has no store marker but is not empty".to_owned()),
        MarkerState::Foreign(why) => Some(format!("its marker is not this store's: {why}")),
    };
    if let Some(why) = marker_trouble {
        return Err(Error::new(
            ErrorCode::StoreUnavailable,
            format!(
                "{} does not look like the store for {user}: {why}. For now nothing was \
                 deleted; {by_hand}",
                places.image.display()
            ),
        ));
    }
    if !contents.extras.is_empty() {
        let list = contents
            .extras
            .iter()
            .fold(String::new(), |mut list, name| {
                let _ = write!(list, "\n  {}", places.root.join(name).display());
                list
            });
        return Err(Error::new(
            ErrorCode::StoreInUse,
            format!(
                "the store for {user} holds files that are not the store's own:{list}\nmove \
                 them out and run this again, or delete them with the store: `sudo git parhel \
                 setup --remove --user {user} --discard-contents`. For now nothing was deleted"
            ),
        ));
    }
    Ok(())
}

/// Print the symlinks that will be left pointing nowhere, before the registry that names
/// them is deleted with the store.
fn announce_links(host: &dyn Host, links: &[PathBuf]) {
    const SHOWN: usize = 50;
    if links.is_empty() {
        return;
    }
    let mut text = String::from(
        "these symlinks were made for the store's worktrees and will dangle once the store is \
         gone (the registry that names them is deleted with it); remove them yourself \
         afterwards:",
    );
    for link in links.iter().take(SHOWN) {
        let _ = write!(text, "\n  {}", link.display().to_string().escape_debug());
    }
    if links.len() > SHOWN {
        let _ = write!(text, "\n  ... and {} more", links.len() - SHOWN);
    }
    host.notice(&text);
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
    use crate::domain::store::REMOVING_MARK;
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
            discard_contents: false,
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
        h.image_roots.borrow_mut().insert(p(IMG), p(ROOT));
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
        h.files
            .borrow_mut()
            .insert(p("/parhel/alice/.parhel-store.lock"), String::new());
        h.mounted.borrow_mut().insert(p(ROOT));
        h.immutable.borrow_mut().insert(p("/var/lib/parhel"));
    }

    fn expected_setup() -> Vec<&'static str> {
        vec![
            "mkdir /var/lib/parhel",
            "create-image /var/lib/parhel/alice.img",
            "size-image /var/lib/parhel/alice.img 107374182400 sparse",
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
            "lock-store /parhel/alice/.parhel-store.lock",
            "unlock-store /parhel/alice/.parhel-store.lock",
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

    // U-SET-03c: a program that is there but where root does not trust it (it belongs to a
    // user, or anybody could replace it) is not run; the error says that, and which file
    // failed which check, instead of "not installed" - and nothing is created. Every
    // program the run starts is checked up front, systemd's included.
    #[rstest]
    #[case::mkfs("mkfs.btrfs", "btrfs-progs")]
    #[case::blkid("blkid", "util-linux")]
    #[case::mount("mount", "util-linux")]
    #[case::systemd_escape("systemd-escape", "systemd")]
    #[case::systemctl("systemctl", "systemd")]
    fn untrusted_tools_are_refused_with_the_reason_before_anything_is_created(
        #[case] tool: &str,
        #[case] package: &str,
    ) {
        let why = format!("`/usr/sbin/{tool}` is owned by uid 1000, not by root");
        let h = FakeHost::default().with_untrusted_tool(tool, &why);
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains(&why), "{}", err.message);
        assert!(err.message.contains(package), "{}", err.message);
        assert!(!err.message.contains("is not installed"), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-03d: removal checks the programs it will run the same way: `mount`, `umount` and
    // `blkid` when it has to look into an image whose unit is gone, `systemd-escape` always.
    #[rstest]
    #[case::systemd_escape("systemd-escape", false)]
    #[case::umount("umount", true)]
    #[case::blkid("blkid", true)]
    #[case::mount("mount", true)]
    fn removal_refuses_untrusted_tools_too(#[case] tool: &str, #[case] unit_gone: bool) {
        let why = format!("`/usr/bin/{tool}` can be written by everyone (mode 0777)");
        let h = FakeHost::default().with_untrusted_tool(tool, &why);
        installed(&h);
        if unit_gone {
            h.files.borrow_mut().remove(&p(UNIT_FILE));
            h.mounted.borrow_mut().clear();
        }
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains(&why), "{}", err.message);
        assert!(h.exists(Path::new(IMG)), "the image is intact");
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.starts_with("rm") || l.contains("chattr")),
            "{:?}",
            h.lines()
        );
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
            h.lines()
                .contains(&format!("size-image {IMG} {bytes} {kind}")),
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
                .any(|l| l.starts_with("create-image ") || l.contains("mkfs")),
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

    // U-SET-19b: ... and a `chattr` that is there but not trusted as root is the same: the
    // store works, the image is not protected, and the error says why `chattr` was not run.
    #[test]
    fn an_untrusted_chattr_fails_loudly_but_leaves_a_working_store() {
        let why = "`/usr/bin/chattr` is owned by uid 1000, not by root";
        let h = FakeHost::default().with_untrusted_tool("chattr", why);
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MissingTool, "{err}");
        assert!(err.message.contains(why), "{}", err.message);
        assert!(err.message.contains(IMG), "{}", err.message);
        assert!(err.message.contains("NOT protected"), "{}", err.message);
        assert!(!err.message.contains("is not installed"), "{}", err.message);
        assert!(!h.lines().iter().any(|l| l.contains("chattr")), "never run");
        assert!(h.text("/parhel/alice/.parhel-store").is_some(), "usable");
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
            discard_contents: false,
        };
        let err = setup(&h, &req).unwrap_err();
        assert!(
            err.message
                .contains("sudo git parhel setup --size 536870912 --preallocate --user bob"),
            "{}",
            err.message
        );
    }

    /// The machine after alice's store, with the user bob to set up next.
    fn alice_is_set_up() -> (FakeHost, SetupRequest) {
        let h = FakeHost::default();
        installed(&h);
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        (h, req)
    }

    // U-SET-02c: ... and the removal that was asked to discard names the flag too.
    #[test]
    fn the_sudo_command_of_a_removal_carries_discard_contents() {
        let h = FakeHost {
            root: false,
            ..FakeHost::default()
        };
        let req = SetupRequest {
            discard_contents: true,
            user: Some("bob".into()),
            ..removal()
        };
        let err = setup(&h, &req).unwrap_err();
        assert!(
            err.message
                .contains("sudo git parhel setup --remove --discard-contents --user bob"),
            "{}",
            err.message
        );
    }

    // U-SET-22: a second user's image goes into the directory that protects the first
    // one: lifted for the creation of the new image, and put back at once.
    #[test]
    fn a_second_users_store_lifts_and_restores_the_protection() {
        let (h, req) = alice_is_set_up();
        assert!(matches!(setup(&h, &req).unwrap(), SetupOutcome::Created(_)));
        let lines = h.lines();
        let at = |line: &str| {
            lines
                .iter()
                .position(|l| l == line)
                .unwrap_or_else(|| panic!("no `{line}` in {lines:#?}"))
        };
        assert!(at("run chattr -i /var/lib/parhel") < at("create-image /var/lib/parhel/bob.img"));
        assert!(at("create-image /var/lib/parhel/bob.img") < at("run chattr +i /var/lib/parhel"));
        // The new image is checked for protection too, not only the first one.
        assert!(
            lines.contains(&"verify-immutable /var/lib/parhel /var/lib/parhel/bob.img".to_owned()),
            "{lines:#?}"
        );
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
        assert!(
            h.exists(Path::new(IMG)),
            "the first user's image is untouched"
        );
        assert!(h.is_mount_point(Path::new(ROOT)));
    }

    // U-SET-22b: ... also when the second store fails half way: the images were already
    // protected again before anything slow ran.
    #[test]
    fn a_failed_run_does_not_leave_the_images_unprotected() {
        let h = FakeHost::default().failing("mkfs.btrfs", ErrorCode::Io);
        installed(&h);
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        setup(&h, &req).unwrap_err();
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
        let lines = h.lines();
        let at = |prefix: &str| lines.iter().position(|l| l.starts_with(prefix)).unwrap();
        assert!(at("run chattr +i") < at("size-image"), "{lines:#?}");
        assert!(at("run chattr +i") < at("run mkfs.btrfs"), "{lines:#?}");
    }

    // U-SET-22c: the directory is open only for the exclusive creation of the new image:
    // nothing between the two chattr runs but the creation (the directory itself, the
    // image), signals held back all that time, and sizing - which can take long when the
    // image is preallocated - and mkfs come after.
    #[rstest]
    #[case::sparse(false, "sparse")]
    #[case::preallocated(true, "allocated")]
    fn the_protection_is_off_only_around_the_creation(
        #[case] preallocate: bool,
        #[case] kind: &str,
    ) {
        let (h, mut req) = alice_is_set_up();
        req.preallocate = preallocate;
        setup(&h, &req).unwrap();
        assert_eq!(
            &h.lines()[..6],
            [
                "signals held back",
                "run chattr -i /var/lib/parhel",
                "mkdir /var/lib/parhel",
                "create-image /var/lib/parhel/bob.img",
                "run chattr +i /var/lib/parhel",
                "signals let through",
            ]
        );
        let lines = h.lines();
        let size = lines
            .iter()
            .position(|l| *l == format!("size-image /var/lib/parhel/bob.img 107374182400 {kind}"))
            .expect("sized");
        let mkfs = lines
            .iter()
            .position(|l| l.starts_with("run mkfs.btrfs"))
            .expect("formatted");
        assert!(5 < size && size < mkfs, "{lines:#?}");
    }

    // U-SET-22d: an image that cannot be created does not leave the protection off, and
    // does not leave the signals held back.
    #[test]
    fn a_creation_that_fails_still_restores_the_protection_and_the_signals() {
        let (h, req) = alice_is_set_up();
        h.fail_create_image.set(true);
        let err = setup(&h, &req).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert_eq!(
            h.lines(),
            vec![
                "signals held back",
                "run chattr -i /var/lib/parhel",
                "mkdir /var/lib/parhel",
                "create-image /var/lib/parhel/bob.img",
                "run chattr +i /var/lib/parhel",
                "signals let through",
            ]
        );
        assert!(h.immutable.borrow().contains(Path::new("/var/lib/parhel")));
    }

    // U-SET-22e: the protection that cannot be put back is an error that says so, loudly,
    // naming the command: every other image is unprotected until it is.
    #[test]
    fn a_protection_that_cannot_be_restored_is_reported() {
        let h = FakeHost {
            chattr_does_nothing: true,
            ..FakeHost::default()
        };
        installed(&h);
        let req = SetupRequest {
            user: Some("bob".into()),
            ..request()
        };
        let err = setup(&h, &req).unwrap_err();
        assert!(
            err.message.contains("chattr +i /var/lib/parhel"),
            "{}",
            err.message
        );
        assert!(err.message.contains("NOT protected"), "{}", err.message);
    }

    // U-SET-22f: the first user's directory is not protected yet: nothing is lifted, no
    // signal is held back for it, and the protection comes at the end as before.
    #[test]
    fn the_first_store_has_no_window() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let lines = h.lines();
        assert!(!lines.iter().any(|l| l.starts_with("signals")), "{lines:?}");
        assert!(!lines.iter().any(|l| l == "run chattr -i /var/lib/parhel"));
    }

    // U-SET-22g: an interrupted run leaves a blank image (created, not sized, or sized and
    // not formatted): the next one finishes it - sized again, never recreated.
    #[test]
    fn a_blank_image_from_an_interrupted_run_is_sized_and_formatted() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
        h.immutable.borrow_mut().clear();
        h.formatted.borrow_mut().clear();
        h.images.borrow_mut().insert(
            p(IMG),
            ImageUsage {
                apparent: 0,
                allocated: 0,
            },
        );
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Created(_)
        ));
        let lines = h.lines();
        assert!(
            !lines.iter().any(|l| l.starts_with("create-image ")),
            "{lines:?}"
        );
        let size = lines
            .iter()
            .position(|l| l.starts_with("size-image "))
            .unwrap();
        let mkfs = lines.iter().position(|l| l.contains("mkfs")).unwrap();
        assert!(size < mkfs);
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
        assert!(
            !lines.iter().any(|l| l.starts_with("create-image ")),
            "{lines:?}"
        );

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

    // U-SET-27: what is mounted at the store's place is the store only if it is the store's
    // image. Anything else - a tmpfs somebody mounted there, another disk, a loop device of
    // another file - is not laid out (directories, a marker, chown), not protected on its
    // behalf, and not unmounted.
    #[rstest]
    #[case::tmpfs("tmpfs (tmpfs)")]
    #[case::another_disk("/dev/sdb1 (ext4)")]
    #[case::another_image("a loop device backed by /srv/other.img")]
    fn a_mount_that_is_not_the_stores_image_is_left_alone(#[case] what: &str) {
        let h = FakeHost::default();
        installed(&h);
        h.foreign_mounts.borrow_mut().insert(p(ROOT), what.into());
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains(what), "{}", err.message);
        assert!(err.message.contains(IMG), "{}", err.message);
        assert_no_mutation(&h);
    }

    // U-SET-27b: ... also when it only turns out after the unit was started (something
    // is mounted over it, or the unit mounts something else): nothing is laid out inside.
    #[test]
    fn a_foreign_filesystem_after_the_start_is_not_laid_out() {
        let h = FakeHost::default();
        installed(&h);
        h.mounted.borrow_mut().clear();
        h.foreign_mounts
            .borrow_mut()
            .insert(p(ROOT), "tmpfs (tmpfs)".into());
        h.files
            .borrow_mut()
            .remove(&p("/parhel/alice/.parhel-store"));
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert_eq!(
            h.lines(),
            vec!["run systemctl enable --now parhel-alice.mount"],
            "started, found something else there, stopped there"
        );
        assert!(h.text("/parhel/alice/.parhel-store").is_none());
    }

    // U-SET-27c: a store that is mounted is not looked at for a filesystem or formatted:
    // the image is in use, whatever `blkid` makes of it.
    #[test]
    fn a_mounted_store_is_never_formatted() {
        let h = FakeHost::default();
        installed(&h);
        h.formatted.borrow_mut().clear();
        h.immutable.borrow_mut().clear();
        setup(&h, &request()).unwrap();
        assert!(
            !h.lines().iter().any(|l| l.contains("mkfs")),
            "{:?}",
            h.lines()
        );
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
        assert_only_locked(&h);
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
                "lock-store /parhel/alice/.parhel-store.lock",
                "seal-store /parhel/alice/.parhel-store.lock",
                "unlock-store /parhel/alice/.parhel-store.lock",
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

    /// The unit was deleted by hand and the store is stopped: an image nobody mounts.
    fn image_without_unit(h: &FakeHost) {
        installed(h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
    }

    const PROBE: &str = "/run/parhel-probe-1";

    fn worktree_inside(h: &FakeHost) {
        for dir in [
            "/parhel/alice/worktrees/abc",
            "/parhel/alice/worktrees/abc/feat",
        ] {
            h.dirs.borrow_mut().insert(p(dir));
        }
    }

    // U-SET-36: a mount unit that is gone does not make an image that is there "nothing":
    // the image is mounted read-only on a private directory, looked into (the marker, the
    // worktrees, what else is there), unmounted, and only then deleted.
    #[test]
    fn remove_looks_inside_an_image_whose_unit_is_gone() {
        let h = FakeHost::default();
        image_without_unit(&h);
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
        assert_eq!(
            h.lines(),
            vec![
                format!("mount-ro {IMG}"),
                format!("umount {PROBE}"),
                "run chattr -i /var/lib/parhel".to_owned(),
                format!("rm {IMG}"),
                format!("rmdir {ROOT}"),
            ]
        );
        assert!(!h.exists(Path::new(IMG)) && !h.exists(Path::new(ROOT)));
    }

    // U-SET-36b: ... and a worktree in it is the user's work: `store-in-use`, naming the
    // worktree where it will appear once the store is mounted (not the private directory),
    // the image intact and nothing left mounted.
    #[test]
    fn remove_refuses_worktrees_found_in_an_image_whose_unit_is_gone() {
        let h = FakeHost::default();
        image_without_unit(&h);
        worktree_inside(&h);
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(
            err.message.contains("/parhel/alice/worktrees/abc/feat"),
            "{}",
            err.message
        );
        assert!(!err.message.contains("parhel-probe"), "{}", err.message);
        assert_eq!(
            h.lines(),
            vec![format!("mount-ro {IMG}"), format!("umount {PROBE}")]
        );
        assert!(h.exists(Path::new(IMG)));
        assert!(h.probes.borrow().is_empty(), "nothing left mounted");
    }

    // U-SET-36c: an image with no filesystem at all (what an interrupted setup leaves)
    // has nothing inside: it is deleted without mounting anything.
    #[test]
    fn remove_deletes_a_blank_image_without_mounting_it() {
        let h = FakeHost::default();
        image_without_unit(&h);
        h.formatted.borrow_mut().clear();
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines(),
            vec![
                "run chattr -i /var/lib/parhel".to_owned(),
                format!("rm {IMG}"),
                format!("rmdir {ROOT}"),
            ]
        );
    }

    /// What a refusal says: nothing was deleted, how to look yourself, how to insist.
    fn assert_refusal_explains(err: &Error) {
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        for needle in [
            "nothing was deleted",
            "mount -t btrfs -o loop,ro,nodev,nosuid,noexec",
            IMG,
            "--discard-contents",
            "--user alice",
        ] {
            assert!(
                err.message.contains(needle),
                "lacks `{needle}`: {}",
                err.message
            );
        }
    }

    // U-SET-36d: an image that cannot be looked into is not deleted on a guess: it does not
    // mount (damaged), holds another filesystem (somebody else's data), or its marker is
    // missing next to other files or says it is not this user's. The refusal says that
    // nothing was deleted and gives the commands to look and to insist; with
    // `--discard-contents` the removal goes on.
    #[rstest]
    #[case::does_not_mount(
        FakeHost::default().failing("mount", ErrorCode::Io),
        |_: &FakeHost| {}
    )]
    #[case::another_filesystem(
        FakeHost::default(),
        |h: &FakeHost| { h.foreign.borrow_mut().insert(p(IMG), "ext4".into()); }
    )]
    #[case::marker_of_another_user(
        FakeHost::default(),
        |h: &FakeHost| {
            h.files.borrow_mut().insert(
                p("/parhel/alice/.parhel-store"),
                "format=1\nuser=bob\nimage=/var/lib/parhel/bob.img\nunit=parhel-bob.mount\n".into(),
            );
        }
    )]
    #[case::marker_missing_next_to_other_files(
        FakeHost::default(),
        |h: &FakeHost| { h.files.borrow_mut().remove(&p("/parhel/alice/.parhel-store")); }
    )]
    #[case::marker_not_ours(
        FakeHost::default(),
        |h: &FakeHost| { h.files.borrow_mut().insert(p("/parhel/alice/.parhel-store"), "garbage".into()); }
    )]
    fn remove_refuses_an_image_it_cannot_vouch_for(
        #[case] h: FakeHost,
        #[case] spoil: fn(&FakeHost),
    ) {
        image_without_unit(&h);
        spoil(&h);
        let err = setup(&h, &removal()).unwrap_err();
        assert_refusal_explains(&err);
        assert!(h.exists(Path::new(IMG)), "the image is intact");
        assert!(h.probes.borrow().is_empty(), "nothing left mounted");
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.starts_with("rm") || l.contains("chattr")),
            "{:?}",
            h.lines()
        );

        // Insisting.
        let insist = SetupRequest {
            discard_contents: true,
            ..removal()
        };
        assert!(matches!(
            setup(&h, &insist).unwrap(),
            SetupOutcome::Removed(_)
        ));
        assert!(!h.exists(Path::new(IMG)));
    }

    // U-SET-36e: no marker and nothing else in the filesystem is an interrupted setup, not
    // somebody's data: removed without a flag.
    #[test]
    fn remove_deletes_a_formatted_image_that_holds_nothing() {
        let h = FakeHost::default();
        image_without_unit(&h);
        for gone in ["/parhel/alice/baselines", "/parhel/alice/worktrees"] {
            h.dirs.borrow_mut().remove(&p(gone));
        }
        for gone in [".parhel-store", ".parhel-store.lock"] {
            h.files
                .borrow_mut()
                .remove(&p(&format!("/parhel/alice/{gone}")));
        }
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
        assert!(!h.exists(Path::new(IMG)));
    }

    // U-SET-36f: what is in the store root that is not the store's - a file the user kept
    // there, a directory - is theirs: refused, listing it, wherever the store is looked at
    // (mounted, started to look, mounted read-only), until `--discard-contents`.
    #[rstest]
    #[case::mounted(|_: &FakeHost| {})]
    #[case::stopped_with_its_unit(|h: &FakeHost| { h.mounted.borrow_mut().clear(); })]
    #[case::unit_gone(|h: &FakeHost| {
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
    })]
    fn remove_refuses_files_that_are_not_the_stores(#[case] arrange: fn(&FakeHost)) {
        let h = FakeHost::default();
        installed(&h);
        arrange(&h);
        h.files
            .borrow_mut()
            .insert(p("/parhel/alice/thesis.tex"), "years of work".into());
        h.dirs.borrow_mut().insert(p("/parhel/alice/photos"));
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(err.message.contains("thesis.tex"), "{}", err.message);
        assert!(err.message.contains("photos"), "{}", err.message);
        assert!(
            err.message.contains("--discard-contents"),
            "{}",
            err.message
        );
        assert!(h.exists(Path::new(IMG)));
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.starts_with("rm") || l.contains("chattr")),
            "{:?}",
            h.lines()
        );
        assert!(h.probes.borrow().is_empty());

        let insist = SetupRequest {
            discard_contents: true,
            ..removal()
        };
        setup(&h, &insist).unwrap();
        assert!(!h.exists(Path::new(IMG)));
    }

    // U-SET-36g: worktrees are never discarded by the flag: it covers what could not be
    // checked or is not the store's, not work in progress.
    #[test]
    fn discard_contents_does_not_discard_worktrees() {
        let h = FakeHost::default();
        image_without_unit(&h);
        worktree_inside(&h);
        let insist = SetupRequest {
            discard_contents: true,
            ..removal()
        };
        let err = setup(&h, &insist).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(h.exists(Path::new(IMG)));
    }

    // U-SET-36h: the registry of symlinks goes with the store and cannot be rebuilt: the
    // links it names are printed before anything is deleted, so they can be removed (or
    // kept in mind) afterwards.
    #[rstest]
    #[case::mounted(|_: &FakeHost| {})]
    #[case::unit_gone(|h: &FakeHost| {
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        h.mounted.borrow_mut().clear();
    })]
    fn remove_lists_the_symlinks_it_is_about_to_lose(#[case] arrange: fn(&FakeHost)) {
        let h = FakeHost::default();
        installed(&h);
        arrange(&h);
        h.files.borrow_mut().insert(
            p("/parhel/alice/links"),
            "/home/alice/wt-one\t/parhel/alice/worktrees/abc/one\n\
             /home/alice/wt-two\t/parhel/alice/worktrees/abc/two\n\
             /home/alice/wt-one\t/parhel/alice/worktrees/abc/one\n"
                .into(),
        );
        setup(&h, &removal()).unwrap();
        let lines = h.lines();
        let notice = lines
            .iter()
            .position(|l| l.starts_with("notice "))
            .expect("the links are listed");
        let first_deletion = lines
            .iter()
            .position(|l| l.starts_with("rm ") || l.contains("disable") || l.contains("chattr"))
            .unwrap();
        assert!(notice < first_deletion, "{lines:#?}");
        let said = &lines[notice];
        assert!(said.contains("/home/alice/wt-one"), "{said}");
        assert!(said.contains("/home/alice/wt-two"), "{said}");
        assert_eq!(
            said.matches("/home/alice/wt-one").count(),
            1,
            "once: {said}"
        );
        assert!(said.contains("dangl"), "{said}");
    }

    // U-SET-36i: a store with no registered links says nothing of the kind.
    #[test]
    fn remove_without_links_prints_nothing() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        assert!(!h.lines().iter().any(|l| l.starts_with("notice ")));
    }

    // U-SET-36j: the unit is there and the image is not mounted, but it does not start: the
    // worktrees cannot be looked for. That is a refusal with the way to look yourself and the
    // way to insist - not a guess.
    #[test]
    fn remove_refuses_a_unit_that_does_not_start() {
        let h = FakeHost::default().failing("systemctl", ErrorCode::Io);
        installed(&h);
        h.mounted.borrow_mut().clear();
        let err = setup(&h, &removal()).unwrap_err();
        assert_refusal_explains(&err);
        assert!(h.exists(Path::new(IMG)));
        assert!(h.exists(Path::new(UNIT_FILE)));
    }

    // U-SET-38b: the store was started to look inside and looking failed (a directory that
    // cannot be read): the unit is stopped again, as it was found, and the error is shown.
    #[test]
    fn remove_stops_a_unit_it_started_when_looking_fails() {
        let h = FakeHost::default();
        installed(&h);
        h.mounted.borrow_mut().clear();
        h.unreadable
            .borrow_mut()
            .insert(p("/parhel/alice/worktrees"));
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert_eq!(
            h.lines(),
            vec![
                "run systemctl start parhel-alice.mount".to_owned(),
                lock_line(),
                unlock_line(),
                "run systemctl stop parhel-alice.mount".to_owned()
            ]
        );
        assert!(h.exists(Path::new(IMG)));
        assert!(!h.is_mount_point(Path::new(ROOT)), "left as it was found");
    }

    // ------------------------------------------------- the store-wide lock

    const LOCK: &str = "/parhel/alice/.parhel-store.lock";

    fn lock_line() -> String {
        format!("lock-store {LOCK}")
    }

    fn unlock_line() -> String {
        format!("unlock-store {LOCK}")
    }

    /// The only thing done is taking the lock and letting go of it.
    fn assert_only_locked(h: &FakeHost) {
        assert_eq!(h.lines(), vec![lock_line(), unlock_line()]);
    }

    fn sealed(h: &FakeHost) -> bool {
        h.text(LOCK).is_some_and(|text| text == REMOVING_MARK)
    }

    // U-SET-41: a run in the store - an `add`, a `doctor --fix` - holds the store's lock
    // shared, so `--remove` takes it exclusively, and without waiting, BEFORE it looks at
    // what is inside: it is asked for first in a store that is mounted, and right after
    // the unit was started to look in one that is not. (Looking fails here, and the lock is
    // already behind it.)
    #[rstest]
    #[case::mounted(false, vec![lock_line(), unlock_line()])]
    #[case::started_to_look(true, vec![
        "run systemctl start parhel-alice.mount".to_owned(),
        lock_line(),
        unlock_line(),
        "run systemctl stop parhel-alice.mount".to_owned(),
    ])]
    fn remove_takes_the_store_lock_before_it_looks_inside(
        #[case] stopped: bool,
        #[case] want: Vec<String>,
    ) {
        let h = FakeHost::default();
        installed(&h);
        if stopped {
            h.mounted.borrow_mut().clear();
        }
        h.unreadable
            .borrow_mut()
            .insert(p("/parhel/alice/worktrees"));
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "looking failed: {err}");
        assert_eq!(h.lines(), want);
        assert!(h.exists(Path::new(IMG)));
    }

    // U-SET-42: somebody holds the lock - a run is in progress - and the removal does not
    // wait and does not guess: `store-in-use`, saying a run is in progress, nothing looked
    // at (worktrees are there, and not named), nothing deleted. The flag that covers files
    // that are not the store's does not cover this. A unit started to look is stopped again,
    // after the lock attempt.
    #[rstest]
    #[case::mounted(false, false, vec![lock_line()])]
    #[case::discarding(false, true, vec![lock_line()])]
    #[case::started_to_look(true, false, vec![
        "run systemctl start parhel-alice.mount".to_owned(),
        lock_line(),
        "run systemctl stop parhel-alice.mount".to_owned(),
    ])]
    fn remove_refuses_while_a_run_holds_the_store_lock(
        #[case] stopped: bool,
        #[case] discard: bool,
        #[case] want: Vec<String>,
    ) {
        let h = FakeHost::default();
        installed(&h);
        if stopped {
            h.mounted.borrow_mut().clear();
        }
        worktree_inside(&h);
        h.store_busy.set(true);
        let request = SetupRequest {
            discard_contents: discard,
            ..removal()
        };
        let err = setup(&h, &request).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(err.message.contains("in progress"), "{}", err.message);
        assert!(err.message.contains("add"), "{}", err.message);
        assert!(!err.message.contains("worktrees/abc"), "{}", err.message);
        assert!(
            err.message.contains("nothing was looked at or deleted"),
            "{}",
            err.message
        );
        assert_eq!(h.lines(), want);
        assert!(h.exists(Path::new(IMG)) && h.exists(Path::new(UNIT_FILE)));
    }

    // U-SET-43: what the removal does with the lock. It is held through the look; then the
    // store is sealed - the lock file says it is going away, which an `add` that takes the
    // lock afterwards reads and does not start - and the lock is let go of BEFORE the unit
    // is stopped, because an open file keeps a mount busy and `umount` would refuse.
    #[test]
    fn remove_seals_the_store_and_lets_go_of_the_lock_before_it_unmounts() {
        let h = FakeHost::default();
        installed(&h);
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines(),
            vec![
                lock_line(),
                format!("seal-store {LOCK}"),
                unlock_line(),
                "run systemctl disable --now parhel-alice.mount".to_owned(),
                format!("rm {UNIT_FILE}"),
                "run systemctl daemon-reload".to_owned(),
                "run chattr -i /var/lib/parhel".to_owned(),
                format!("rm {IMG}"),
                format!("rmdir {ROOT}"),
            ]
        );
        // The same for a mount no unit owns.
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(UNIT_FILE));
        setup(&h, &removal()).unwrap();
        assert_eq!(
            h.lines()[..4],
            [
                lock_line(),
                format!("seal-store {LOCK}"),
                unlock_line(),
                "umount /parhel/alice".to_owned(),
            ]
        );
    }

    // U-SET-44: a removal that refuses leaves the store as it found it: the lock taken and
    // let go of, nothing sealed - the store keeps working.
    #[rstest]
    #[case::worktrees(|h: &FakeHost| worktree_inside(h))]
    #[case::other_files(|h: &FakeHost| { h.files.borrow_mut().insert(p("/parhel/alice/thesis.tex"), "x".into()); })]
    fn a_refusal_does_not_seal_the_store(#[case] arrange: fn(&FakeHost)) {
        let h = FakeHost::default();
        installed(&h);
        arrange(&h);
        setup(&h, &removal()).unwrap_err();
        assert_only_locked(&h);
        assert!(!sealed(&h));
    }

    // U-SET-45: a removal that fails after it sealed the store opens it again: the lock is
    // taken once more to take the mark out, and the error shown is the real one. When the
    // lock cannot be had (a run got in meanwhile) the store stays closed to `add`, and the
    // error says how to bring it back.
    #[test]
    fn a_removal_that_fails_after_sealing_reopens_the_store() {
        let h = FakeHost::default().failing("systemctl", ErrorCode::Io);
        installed(&h);
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert!(err.message.contains("systemctl"), "{}", err.message);
        assert_eq!(
            h.lines(),
            vec![
                lock_line(),
                format!("seal-store {LOCK}"),
                unlock_line(),
                "run systemctl disable --now parhel-alice.mount".to_owned(),
                lock_line(),
                format!("unseal-store {LOCK}"),
                unlock_line(),
            ]
        );
        assert!(!sealed(&h));
        assert!(h.exists(Path::new(IMG)));

        let h = FakeHost::default().failing("systemctl", ErrorCode::Io);
        installed(&h);
        h.busy_once_sealed.set(true);
        let err = setup(&h, &removal()).unwrap_err();
        assert!(sealed(&h));
        assert!(
            err.message.contains("sudo git parhel setup"),
            "{}",
            err.message
        );
    }

    // U-SET-45b: ... but not when the store is gone by then (the unit was stopped and a later
    // step failed): there is no store to open, and nothing is created where it was mounted.
    #[test]
    fn a_removal_that_fails_after_the_unmount_does_not_touch_the_lock_file() {
        let h = FakeHost::default().failing("chattr", ErrorCode::Io);
        installed(&h);
        setup(&h, &removal()).unwrap_err();
        let lines = h.lines();
        assert_eq!(
            lines.iter().filter(|l| **l == lock_line()).count(),
            1,
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("unseal-store")),
            "{lines:#?}"
        );
    }

    // U-SET-45c: a lock file that cannot be written leaves the store as it was: nothing is
    // stopped or deleted.
    #[test]
    fn a_store_that_cannot_be_sealed_is_not_removed() {
        let h = FakeHost::default();
        installed(&h);
        h.fail_seal.set(true);
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io, "{err}");
        assert!(
            !h.lines()
                .iter()
                .any(|l| l.contains("disable") || l.starts_with("rm") || l.starts_with("umount")),
            "{:?}",
            h.lines()
        );
        assert!(h.exists(Path::new(IMG)));
    }

    // U-SET-46: an image whose unit is gone is not mounted at the store's place: it is
    // mounted read-only on a private directory of root's, which no user can enter, and `add`
    // finds a store by its marker at `/parhel/<user>`, never there. No run can be using it, so
    // there is nothing to lock - and no lock file is made on a mount point that is not mounted.
    #[test]
    fn looking_into_an_unmounted_image_takes_no_lock() {
        for with_worktree in [false, true] {
            let h = FakeHost::default();
            image_without_unit(&h);
            // (What is in the image is not the lock the test is about: a lock file made
            // there would show.)
            h.files.borrow_mut().remove(&p(LOCK));
            if with_worktree {
                worktree_inside(&h);
            }
            let _ = setup(&h, &removal());
            assert!(
                !h.lines().iter().any(|l| l.contains("lock-store")),
                "{:?}",
                h.lines()
            );
            assert!(h.text(LOCK).is_none());
        }
    }

    // U-SET-47: the lock file is the store's own: not a foreign file that blocks the removal.
    #[test]
    fn the_lock_file_is_not_a_foreign_file() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(p(LOCK), String::new());
        assert!(matches!(
            setup(&h, &removal()).unwrap(),
            SetupOutcome::Removed(_)
        ));
    }

    // U-SET-48: `setup` makes the lock file, the user's and private, in a store that has
    // none (a new one, or one set up before the lock existed) - without touching a lock file
    // that is there: replacing it would put a run that holds the old one outside the lock.
    #[test]
    fn setup_makes_the_lock_file_and_never_replaces_one() {
        let h = FakeHost::default();
        setup(&h, &request()).unwrap();
        let made = h
            .calls()
            .into_iter()
            .find(|c| matches!(c, HostCall::LockStore { .. }))
            .expect("the lock file is made");
        assert_eq!(
            made,
            HostCall::LockStore {
                path: p(LOCK),
                owner: Some((1000, 1000))
            }
        );
        assert_eq!(h.text(LOCK).as_deref(), Some(""));
        assert!(
            !h.lines().contains(&format!("write {LOCK}")),
            "{:?}",
            h.lines()
        );

        // Older store: made, and said.
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().remove(&p(LOCK));
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Restored(_)
        ));
        assert_eq!(h.lines(), vec![lock_line(), unlock_line()]);

        // A store that has it: nothing is done.
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(p(LOCK), String::new());
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::AlreadySetUp(_)
        ));
        assert_no_mutation(&h);
    }

    // U-SET-49: a store whose removal died after sealing it (killed, power lost) is closed to
    // `add` for good, until `setup` runs: it takes the mark out and says the store was
    // restored. If a run holds the lock at that moment, it says so and leaves the mark.
    #[test]
    fn setup_reopens_a_store_whose_removal_died_after_sealing() {
        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(p(LOCK), REMOVING_MARK.into());
        assert!(matches!(
            setup(&h, &request()).unwrap(),
            SetupOutcome::Restored(_)
        ));
        assert_eq!(
            h.lines(),
            vec![lock_line(), format!("unseal-store {LOCK}"), unlock_line()]
        );
        assert!(!sealed(&h));

        let h = FakeHost::default();
        installed(&h);
        h.files.borrow_mut().insert(p(LOCK), REMOVING_MARK.into());
        h.store_busy.set(true);
        let err = setup(&h, &request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreInUse, "{err}");
        assert!(sealed(&h));
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
                "run systemctl start parhel-alice.mount".to_owned(),
                lock_line(),
                unlock_line(),
                "run systemctl stop parhel-alice.mount".to_owned()
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
            h.lines().get(3).map(String::as_str),
            Some("umount /parhel/alice"),
            "after the lock was let go of: {:?}",
            h.lines()
        );
        assert!(!h.exists(Path::new(IMG)) && !h.exists(Path::new(ROOT)));
    }

    // U-SET-28: removal unmounts the store only if it is the store: a foreign mount at the
    // place is refused before the unit is touched, the image deleted or anything unmounted.
    #[rstest]
    #[case::with_the_unit(true)]
    #[case::without_the_unit(false)]
    fn remove_leaves_a_foreign_mount_alone(#[case] unit: bool) {
        let h = FakeHost::default();
        installed(&h);
        if !unit {
            h.files.borrow_mut().remove(&p(UNIT_FILE));
        }
        h.foreign_mounts
            .borrow_mut()
            .insert(p(ROOT), "tmpfs (tmpfs)".into());
        let err = setup(&h, &removal()).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoreUnavailable, "{err}");
        assert!(err.message.contains("tmpfs"), "{}", err.message);
        assert_no_mutation(&h);
        assert!(h.exists(Path::new(IMG)));
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
