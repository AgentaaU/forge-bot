//! Repository checkout management.
//!
//! Before invoking an agent we can prepare a working copy of the repository so
//! that CLI agents start inside the code they are asked to change. Preparing a
//! workspace is optional; with `workspace.enabled = false` agents run in an
//! empty directory and are expected to clone/access the forge themselves.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustix::fs::{
    AtFlags, CWD, Mode, OFlags, RenameFlags, StatxFlags, openat, renameat_with, statx, unlinkat,
};
use url::Url;
use uuid::Uuid;

use crate::config::{WorkspaceConfig, is_valid_user_identifier};
use crate::error::{BotError, Result};
use crate::executor::{ExecSpec, Executor, Prepared};
use crate::forge::ForgeMessage;
use crate::location::ForgeKind;

const TRASH_DIR: &str = ".forge-bot-trash";
const UNVERIFIED_PREFIX: &str = "unverified-";
const VERIFIED_PREFIX: &str = "verified-";

/// Creates and updates per-issue checkouts.
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    enabled: bool,
    reuse: bool,
    root: PathBuf,
    git_author_name: String,
    git_author_email: String,
    /// Launches `git` through the same account boundary as the agents.
    executor: Arc<Executor>,
}

impl WorkspaceManager {
    pub fn new(config: &WorkspaceConfig, executor: Arc<Executor>) -> Self {
        Self {
            enabled: config.enabled,
            reuse: config.reuse,
            root: crate::config::expand_tilde(&config.root),
            git_author_name: config.git_author_name.clone(),
            git_author_email: config.git_author_email.clone(),
            executor,
        }
    }

    /// Path used for a message, whether or not it exists yet.
    ///
    /// This is the legacy/test path, rooted at `[workspace].root`. Explicit
    /// `[users.*]` runs use [`Self::dir_for`], which roots the checkout at the
    /// account's home instead.
    pub fn path_for(&self, message: &ForgeMessage, user_id: Option<&str>) -> PathBuf {
        let name = workspace_dir_name(message);
        match user_id {
            Some(user) => self.root.join(user).join(name),
            None => self.root.join(name),
        }
    }

    /// Checkout directory for one run.
    ///
    /// An explicit `[users.*]` run gets its checkout directly in the
    /// `host_user`'s home. That is the only directory the dropped-privilege
    /// process is guaranteed to own end to end, so no root-owned intermediate
    /// directory can block `git clone`.
    fn dir_for(
        &self,
        message: &ForgeMessage,
        host_user: Option<&str>,
        user_id: Option<&str>,
    ) -> PathBuf {
        match host_user.and_then(|user| self.executor.account(user)) {
            Some(account) => account.home.join(workspace_dir_name(message)),
            None => self.path_for(message, user_id),
        }
    }

    /// Checkout directory a run with this context will use. Follow-up steering
    /// compares it with the live context so a linked PR cannot inherit another
    /// repository's working copy.
    pub(crate) fn path_for_run(
        &self,
        message: &ForgeMessage,
        host_user: Option<&str>,
        user_id: Option<&str>,
    ) -> PathBuf {
        self.dir_for(message, host_user, user_id)
    }

    /// Refresh the idle timer before a job stops protecting this checkout.
    pub(crate) fn record_use(&self, path: &Path) {
        let touch = || -> io::Result<()> {
            let mut directory = open_directory(&self.root)?;
            // Resolve aliases through opened directories, then verify that the
            // checkout reached from the cleanup root is the same inode.
            let checkout = File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(path)?;
            let Some(relative) = checkout_path_within_root(&directory, &checkout)? else {
                return Ok(());
            };
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    return Ok(());
                };
                directory = open_child_directory(&directory, name)?;
            }
            if directory_identity(&directory.metadata()?)
                == directory_identity(&checkout.metadata()?)
            {
                directory.set_modified(SystemTime::now())?;
            }
            Ok(())
        };
        if let Err(error) = touch()
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %path.display(), %error, "failed to record workspace activity");
        }
    }

    /// Detach expired checkouts, then hand the trash back to the caller to
    /// purge without holding up job admission.
    pub(crate) fn retire_stale(
        &self,
        retention: Duration,
        protected: &HashSet<PathBuf>,
    ) -> io::Result<Option<WorkspaceTrash>> {
        if retention.is_zero() {
            return Ok(None);
        }
        let root = match open_directory(&self.root) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if has_git_marker(&root)? {
            return Ok(None);
        }
        let now = SystemTime::now();
        let mut protected_ids = HashSet::new();
        for path in protected {
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => {
                    protected_ids.insert(directory_identity(&metadata));
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        let mounts = read_mounts()?;
        let trash = match WorkspaceTrash::open(&root) {
            Ok(trash) => Some(trash),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut sweep = WorkspaceSweep {
            now,
            retention,
            protected,
            protected_ids,
            mounts,
            root: &root,
            trash,
        };
        sweep.namespace(&root, &self.root, true)?;
        Ok(sweep.trash)
    }

    /// Ensure a checkout exists for `message` and return its path.
    pub async fn prepare(
        &self,
        message: &ForgeMessage,
        credentials: &[(String, String)],
        host_user: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<PathBuf> {
        let dir = self.dir_for(message, host_user, user_id);
        self.executor.create_dir_all(&dir, host_user).await?;

        if !self.enabled {
            return Ok(dir);
        }

        let clone_url = clean_clone_url(message);

        if !dir.join(".git").exists() {
            tracing::info!(dir = %dir.display(), repo = %message.repository, "cloning repository");
            run_git_with_credentials(
                &self.executor,
                host_user,
                dir.parent().unwrap_or(Path::new(".")),
                &["clone", &clone_url, dir.to_string_lossy().as_ref()],
                credentials,
            )
            .await
            .map_err(|e| BotError::Agent {
                name: "workspace".into(),
                reason: format!("git clone failed: {e}"),
            })?;
        } else if self.reuse {
            run_git_with_credentials(
                &self.executor,
                host_user,
                &dir,
                &["remote", "set-url", "origin", &clone_url],
                &[],
            )
            .await?;
            tracing::info!(dir = %dir.display(), "updating existing workspace");
            // Best effort: a dirty workspace or a detached HEAD must not stop
            // the job.
            if let Err(e) = run_git_with_credentials(
                &self.executor,
                host_user,
                &dir,
                &["fetch", "--all", "--prune"],
                credentials,
            )
            .await
            {
                tracing::warn!(error = %e, "git fetch failed");
            }
        }

        // Make sure the agent can create commits.
        let _ = run_git_with_credentials(
            &self.executor,
            host_user,
            &dir,
            &["config", "user.name", &self.git_author_name],
            &[],
        )
        .await;
        let _ = run_git_with_credentials(
            &self.executor,
            host_user,
            &dir,
            &["config", "user.email", &self.git_author_email],
            &[],
        )
        .await;
        // Origin always contains a clean URL, including on a failed clone.
        let clean_url = clean_clone_url(message);
        let _ = run_git_with_credentials(
            &self.executor,
            host_user,
            &dir,
            &["remote", "set-url", "origin", &clean_url],
            &[],
        )
        .await;

        Ok(dir)
    }
}

struct WorkspaceSweep<'a> {
    now: SystemTime,
    retention: Duration,
    protected: &'a HashSet<PathBuf>,
    protected_ids: HashSet<(u64, u64)>,
    mounts: Vec<Mount>,
    root: &'a File,
    trash: Option<WorkspaceTrash>,
}

impl WorkspaceSweep<'_> {
    fn namespace(&mut self, dir: &File, logical: &Path, root: bool) -> io::Result<()> {
        visit_directories(dir, logical, |name, path, logical| {
            let checkout = name.rsplit_once('-').is_some_and(|(repo, suffix)| {
                repo.contains("__") && sanitize(repo) == repo && is_checkout_suffix(suffix)
            });
            let user_namespace = root && is_valid_user_identifier(name);
            let gitlab_namespace = name == "gitlab";
            if !checkout && !user_namespace && !gitlab_namespace {
                return Ok(());
            }
            let child = open_child_directory(dir, OsStr::new(name))?;
            if has_git_marker(&child)? {
                if checkout {
                    self.checkout(&child, path, logical)?;
                }
                return Ok(());
            }
            if gitlab_namespace {
                self.gitlab_projects(&child, logical)?;
            }
            if user_namespace {
                self.namespace(&child, logical, false)?;
            }
            Ok(())
        })
    }

    fn gitlab_projects(&mut self, dir: &File, logical: &Path) -> io::Result<()> {
        visit_directories(dir, logical, |name, _, logical| {
            if name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                let project = open_child_directory(dir, OsStr::new(name))?;
                if has_git_marker(&project)? {
                    return Ok(());
                }
                visit_directories(&project, logical, |name, path, logical| {
                    if name.rsplit_once('-').is_some_and(|(kind, suffix)| {
                        matches!(kind, "issue" | "mr") && is_checkout_suffix(suffix)
                    }) {
                        let checkout = open_child_directory(&project, OsStr::new(name))?;
                        self.checkout(&checkout, path, logical)?;
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })
    }

    fn checkout(&mut self, dir: &File, path: &Path, logical: &Path) -> io::Result<()> {
        let metadata = dir.metadata()?;
        if self.protected.contains(logical)
            || self.protected_ids.contains(&directory_identity(&metadata))
        {
            return Ok(());
        }
        let git = match open_child_directory(dir, OsStr::new(".git")) {
            Ok(git) => git,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if !fs::symlink_metadata(directory_path(&git).join("HEAD"))?.is_file() {
            return Ok(());
        }
        // Older checkouts have no explicit activity stamp. Git updates the
        // metadata directory on fetch, checkout and config changes.
        let modified = metadata.modified()?.max(git.metadata()?.modified()?);
        if self
            .now
            .duration_since(modified)
            .is_ok_and(|age| age >= self.retention)
            && !has_registered_worktrees(&git)?
            && !has_mount_at_or_below(dir, &self.mounts)?
        {
            let trash = match &self.trash {
                Some(trash) => trash,
                None => self.trash.insert(WorkspaceTrash::get_or_create(self.root)?),
            };
            trash.retire(path, dir)?;
            tracing::info!(dir = %logical.display(), "retired stale workspace");
        }
        Ok(())
    }
}

// A linked worktree stores a .git file that points to metadata elsewhere.
// Both forms mark a checkout boundary, even when the checkout is unrelated.
fn has_git_marker(dir: &File) -> io::Result<bool> {
    match fs::symlink_metadata(directory_path(dir).join(".git")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn has_registered_worktrees(git: &File) -> io::Result<bool> {
    let worktrees = match open_child_directory(git, OsStr::new("worktrees")) {
        Ok(worktrees) => worktrees,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(fs::read_dir(directory_path(&worktrees))?.next().is_some())
}

pub(crate) struct WorkspaceTrash(File);

impl WorkspaceTrash {
    fn open(root: &File) -> io::Result<Self> {
        let dir = open_child_directory(root, OsStr::new(TRASH_DIR))?;
        let metadata = dir.metadata()?;
        // SAFETY: geteuid has no pointer arguments or side effects.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace trash must be private to the gateway account",
            ));
        }
        Ok(Self(dir))
    }

    fn get_or_create(root: &File) -> io::Result<Self> {
        if let Err(error) = fs::DirBuilder::new()
            .mode(0o700)
            .create(directory_path(root).join(TRASH_DIR))
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error);
        }
        Self::open(root)
    }

    fn stage(&self, source: &Path, id: Uuid) -> io::Result<String> {
        let name = format!("{UNVERIFIED_PREFIX}{id}");
        renameat_with(CWD, source, &self.0, name.as_str(), RenameFlags::NOREPLACE)?;
        Ok(name)
    }

    fn retire(&self, source: &Path, inspected: &File) -> io::Result<()> {
        let id = Uuid::new_v4();
        let staged_name = self.stage(source, id)?;
        // The source name may have been replaced after inspection. The
        // private trash name becomes eligible for purge only after we verify
        // the move against the still-open directory.
        let verify = || -> io::Result<()> {
            let staged = open_child_directory(&self.0, OsStr::new(&staged_name))?;
            if directory_identity(&staged.metadata()?) != directory_identity(&inspected.metadata()?)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "staged checkout differs from inspected directory",
                ));
            }
            renameat_with(
                &self.0,
                staged_name.as_str(),
                &self.0,
                format!("{VERIFIED_PREFIX}{id}").as_str(),
                RenameFlags::NOREPLACE,
            )?;
            Ok(())
        };
        verify().map_err(|error| {
            let path = Path::new(TRASH_DIR).join(&staged_name);
            io::Error::other(format!(
                "preserved {} under the workspace root after verification failed: {error}",
                path.display()
            ))
        })
    }

    /// Retry interrupted deletions, including leftovers from a previous run.
    pub(crate) fn purge(self) -> io::Result<()> {
        let mounts = read_mounts()?;
        visit_directories(&self.0, Path::new(TRASH_DIR), |name, _, _| {
            if name
                .strip_prefix(VERIFIED_PREFIX)
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
            {
                let name = OsStr::new(name);
                let checkout = open_child_directory(&self.0, name)?;
                if !has_mount_at_or_below(&checkout, &mounts)? {
                    remove_directory_contents(&checkout)?;
                    unlinkat(&self.0, name, AtFlags::REMOVEDIR)?;
                }
            }
            Ok(())
        })
    }
}

fn remove_directory_contents(dir: &File) -> io::Result<()> {
    for entry in fs::read_dir(directory_path(dir))? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            // Enforce the boundary on every open in case a mount appears
            // after the mount table was read. This also covers bind mounts.
            let child = open_child_directory(dir, &name)?;
            remove_directory_contents(&child)?;
            unlinkat(dir, &name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(dir, &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

fn has_mount_at_or_below(dir: &File, mounts: &[Mount]) -> io::Result<bool> {
    let path = fs::read_link(directory_path(dir))?;
    Ok(mounts.iter().any(|mount| mount.target.starts_with(&path)))
}

struct Mount {
    id: u64,
    root: PathBuf,
    target: PathBuf,
}

fn read_mounts() -> io::Result<Vec<Mount>> {
    // mountinfo lists bind mounts separately even when st_dev is unchanged.
    let data = fs::read("/proc/self/mountinfo")?;
    data.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split(|byte| *byte == b' ');
            let (Some(id), Some(root), Some(target)) =
                (fields.next(), fields.nth(2), fields.next())
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "incomplete mount entry",
                ));
            };
            let id = std::str::from_utf8(id)
                .ok()
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid mount ID"))?;
            Ok(Mount {
                id,
                root: decode_mount_path(root)?,
                target: decode_mount_path(target)?,
            })
        })
        .collect()
}

fn decode_mount_path(field: &[u8]) -> io::Result<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'\\' {
            let decoded_byte = match tail.get(..3) {
                Some(b"040") => b' ',
                Some(b"011") => b'\t',
                Some(b"012") => b'\n',
                Some(b"134") => b'\\',
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid mount path escape",
                    ));
                }
            };
            decoded.push(decoded_byte);
            rest = &tail[3..];
        } else {
            decoded.push(byte);
            rest = tail;
        }
    }
    Ok(OsString::from_vec(decoded).into())
}

fn checkout_path_within_root(root: &File, checkout: &File) -> io::Result<Option<PathBuf>> {
    let root_path = fs::read_link(directory_path(root))?;
    let checkout_path = fs::read_link(directory_path(checkout))?;
    if let Ok(relative) = checkout_path.strip_prefix(&root_path) {
        return Ok(Some(relative.to_owned()));
    }

    let root_mount_id = mount_id(root)?;
    let checkout_mount_id = mount_id(checkout)?;
    if root_mount_id == checkout_mount_id || root.metadata()?.dev() != checkout.metadata()?.dev() {
        return Ok(None);
    }

    // For bind aliases, compare paths relative to their filesystem root.
    let mounts = read_mounts()?;
    let filesystem_path = |id, path: &Path| {
        let mount = mounts.iter().find(|mount| mount.id == id)?;
        Some(mount.root.join(path.strip_prefix(&mount.target).ok()?))
    };
    let (Some(root), Some(checkout)) = (
        filesystem_path(root_mount_id, &root_path),
        filesystem_path(checkout_mount_id, &checkout_path),
    ) else {
        return Ok(None);
    };
    Ok(checkout.strip_prefix(root).ok().map(Path::to_owned))
}

fn directory_identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn is_checkout_suffix(suffix: &str) -> bool {
    suffix == "latest" || (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
}

fn open_child_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let child = File::from(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?);
    // Compare the opened mount, since a bind mount has the same device and
    // inode as its source. Keep the descriptor pinned for all later lookups.
    if mount_id(parent)? != mount_id(&child)? {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(child)
}

fn mount_id(dir: &File) -> io::Result<u64> {
    if let Ok(info) = statx(dir, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        && info.stx_mask & StatxFlags::MNT_ID.bits() != 0
    {
        return Ok(info.stx_mnt_id);
    }
    // Linux before 5.8 does not return mount IDs via statx.
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", dir.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing mount ID"))
}

fn open_directory(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

// Keep parent directories open during the walk. A rename or symlink swap
// cannot redirect a subsequent lookup through a different parent.
fn directory_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn visit_directories(
    dir: &File,
    logical: &Path,
    mut visit: impl FnMut(&str, &Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory_path(dir))? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir()
            && let Some(name) = name.to_str()
        {
            let logical = logical.join(name);
            if let Err(error) = visit(name, &entry.path(), &logical)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(dir = %logical.display(), %error, "failed to check stale workspace");
            }
        }
    }
    Ok(())
}

/// Name of the checkout directory for one message.
fn workspace_dir_name(message: &ForgeMessage) -> String {
    let owner = sanitize(message.owner());
    let repo = sanitize(message.repo_name());
    // Linked PRs in this repository continue in the issue's checkout. A
    // cross-repository PR still needs its own repository's code and remote.
    let local_issue = message.linked_issue.as_ref().filter(|issue| {
        message.is_pull_request
            && issue
                .repository
                .as_deref()
                .is_none_or(|repo| repo == message.repository)
    });
    let number = local_issue.map(|issue| issue.number).or(message.number);
    let suffix = number
        .map(|n| format!("-{n}"))
        .unwrap_or_else(|| "-latest".to_owned());
    if message.forge == ForgeKind::GitLab {
        use sha2::{Digest, Sha256};
        let project = hex::encode(Sha256::digest(message.repository.as_bytes()));
        let kind = if message.is_pull_request && local_issue.is_none() {
            "mr"
        } else {
            "issue"
        };
        format!("gitlab/{project}/{kind}{suffix}")
    } else {
        format!("{owner}__{repo}{suffix}")
    }
}

/// Clone URL without credentials.
fn clean_clone_url(message: &ForgeMessage) -> String {
    let mut base = message.location.clone();
    let _ = base.set_username("");
    let _ = base.set_password(None);
    base.set_path("");
    base.set_query(None);
    base.set_fragment(None);
    let base = base.to_string();
    let base = base.trim_end_matches('/');
    format!("{base}/{}/{}.git", message.owner(), message.repo_name())
}

/// Pick the best credential for a forge from the exported variables.
fn credential(credentials: &[(String, String)], forge: ForgeKind) -> Option<String> {
    let keys: &[&str] = match forge {
        ForgeKind::Forgejo | ForgeKind::Gitea => &["FORGEJO_TOKEN", "FORGE_TOKEN"],
        ForgeKind::GitHub => &["GITHUB_TOKEN", "GH_TOKEN", "FORGE_TOKEN"],
        ForgeKind::GitLab => &["GITLAB_TOKEN", "FORGE_TOKEN"],
        ForgeKind::Unknown => &["FORGE_TOKEN"],
    };
    for key in keys {
        if let Some((_, value)) = credentials.iter().find(|(k, _)| k == key)
            && !value.is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

/// Replace any character that is unsafe in a path with `_`.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Run a git command through `executor`, returning an error with stderr on
/// failure. In systemd mode the command runs under `host_user`, so repository
/// hooks and `git` never execute as the gateway account.
async fn run_git_with_credentials(
    executor: &Executor,
    host_user: Option<&str>,
    cwd: &Path,
    args: &[&str],
    credentials: &[(String, String)],
) -> Result<String> {
    let spec = git_spec(host_user, cwd, args, credentials);
    let Prepared {
        command: mut cmd,
        cgroup: _cgroup,
    } = executor
        .command(&spec)
        .map_err(|error| BotError::Other(anyhow::anyhow!(error.to_string())))?;
    let output = cmd.output().await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = format!(
            "git {} failed: {}",
            args.first().unwrap_or(&"command"),
            redact_git_error(stderr.trim(), credentials)
        );
        let lower = stderr.to_lowercase();
        if stderr.contains("403")
            || stderr.contains("401")
            || lower.contains("authentication failed")
            || lower.contains("could not read username")
            || lower.contains("permission denied")
        {
            Err(BotError::ForgePermissionDenied(message))
        } else {
            Err(BotError::Other(anyhow::anyhow!(message)))
        }
    }
}

/// Git runs the helper using its shell, with the secret read only from the
/// environment. Clearing the helper list prevents store helpers persisting it.
fn git_spec(
    host_user: Option<&str>,
    cwd: &Path,
    args: &[&str],
    credentials: &[(String, String)],
) -> ExecSpec {
    let mut spec = ExecSpec {
        program: "git".into(),
        args: vec!["-c".into(), "credential.helper=".into()],
        env: vec![
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_ASKPASS".into(), "/bin/false".into()),
        ],
        cwd: Some(cwd.to_owned()),
        host_user: host_user.map(str::to_owned),
    };
    let forge = if credentials.iter().any(|(key, _)| key == "GITHUB_TOKEN") {
        ForgeKind::GitHub
    } else if credentials.iter().any(|(key, _)| key == "GITLAB_TOKEN") {
        ForgeKind::GitLab
    } else {
        ForgeKind::Forgejo
    };
    if let Some(token) = credential(credentials, forge) {
        let (username, password) = if forge == ForgeKind::GitHub {
            ("x-access-token".to_owned(), token)
        } else {
            (token, String::new())
        };
        spec.env.push(("FORGE_BOT_GIT_USERNAME".into(), username));
        spec.env.push(("FORGE_BOT_GIT_PASSWORD".into(), password));
        spec.args.extend(["-c".into(),
            "credential.helper=!f() { if [ \"$1\" = get ]; then printf 'username=%s\\npassword=%s\\n' \"$FORGE_BOT_GIT_USERNAME\" \"$FORGE_BOT_GIT_PASSWORD\"; fi; }; f".into()]);
    }
    spec.args.extend(args.iter().map(|arg| (*arg).to_owned()));
    spec
}

fn redact_git_error(text: &str, credentials: &[(String, String)]) -> String {
    let mut redacted = text.to_owned();
    for (_, token) in credentials
        .iter()
        .filter(|(key, value)| key.ends_with("TOKEN") && !value.is_empty())
    {
        redacted = redacted.replace(token, "[REDACTED]");
        // Git/HTTP errors can contain URL-escaped credentials as well.
        let mut url = Url::parse("https://redaction.invalid").expect("constant URL");
        let _ = url.set_username(token);
        redacted = redacted.replace(url.username(), "[REDACTED]");
        let _ = url.set_password(Some(token));
        if let Some(encoded) = url.password() {
            redacted = redacted.replace(encoded, "[REDACTED]");
        }
    }
    redacted
}

#[cfg(test)]
async fn run_git(
    executor: &Executor,
    host_user: Option<&str>,
    cwd: &Path,
    args: &[&str],
) -> Result<String> {
    run_git_with_credentials(executor, host_user, cwd, args, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_manager(config: &WorkspaceConfig) -> WorkspaceManager {
        WorkspaceManager::new(config, Arc::new(Executor::direct()))
    }

    fn msg() -> ForgeMessage {
        ForgeMessage {
            forge: ForgeKind::Forgejo,
            location: Url::parse("http://forge.local:3000/Org/My-Repo/issues/3").unwrap(),
            body: "@agent x".into(),
            author: "u".into(),
            repository: "Org/My-Repo".into(),
            comment_id: None,
            review_id: None,
            number: Some(3),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
            reply_target: Default::default(),
        }
    }

    fn checkout(path: &Path, modified: SystemTime) {
        let git = path.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        set_checkout_mtime(path, modified);
    }

    fn set_checkout_mtime(path: &Path, modified: SystemTime) {
        File::open(path.join(".git"))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        File::open(path).unwrap().set_modified(modified).unwrap();
    }

    #[test]
    fn retirement_preserves_a_checkout_replaced_after_inspection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("Org__Repo-1");
        let original = root.join("original");
        let old = SystemTime::now() - Duration::from_secs(600);
        checkout(&source, old);
        let inspected = open_directory(&source).unwrap();

        fs::rename(&source, &original).unwrap();
        checkout(&source, SystemTime::now());
        fs::write(source.join("local changes"), "fresh checkout").unwrap();

        let trash = WorkspaceTrash::get_or_create(&open_directory(root).unwrap()).unwrap();
        let error = trash.retire(&source, &inspected).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs from inspected directory")
        );
        let staged = fs::read_dir(root.join(TRASH_DIR))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert!(
            staged
                .file_name()
                .to_str()
                .unwrap()
                .starts_with(UNVERIFIED_PREFIX)
        );

        trash.purge().unwrap();
        assert!(original.join(".git/HEAD").is_file());
        assert_eq!(
            fs::read_to_string(staged.path().join("local changes")).unwrap(),
            "fresh checkout"
        );
    }

    #[test]
    fn restart_purges_only_verified_trash() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let trash_path = root.join(TRASH_DIR);
        let interrupted = root.join("Org__Repo-1");
        let stale = root.join("Org__Repo-2");
        let old = SystemTime::now() - Duration::from_secs(600);
        checkout(&interrupted, old);
        checkout(&stale, old);

        let trash = WorkspaceTrash::get_or_create(&open_directory(root).unwrap()).unwrap();
        let staged_name = trash.stage(&interrupted, Uuid::new_v4()).unwrap();
        // Older UUID-only entries have no verification record either.
        let legacy = trash_path.join(Uuid::new_v4().to_string());
        checkout(&legacy, old);
        drop(trash);

        let manager = workspace_manager(&WorkspaceConfig {
            root: root.to_owned(),
            ..Default::default()
        });
        manager
            .retire_stale(Duration::from_secs(60), &HashSet::new())
            .unwrap()
            .unwrap()
            .purge()
            .unwrap();

        assert!(!stale.exists());
        assert!(trash_path.join(staged_name).join(".git/HEAD").is_file());
        assert!(legacy.join(".git/HEAD").is_file());
        assert_eq!(fs::read_dir(trash_path).unwrap().count(), 2);
    }

    #[test]
    fn retires_only_idle_checkouts_in_managed_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspaces");
        let manager = workspace_manager(&WorkspaceConfig {
            root: root.clone(),
            ..Default::default()
        });
        let recent = SystemTime::now();
        let old = recent - Duration::from_secs(600);
        let retention = Duration::from_secs(60);
        let gitlab = format!("{}/mr-7", "a".repeat(64));
        let stale = [
            root.join("Org__My-Repo-3"),
            root.join("old__user-1/Org__My-Repo-latest"),
            root.join("gitlab").join(&gitlab),
            root.join("reviewer/gitlab").join(&gitlab),
        ];
        let outside = dir.path().join("outside");
        let kept = [
            outside.join("Org__My-Repo-8"),
            root.join("reference"),
            root.join("Org__My-Repo-backup"),
            root.join("Org__My-Repo-5"),
        ];
        for path in stale.iter().chain(&kept) {
            checkout(path, old);
        }
        let nested = root.join("reference/Org__My-Repo-9");
        checkout(&nested, old);
        let nested_worktree = root.join("external-worktree/Org__My-Repo-10");
        checkout(&nested_worktree, old);
        fs::write(root.join("external-worktree/.git"), "gitdir: /elsewhere\n").unwrap();
        let fresh = root.join("Org__My-Repo-6");
        checkout(&fresh, old);
        File::open(fresh.join(".git"))
            .unwrap()
            .set_modified(recent)
            .unwrap();
        let other = root.join("Org__Other-1");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("notes"), "keep these notes").unwrap();
        let symlink = root.join("Org__My-Repo-8");
        std::os::unix::fs::symlink(&kept[0], &symlink).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked-user")).unwrap();
        std::os::unix::fs::symlink(&outside, stale[0].join("external")).unwrap();
        File::open(&stale[0]).unwrap().set_modified(old).unwrap();

        let protected = HashSet::from([kept[3].clone()]);
        let trash = manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap();
        assert!(stale.iter().all(|path| !path.exists()));
        let trash_dir = root.join(".forge-bot-trash");
        assert_eq!(fs::read_dir(&trash_dir).unwrap().count(), stale.len());
        // Simulate restarting after detaching checkouts and before purging.
        drop(trash);
        manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap()
            .purge()
            .unwrap();
        assert_eq!(fs::read_dir(&trash_dir).unwrap().count(), 0);
        assert!(kept.iter().all(|path| path.is_dir()));
        assert!(nested.join(".git/HEAD").is_file());
        assert!(nested_worktree.join(".git/HEAD").is_file());
        assert!(symlink.is_symlink());
        assert!(fresh.is_dir());
        assert_eq!(
            fs::read_to_string(other.join("notes")).unwrap(),
            "keep these notes"
        );
    }

    #[test]
    fn disabled_cleanup_and_recent_use_preserve_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspaces");
        let manager = workspace_manager(&WorkspaceConfig {
            root: root.clone(),
            ..Default::default()
        });
        let protected = HashSet::new();
        let retention = Duration::from_secs(60);
        assert!(
            manager
                .retire_stale(retention, &protected)
                .unwrap()
                .is_none()
        );
        let path = root.join("reviewer/Org__My-Repo-3");
        checkout(&path, SystemTime::now() - Duration::from_secs(600));

        assert!(
            manager
                .retire_stale(Duration::ZERO, &protected)
                .unwrap()
                .is_none()
        );
        assert!(path.is_dir());

        let aliased_home = dir.path().join("home");
        std::os::unix::fs::symlink(path.parent().unwrap(), &aliased_home).unwrap();
        manager.record_use(&aliased_home.join(path.file_name().unwrap()));
        assert!(
            manager
                .retire_stale(retention, &protected)
                .unwrap()
                .is_none()
        );
        assert!(path.is_dir());
    }

    #[tokio::test]
    async fn keeps_shared_git_metadata_while_a_linked_worktree_is_registered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspaces");
        let manager = workspace_manager(&WorkspaceConfig {
            root: root.clone(),
            ..Default::default()
        });
        let main = root.join("Org__Repo-1");
        let linked = dir.path().join("linked");
        fs::create_dir_all(&main).unwrap();
        let executor = Executor::direct();
        run_git(&executor, None, &main, &["init"]).await.unwrap();
        run_git(
            &executor,
            None,
            &main,
            &[
                "-c",
                "user.name=forge-bot",
                "-c",
                "user.email=forge-bot@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "local work",
            ],
        )
        .await
        .unwrap();
        run_git(
            &executor,
            None,
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        )
        .await
        .unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        set_checkout_mtime(&main, old);
        let retention = Duration::from_secs(60);
        let protected = HashSet::new();

        assert!(
            manager
                .retire_stale(retention, &protected)
                .unwrap()
                .is_none()
        );
        run_git(&executor, None, &linked, &["status"])
            .await
            .unwrap();
        let summary = run_git(&executor, None, &linked, &["log", "-1", "--format=%s"])
            .await
            .unwrap();
        assert_eq!(summary.trim(), "local work");

        run_git(
            &executor,
            None,
            &main,
            &["worktree", "remove", linked.to_str().unwrap()],
        )
        .await
        .unwrap();
        set_checkout_mtime(&main, old);
        manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap()
            .purge()
            .unwrap();
        assert!(!main.exists());
    }

    #[test]
    #[ignore = "run inside a private user and mount namespace with unshare --map-root-user --mount"]
    fn cleanup_respects_same_filesystem_bind_mounts() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspaces");
        let manager = workspace_manager(&WorkspaceConfig {
            root: root.clone(),
            ..Default::default()
        });
        let stale = root.join("Org__Repo-1");
        let mounted = root.join("Org__Repo-2");
        let checkout_with_alias = root.join("Org__Repo-3");
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("artifact"), "build output").unwrap();
        let cache_mount = mounted.join("build cache\t\n\\variant");
        fs::create_dir_all(&cache_mount).unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        checkout(&stale, old);
        checkout(&mounted, old);
        checkout(&checkout_with_alias, old);

        let bind = |source: &Path, target: &Path| {
            let result = Command::new("mount")
                .arg("--bind")
                .arg(source)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                fs::metadata(target).unwrap().dev(),
                fs::metadata(source).unwrap().dev()
            );
        };
        bind(&cache, &cache_mount);

        let checkout_alias = dir.path().join("checkout alias");
        fs::create_dir(&checkout_alias).unwrap();
        bind(&checkout_with_alias, &checkout_alias);
        manager.record_use(&checkout_alias);
        assert!(
            fs::metadata(&checkout_with_alias)
                .unwrap()
                .modified()
                .unwrap()
                > old
        );

        let retention = Duration::from_secs(60);
        let protected = HashSet::new();
        let trash = manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap();
        assert!(mounted.is_dir());
        assert!(checkout_with_alias.is_dir());
        assert!(!stale.exists());

        let staged = fs::read_dir(root.join(TRASH_DIR))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let recovered_mount = staged.join("recovered cache");
        fs::create_dir(&recovered_mount).unwrap();
        bind(&cache, &recovered_mount);
        drop(trash);
        manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap()
            .purge()
            .unwrap();
        assert!(staged.join(".git/HEAD").is_file());
        assert!(cache_mount.join("artifact").is_file());
        assert!(recovered_mount.join("artifact").is_file());

        // Model a mount that was absent when the mount table was read.
        let error = remove_directory_contents(&open_directory(&staged).unwrap()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EXDEV));
        assert_eq!(
            fs::read_to_string(cache.join("artifact")).unwrap(),
            "build output"
        );

        for mount in [&recovered_mount, &cache_mount, &checkout_alias] {
            assert!(
                Command::new("umount")
                    .arg(mount)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        manager
            .retire_stale(retention, &protected)
            .unwrap()
            .unwrap()
            .purge()
            .unwrap();
        assert!(!mounted.exists());
        assert!(!staged.exists());
        assert!(cache.join("artifact").is_file());
    }

    #[test]
    fn builds_path_safely() {
        let manager = workspace_manager(&WorkspaceConfig {
            root: PathBuf::from("/tmp/ws"),
            ..Default::default()
        });
        let path = manager.path_for(&msg(), None);
        assert_eq!(path, PathBuf::from("/tmp/ws/Org__My-Repo-3"));
        // An explicit user gets a private subdirectory; the legacy path is
        // unchanged.
        assert_eq!(
            manager.path_for(&msg(), Some("reviewer")),
            PathBuf::from("/tmp/ws/reviewer/Org__My-Repo-3")
        );
    }

    #[test]
    fn gitlab_merge_requests_share_issue_identity_only_when_linked() {
        use crate::agent::{AgentContext, conversation_key};
        let mut issue = msg();
        issue.forge = ForgeKind::GitLab;
        let mut merge_request = issue.clone();
        merge_request.is_pull_request = true;
        let backend = |message: &ForgeMessage| {
            let key = conversation_key(&AgentContext {
                forge: Some(message.forge),
                repository: message.repository.clone(),
                is_pull_request: message.is_pull_request,
                issue_number: message.number,
                linked_issue: message.linked_issue.clone(),
                ..Default::default()
            });
            assert_eq!(key, message.conversation_key());
            key
        };
        assert_ne!(
            workspace_dir_name(&issue),
            workspace_dir_name(&merge_request)
        );
        assert_ne!(backend(&issue), backend(&merge_request));
        merge_request.number = Some(7);
        merge_request.linked_issue = Some(crate::forge::IssueRef {
            repository: None,
            number: 3,
        });
        assert_eq!(
            workspace_dir_name(&issue),
            workspace_dir_name(&merge_request)
        );
        assert_eq!(backend(&issue), backend(&merge_request));
        merge_request.repository = "other/repo".into();
        merge_request.linked_issue.as_mut().unwrap().repository = Some(issue.repository.clone());
        assert_ne!(
            workspace_dir_name(&issue),
            workspace_dir_name(&merge_request)
        );
        assert_eq!(backend(&issue), backend(&merge_request));
    }

    #[test]
    fn linked_pr_reuses_issue_checkout_only_in_the_same_repository() {
        let issue = msg();
        let mut pr = issue.clone();
        pr.is_pull_request = true;
        pr.number = Some(4);
        pr.linked_issue = Some(crate::forge::IssueRef {
            repository: None,
            number: 3,
        });
        assert_eq!(workspace_dir_name(&issue), workspace_dir_name(&pr));
        pr.linked_issue.as_mut().unwrap().repository = Some(issue.repository.clone());
        assert_eq!(workspace_dir_name(&issue), workspace_dir_name(&pr));
        pr.linked_issue.as_mut().unwrap().repository = Some("other/repo".into());
        assert_eq!(workspace_dir_name(&pr), "Org__My-Repo-4");
    }

    #[test]
    fn explicit_users_check_out_in_their_home() {
        let mut accounts = std::collections::BTreeMap::new();
        accounts.insert(
            "agent".to_owned(),
            crate::executor::HostAccount {
                name: "agent".to_owned(),
                uid: 1000,
                gid: 1000,
                home: PathBuf::from("/home/agent"),
            },
        );
        let manager = WorkspaceManager::new(
            &WorkspaceConfig {
                root: PathBuf::from("/tmp/ws"),
                ..Default::default()
            },
            Arc::new(Executor::with_accounts_for_tests(accounts)),
        );
        // The configured root is ignored for an explicit account: the checkout
        // is rooted in that account's home so it owns every path component.
        assert_eq!(
            manager.dir_for(&msg(), Some("agent"), Some("forge-bot")),
            PathBuf::from("/home/agent/Org__My-Repo-3")
        );
        // No `host_user` keeps the legacy root.
        assert_eq!(
            manager.dir_for(&msg(), None, None),
            PathBuf::from("/tmp/ws/Org__My-Repo-3")
        );
    }

    #[test]
    fn builds_clean_clone_url() {
        assert_eq!(
            clean_clone_url(&msg()),
            "http://forge.local:3000/Org/My-Repo.git"
        );
    }

    #[test]
    fn git_credentials_are_only_in_the_environment() {
        for key in ["FORGEJO_TOKEN", "GITHUB_TOKEN", "GITLAB_TOKEN"] {
            let credentials = vec![(key.to_owned(), "synthetic-secret".to_owned())];
            let spec = git_spec(
                Some("agent"),
                Path::new("/tmp"),
                &["clone", "https://forge.invalid/o/r.git"],
                &credentials,
            );
            assert!(!spec.args.iter().any(|arg| arg.contains("synthetic-secret")));
            assert!(
                spec.env
                    .iter()
                    .any(|(_, value)| value == "synthetic-secret")
            );
            assert!(spec.args.contains(&"credential.helper=".into()));
        }
    }

    #[test]
    fn errors_redact_raw_and_url_encoded_credentials() {
        let credentials = vec![("FORGEJO_TOKEN".into(), "secret/@value".into())];
        let text = redact_git_error(
            "secret/@value https://secret%2F%40value@forge.invalid",
            &credentials,
        );
        assert!(!text.contains("secret"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn credential_selection_prefers_forge_specific_tokens() {
        let creds = vec![
            ("FORGE_TOKEN".to_string(), "generic".to_string()),
            ("GITHUB_TOKEN".to_string(), "specific".to_string()),
        ];
        assert_eq!(
            credential(&creds, ForgeKind::GitHub).as_deref(),
            Some("specific")
        );
        assert_eq!(
            credential(&creds, ForgeKind::GitLab).as_deref(),
            Some("generic")
        );
        assert_eq!(
            credential(&creds, ForgeKind::Unknown).as_deref(),
            Some("generic")
        );

        let empty = vec![("GITHUB_TOKEN".to_string(), String::new())];
        assert_eq!(credential(&empty, ForgeKind::GitHub), None);
    }

    #[test]
    fn sanitizes_unsafe_path_characters() {
        assert_eq!(sanitize("a/b c"), "a_b_c");
        assert_eq!(sanitize("safe-._"), "safe-._");
    }

    #[test]
    fn path_for_without_number_is_latest() {
        let mut message = msg();
        message.number = None;
        let manager = workspace_manager(&WorkspaceConfig {
            root: PathBuf::from("/tmp/ws"),
            ..Default::default()
        });
        assert_eq!(
            manager.path_for(&message, None),
            PathBuf::from("/tmp/ws/Org__My-Repo-latest")
        );
    }

    #[tokio::test]
    async fn run_git_reports_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let output = run_git(&Executor::direct(), None, dir.path(), &["--version"])
            .await
            .unwrap();
        assert!(output.to_lowercase().contains("git"));

        let error = run_git(
            &Executor::direct(),
            None,
            dir.path(),
            &["rev-parse", "--show-toplevel"],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BotError::Other(_)));
    }

    #[tokio::test]
    async fn run_git_maps_auth_failures_to_permission_denied() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://{addr}/repo.git");
        let error = run_git(
            &Executor::direct(),
            None,
            dir.path(),
            &["clone", &url, "clone"],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BotError::ForgePermissionDenied(_)));
    }

    #[tokio::test]
    async fn authenticated_clone_keeps_tokens_out_of_origin_and_errors() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let authenticated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = authenticated.clone();
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let (status, headers, body) = if !request
                    .to_ascii_lowercase()
                    .contains("authorization: basic c3ludghldgljlxrva2vuog==")
                {
                    (
                        "401 Unauthorized",
                        "WWW-Authenticate: Basic realm=\"test\"\r\n",
                        "",
                    )
                } else {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let path = request.split_whitespace().nth(1).unwrap();
                    if path.starts_with("/Org/My-Repo.git/info/refs") {
                        ("200 OK", "", "")
                    } else if path == "/Org/My-Repo.git/HEAD" {
                        ("200 OK", "", "ref: refs/heads/main\n")
                    } else {
                        ("404 Not Found", "", "synthetic-token")
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let manager = workspace_manager(&WorkspaceConfig {
            root: dir.path().to_owned(),
            ..Default::default()
        });
        let credentials = vec![("FORGEJO_TOKEN".into(), "synthetic-token".into())];
        let mut message = msg();
        message.location = format!("http://{addr}/Org/My-Repo/issues/3")
            .parse()
            .unwrap();
        let checkout = manager
            .prepare(&message, &credentials, None, None)
            .await
            .unwrap();
        assert!(authenticated.load(std::sync::atomic::Ordering::SeqCst) > 0);
        let origin = run_git(
            &Executor::direct(),
            None,
            &checkout,
            &["remote", "get-url", "origin"],
        )
        .await
        .unwrap();
        assert_eq!(origin.trim(), format!("http://{addr}/Org/My-Repo.git"));
        assert!(
            !std::fs::read_to_string(checkout.join(".git/config"))
                .unwrap()
                .contains("synthetic-token")
        );
        let bad_url = format!("http://{addr}/missing.git");
        let error = run_git_with_credentials(
            &Executor::direct(),
            None,
            dir.path(),
            &["clone", &bad_url, "missing"],
            &credentials,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("synthetic-token"));
        server.abort();
    }

    #[tokio::test]
    async fn prepare_without_workspace_skips_cloning() {
        let dir = tempfile::tempdir().unwrap();
        let manager = workspace_manager(&WorkspaceConfig {
            enabled: false,
            root: dir.path().to_path_buf(),
            ..Default::default()
        });
        let path = manager.prepare(&msg(), &[], None, None).await.unwrap();
        assert!(path.is_dir());
        assert!(!path.join(".git").exists());
    }

    #[tokio::test]
    async fn prepare_reuses_an_existing_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let manager = workspace_manager(&WorkspaceConfig {
            enabled: true,
            reuse: true,
            root: dir.path().to_path_buf(),
            ..Default::default()
        });
        let path = manager.path_for(&msg(), None);
        tokio::fs::create_dir_all(&path).await.unwrap();
        run_git(&Executor::direct(), None, &path, &["init"])
            .await
            .unwrap();
        run_git(
            &Executor::direct(),
            None,
            &path,
            &[
                "remote",
                "add",
                "origin",
                "http://old-secret@127.0.0.1:9/o/r.git",
            ],
        )
        .await
        .unwrap();

        let prepared = manager
            .prepare(
                &msg(),
                &[("FORGEJO_TOKEN".into(), "tok".into())],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(prepared, path);
        let config = tokio::fs::read_to_string(path.join(".git/config"))
            .await
            .unwrap();
        assert!(!config.contains("old-secret"));
        assert!(!config.contains("tok@"));
    }

    #[tokio::test]
    async fn prepare_surfaces_clone_failures() {
        let dir = tempfile::tempdir().unwrap();
        let manager = workspace_manager(&WorkspaceConfig {
            enabled: true,
            reuse: true,
            root: dir.path().to_path_buf(),
            ..Default::default()
        });
        let mut message = msg();
        // Connection refused is immediate, unlike a DNS miss.
        message.location = Url::parse("http://127.0.0.1:1/Org/My-Repo/issues/3").unwrap();

        let error = manager
            .prepare(&message, &[], None, None)
            .await
            .unwrap_err();
        assert!(matches!(error, BotError::Agent { .. }));
    }
}
