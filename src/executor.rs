//! Process execution and Linux-account isolation.
//!
//! Every child process forge-bot starts — one-shot command agents, the
//! persistent `pi-rpc` pool, and workspace `git` commands — is created through
//! an [`Executor`]. The backend is chosen per run from the `host_user`:
//!
//! * A `host_user` (set for every explicit `[users.*]`, and therefore for the
//!   root-controlled system service) forks the child natively, places it in a
//!   per-run cgroup v2 and drops to that Linux account before `exec`. This is
//!   the account boundary that makes `[users.*].host_user` real, and the reason
//!   a system-level gateway never runs agent or repository code as itself. No
//!   external `systemd-run`/`systemctl` executable is involved.
//! * An unset `host_user` is only reachable from the test-only direct executor;
//!   production always has a configured account.
//!
//! Secrets are passed in the child's environment (`execve`'s envp), never on a
//! command line or in a world-readable file. Cancelling a run writes
//! `cgroup.kill`, which stops the run's whole cgroup, not just the direct
//! child.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::process::Command;

use crate::config::ExecutorConfig;
use crate::error::{BotError, Result};
use crate::identity::Identities;

/// A resolved Linux account from `passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccount {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// Parse a `passwd(5)` file. Malformed lines are skipped rather than failing
/// the whole host, matching `getpwnam`'s tolerance of unrelated entries.
fn parse_passwd(content: &str) -> Vec<HostAccount> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            let gid: u32 = fields.next()?.parse().ok()?;
            let _gecos = fields.next()?;
            let home = fields.next()?;
            Some(HostAccount {
                name: name.to_owned(),
                uid,
                gid,
                home: PathBuf::from(home),
            })
        })
        .collect()
}

/// Everything needed to launch one child process.
#[derive(Debug, Clone, Default)]
pub struct ExecSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Child environment. Applied through `execve`'s envp, never a command
    /// line, so secrets do not appear in `ps`.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// Linux account to run as. An explicit `[users.*]` always sets this, which
    /// selects the cgroup/privilege-drop backend.
    pub host_user: Option<String>,
}

/// A command ready to be spawned, plus the cgroup that must outlive it.
pub struct Prepared {
    pub command: Command,
    /// Guard that kills the run's whole cgroup when dropped. It must stay alive
    /// until the child exits; cancelling a run drops it and stops every
    /// descendant, not just the direct child.
    pub cgroup: Option<CgroupGuard>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("has_cgroup", &self.cgroup.is_some())
            .finish()
    }
}

/// Stops a run's cgroup when the run ends.
///
/// Killing the direct child is not enough: an agent can fork descendants (build
/// tools, language servers, git hooks). Writing `1` to `cgroup.kill` (cgroup
/// v2, Linux 5.14+) stops every process in the run's cgroup. The directory is
/// then removed; if the kernel has not reaped the processes yet the removal is
/// left to a later attempt.
#[derive(Debug)]
pub struct CgroupGuard {
    path: PathBuf,
}

impl CgroupGuard {
    /// cgroup directory for this run.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        // Tests use a plain temporary directory that is not a cgroup; do not
        // touch the host's cgroup filesystem from a test binary.
        if cfg!(test) {
            return;
        }
        cleanup_cgroup(&self.path);
    }
}

/// Kill everything in `path` and remove the cgroup directory.
///
/// The removal is retried in a background thread when a just-killed descendant
/// has not been reaped yet, so the async runtime thread is not blocked.
fn cleanup_cgroup(path: &Path) {
    let _ = std::fs::write(path.join("cgroup.kill"), "1");
    if std::fs::remove_dir(path).is_ok() {
        return;
    }
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_dir(&path).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    });
}

/// Launches child processes for the gateway.
///
/// A run's `host_user` (set for every configured `[users.*]` account) forks the
/// child into a per-run cgroup and drops to that Linux account. The gateway is
/// always multi-user; there is no single-account fallback. Tests may build a
/// [`Self::direct`] executor that skips the cgroup/privilege drop so they can
/// spawn fake CLIs without root.
#[derive(Debug, Clone)]
pub struct Executor {
    cgroup_root: PathBuf,
    accounts: Arc<BTreeMap<String, HostAccount>>,
    /// Test hook: run directly without cgroup isolation or a privilege drop.
    direct: bool,
}

impl Default for Executor {
    fn default() -> Self {
        Self::direct()
    }
}

impl Executor {
    /// Direct execution with no account switching. **Test-only**: production
    /// executors are built with [`Self::from_config`] and always delegate to a
    /// configured account.
    pub fn direct() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup/forge-bot"),
            accounts: Arc::new(BTreeMap::new()),
            direct: true,
        }
    }

    /// A direct executor with a fixed passwd table, for tests that exercise
    /// per-account paths without touching a real host account.
    #[cfg(test)]
    pub fn with_accounts_for_tests(accounts: BTreeMap<String, HostAccount>) -> Self {
        Self {
            cgroup_root: PathBuf::from("/tmp/forge-bot-cgroups"),
            accounts: Arc::new(accounts),
            direct: true,
        }
    }

    /// Build the executor for `config`, reading and validating the passwd file
    /// eagerly so a misconfigured deployment fails at startup.
    pub fn from_config(config: &ExecutorConfig) -> Result<Self> {
        let passwd = crate::config::expand_tilde(&config.passwd_file);
        Ok(Self {
            cgroup_root: crate::config::expand_tilde(&config.cgroup_root),
            accounts: Arc::new(load_accounts(&passwd)?),
            direct: config.direct,
        })
    }

    /// The resolved account for `name`, if the executor loaded a passwd file.
    pub fn account(&self, name: &str) -> Option<&HostAccount> {
        self.accounts.get(name)
    }

    /// Create the cgroup root and report a clear error when it is not usable.
    /// Called by `check` for a multi-user deployment so a host without cgroup
    /// v2 (or without permission) fails before serving.
    pub fn ensure_cgroup_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cgroup_root).map_err(|error| {
            BotError::Config(format!(
                "cannot create cgroup root {}: {error}; a multi-user deployment needs a \
                 writable cgroup v2 hierarchy",
                self.cgroup_root.display()
            ))
        })?;
        if !is_cgroup_root(&self.cgroup_root) {
            return Err(BotError::Config(format!(
                "{} is not a cgroup v2 hierarchy (no cgroup.controllers/cgroup.procs); \
                 mount cgroup2 at /sys/fs/cgroup",
                self.cgroup_root.display()
            )));
        }
        Ok(())
    }

    /// Validate every configured user's `host_user` against the passwd file.
    ///
    /// Rejects unknown accounts, UID 0, and two users resolving to the same
    /// UID. A direct (test) executor has no accounts and is a no-op.
    pub fn validate_users(&self, identities: &Identities) -> Result<()> {
        if self.direct {
            return Ok(());
        }
        let mut seen_uids: BTreeMap<u32, String> = BTreeMap::new();
        for user in identities.users() {
            if user.host_user.is_empty() {
                continue;
            }
            let account = self.accounts.get(&user.host_user).ok_or_else(|| {
                BotError::Config(format!(
                    "user `{}` maps to unknown Linux account `{}`",
                    user.id, user.host_user
                ))
            })?;
            if account.uid == 0 {
                return Err(BotError::Config(format!(
                    "user `{}` must not run as root (account `{}`)",
                    user.id, user.host_user
                )));
            }
            if let Some(other) = seen_uids.insert(account.uid, user.id.clone()) {
                return Err(BotError::Config(format!(
                    "users `{other}` and `{}` map to the same UID {}",
                    user.id, account.uid
                )));
            }
        }
        Ok(())
    }

    /// Create a workspace using the target account's permissions. The gateway
    /// must never create or chown paths inside an agent-writable tree as root.
    pub async fn create_dir_all(&self, path: &Path, host_user: Option<&str>) -> Result<()> {
        let spec = ExecSpec {
            program: "/bin/mkdir".into(),
            args: vec![
                "-p".into(),
                "--".into(),
                path.to_string_lossy().into_owned(),
            ],
            host_user: host_user.map(str::to_owned),
            ..Default::default()
        };
        let Prepared {
            mut command,
            cgroup: _cgroup,
        } = self.command(&spec)?;
        let output = command.output().await?;
        if !output.status.success() {
            return Err(BotError::Config(format!(
                "cannot create workspace {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Read a CLI reply without letting the gateway's privileges follow an
    /// agent-created link. Reply paths are direct children of the gateway's
    /// trusted temporary directory; validate the opened inode, never a path
    /// checked before opening it.
    pub fn read_reply_file(&self, path: &Path, host_user: Option<&str>) -> std::io::Result<String> {
        let uid = if self.direct {
            // SAFETY: geteuid has no pointer arguments or side effects.
            unsafe { libc::geteuid() }
        } else {
            host_user
                .and_then(|user| self.account(user))
                .map(|account| account.uid)
                .filter(|uid| *uid != 0)
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?
        };
        read_owned_reply(path, uid)
    }

    /// Build the command for one child process.
    ///
    /// A production executor (built with [`Self::from_config`]) always requires
    /// a `host_user` and runs the cgroup/privilege-drop backend. A direct test
    /// executor spawns the program as-is.
    pub fn command(&self, spec: &ExecSpec) -> Result<Prepared> {
        if self.direct {
            return Ok(Prepared {
                command: self.direct_command(spec),
                cgroup: None,
            });
        }
        let has_host_user = spec
            .host_user
            .as_deref()
            .map(str::trim)
            .is_some_and(|user| !user.is_empty());
        if !has_host_user {
            return Err(BotError::Config(
                "a run without a host_user cannot be spawned; configure [users.*] so the \
                 executor can delegate it to a Linux account"
                    .into(),
            ));
        }
        self.cgroup_command(spec)
    }

    fn direct_command(&self, spec: &ExecSpec) -> Command {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        command.envs(spec.env.iter().cloned());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.kill_on_drop(true);
        command
    }

    /// Fork the child natively: place it in a per-run cgroup and drop to the
    /// target account before `exec`. No external helper is involved.
    fn cgroup_command(&self, spec: &ExecSpec) -> Result<Prepared> {
        let user = spec
            .host_user
            .as_deref()
            .map(str::trim)
            .filter(|user| !user.is_empty())
            .ok_or_else(|| BotError::Config("the cgroup executor requires a host_user".into()))?;
        let account = self.accounts.get(user).ok_or_else(|| {
            BotError::Config(format!("unknown Linux account `{user}` for the executor"))
        })?;
        if account.uid == 0 {
            return Err(BotError::Config(format!(
                "refusing to run as root (account `{user}`)"
            )));
        }

        // A unique per-run cgroup under a stable per-user parent. The run must
        // be unique because a user can have several concurrent runs (different
        // conversations, the pi-rpc pool); the parent groups all of a user's
        // work for accounting or a single `cgroup.kill`.
        let cgroup = self
            .cgroup_root
            .join(&account.name)
            .join(format!("run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cgroup).map_err(|error| {
            BotError::Config(format!(
                "cannot create cgroup {}: {error}",
                cgroup.display()
            ))
        })?;
        if !is_cgroup_root(&self.cgroup_root) {
            return Err(BotError::Config(format!(
                "{} is not a cgroup v2 hierarchy; refusing to run without isolation",
                self.cgroup_root.display()
            )));
        }

        let env = self.controlled_env(account, &spec.env);
        use std::os::unix::ffi::OsStrExt as _;
        let procs = std::ffi::CString::new(cgroup.join("cgroup.procs").as_os_str().as_bytes())
            .map_err(|_| BotError::Config("cgroup path contains a NUL byte".into()))?;
        let uid = account.uid;
        let gid = account.gid;

        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        // `execve` receives the environment, so secrets never appear in argv.
        command.env_clear();
        command.envs(env);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.kill_on_drop(true);
        // SAFETY: the closure only calls async-signal-safe libc functions on a
        // pre-built C string and integer credentials.
        unsafe {
            command.pre_exec(move || {
                place_in_cgroup(&procs)?;
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Prepared {
            command,
            cgroup: Some(CgroupGuard { path: cgroup }),
        })
    }

    /// Environment the cgroup backend starts with: a controlled base plus the
    /// caller-supplied variables, which win on collision.
    fn controlled_env(
        &self,
        account: &HostAccount,
        extra: &[(String, String)],
    ) -> BTreeMap<String, String> {
        controlled_env_with(account, extra, |path| path.is_dir())
    }
}

/// Nonblocking open prevents a FIFO from hanging before its type is checked.
/// The ownership and link-count checks also exclude hard links to another
/// account's files. Reads are bounded even if the CLI grows the file.
fn read_owned_reply(path: &Path, uid: u32) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    const MAX_REPLY_BYTES: u64 = 1024 * 1024;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    if metadata.len() > MAX_REPLY_BYTES {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    let mut text = String::new();
    file.take(MAX_REPLY_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_REPLY_BYTES {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    Ok(text)
}

/// Build the environment for a cgroup-backed child.
///
/// `XDG_RUNTIME_DIR` is exported only when `/run/user/<uid>` exists. A service
/// is not a login session, so the directory is absent unless the account has
/// lingering enabled (`loginctl enable-linger <user>`). Exporting a path that
/// does not exist makes D-Bus and `systemctl --user` clients fail in confusing
/// ways, so ordinary external commands still work and session-scoped tools get
/// a clear "no session" instead.
fn controlled_env_with(
    account: &HostAccount,
    extra: &[(String, String)],
    exists: impl Fn(&Path) -> bool,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), account.home.display().to_string());
    env.insert("USER".to_owned(), account.name.clone());
    env.insert("LOGNAME".to_owned(), account.name.clone());
    let runtime_dir = Path::new("/run/user").join(account.uid.to_string());
    if exists(&runtime_dir) {
        env.insert(
            "XDG_RUNTIME_DIR".to_owned(),
            runtime_dir.display().to_string(),
        );
    }
    // Prefer the target account's own bin directories, then the gateway's PATH,
    // so per-user agent installs (`~/.local/bin`, cargo, ...) resolve.
    let mut path = vec![
        account.home.join(".local/bin").display().to_string(),
        account.home.join(".cargo/bin").display().to_string(),
    ];
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing.to_string_lossy().into_owned());
    }
    env.insert("PATH".to_owned(), path.join(":"));
    for (key, value) in extra {
        env.insert(key.clone(), value.clone());
    }
    env
}

/// Whether `root` is a cgroup v2 directory (it exposes the cgroup interface
/// files). A plain directory would silently run agents without isolation or
/// whole-cgroup cancellation, so both `check` and every spawn verify it.
fn is_cgroup_root(root: &Path) -> bool {
    root.join("cgroup.controllers").is_file() || root.join("cgroup.procs").is_file()
}

/// Place the calling process in the cgroup whose `cgroup.procs` path is given.
///
/// Runs in the forked child before `exec`, so it uses only async-signal-safe
/// libc calls and a pre-built C string: no allocation, no locks.
fn place_in_cgroup(procs: &std::ffi::CStr) -> std::io::Result<()> {
    // SAFETY: `procs` is a valid NUL-terminated path and the buffer is on the
    // stack. `open`/`write`/`close`/`getpid` are async-signal-safe.
    unsafe {
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid = libc::getpid() as u64;
        let mut buf = [0u8; 20];
        let digits = write_decimal(pid, &mut buf);
        let written = loop {
            let written = libc::write(fd, digits.as_ptr() as *const libc::c_void, digits.len());
            if written < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            break written;
        };
        // Capture errno before close can change it. A partial PID write is not
        // safe to retry: a cgroup write is a single membership operation.
        let result = if written < 0 {
            Err(std::io::Error::last_os_error())
        } else if written as usize != digits.len() {
            Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
        } else {
            Ok(())
        };
        libc::close(fd);
        result
    }
}

/// Write `value` as decimal into `buf`, returning the used slice. No
/// allocation, so it is safe in a forked child.
fn write_decimal(value: u64, buf: &mut [u8]) -> &[u8] {
    let mut i = buf.len();
    let mut value = value;
    loop {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &buf[i..]
}

/// Read a `passwd` file into a name -> account map. The first entry for a name
/// wins, matching `getpwnam`.
fn load_accounts(path: &Path) -> Result<BTreeMap<String, HostAccount>> {
    let content = std::fs::read_to_string(path).map_err(|error| {
        BotError::Config(format!(
            "executor cannot read passwd file {}: {error}",
            path.display()
        ))
    })?;
    let mut accounts: BTreeMap<String, HostAccount> = BTreeMap::new();
    for account in parse_passwd(&content) {
        accounts.entry(account.name.clone()).or_insert(account);
    }
    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn spec(host_user: Option<&str>) -> ExecSpec {
        ExecSpec {
            program: "codex".into(),
            args: vec!["exec".into(), "hello".into()],
            env: vec![("FORGEJO_TOKEN".into(), "s3cret".into())],
            cwd: Some(PathBuf::from("/var/lib/forge-bot/workspaces/agent/o__r-1")),
            host_user: host_user.map(str::to_owned),
        }
    }

    fn account(name: &str, uid: u32, gid: u32, home: &str) -> HostAccount {
        HostAccount {
            name: name.into(),
            uid,
            gid,
            home: PathBuf::from(home),
        }
    }

    #[test]
    fn parses_passwd_entries() {
        let accounts = parse_passwd(
            "root:x:0:0:root:/root:/bin/bash\n\
             agent:x:1000:1000::/home/agent:/bin/bash\n\
             # comment\n\
             bad line\n\
             reviewer:x:1001:1001::/home/reviewer:/bin/sh\n",
        );
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts[1].name, "agent");
        assert_eq!(accounts[1].uid, 1000);
        assert_eq!(accounts[1].gid, 1000);
        assert_eq!(accounts[1].home, PathBuf::from("/home/agent"));
    }

    #[test]
    fn write_decimal_formats_without_allocation() {
        let mut buf = [0u8; 20];
        assert_eq!(write_decimal(0, &mut buf), b"0");
        assert_eq!(write_decimal(7, &mut buf), b"7");
        assert_eq!(write_decimal(12345, &mut buf), b"12345");
        assert_eq!(write_decimal(u32::MAX as u64, &mut buf), b"4294967295");
    }

    #[test]
    fn command_selects_the_backend_by_host_user() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());

        // The test-only direct executor ignores host_user.
        let prepared = Executor::direct().command(&spec(None)).unwrap();
        assert!(prepared.cgroup.is_none());

        // A production executor with a host_user uses the cgroup backend and a
        // stoppable per-run cgroup.
        let prepared = executor.command(&spec(Some("agent"))).unwrap();
        assert!(prepared.cgroup.is_some());
        let cgroup = prepared.cgroup.as_ref().unwrap().path();
        assert!(cgroup.starts_with(dir.path().join("cgroup/agent")));
        assert!(cgroup.exists(), "the run cgroup directory is created");
    }

    #[test]
    fn cgroup_executor_requires_a_host_user() {
        // A production executor (built from config) refuses a run without an
        // account; only the test-only direct executor spawns as-is.
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());
        let error = executor.command(&spec(None)).unwrap_err();
        assert!(error.to_string().contains("without a host_user"));
        assert!(executor.command(&spec(Some("agent"))).is_ok());
    }

    #[test]
    fn direct_command_preserves_program_args_and_cwd() {
        let executor = Executor::direct();
        let prepared = executor.command(&spec(None)).unwrap();
        assert!(prepared.cgroup.is_none());
    }

    #[test]
    fn from_config_reads_passwd_eagerly() {
        let dir = tempfile::tempdir().unwrap();
        let config = ExecutorConfig {
            passwd_file: dir.path().join("missing"),
            ..Default::default()
        };
        assert!(Executor::from_config(&config).is_err());
    }

    #[test]
    fn from_config_systemd_reads_passwd_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        std::fs::write(
            &passwd,
            "root:x:0:0:root:/root:/bin/bash\nagent:x:1000:1000::/home/agent:/bin/bash\n",
        )
        .unwrap();
        let config = ExecutorConfig {
            passwd_file: passwd,
            cgroup_root: dir.path().join("cgroup"),
            direct: false,
        };
        let executor = Executor::from_config(&config).unwrap();
        assert_eq!(executor.account("agent").unwrap().uid, 1000);
        assert!(executor.account("nobody").is_none());
    }

    fn systemd_executor(dir: &Path) -> Executor {
        let passwd = dir.join("passwd");
        std::fs::write(
            &passwd,
            "root:x:0:0:root:/root:/bin/bash\n\
             agent:x:1000:1000::/home/agent:/bin/bash\n\
             dup:x:1000:1000::/home/dup:/bin/bash\n\
             reviewer:x:1001:1001::/home/reviewer:/bin/bash\n",
        )
        .unwrap();
        // A real cgroup root exposes the cgroup interface files, which the
        // executor checks before spawning.
        let cgroup_root = dir.join("cgroup");
        std::fs::create_dir_all(&cgroup_root).unwrap();
        std::fs::write(cgroup_root.join("cgroup.controllers"), "").unwrap();
        Executor::from_config(&ExecutorConfig {
            passwd_file: passwd,
            cgroup_root,
            direct: false,
        })
        .unwrap()
    }

    fn identities(users: &[(&str, &str)]) -> Identities {
        use crate::config::{ForgejoConfig, UserConfig, UserRole};
        let mut config = Config::default();
        config.forges.forgejo = Some(ForgejoConfig {
            bot_username: Some("bot".into()),
            ..Default::default()
        });
        for (index, (id, host)) in users.iter().enumerate() {
            config.users.insert(
                (*id).into(),
                UserConfig {
                    role: if index == 0 {
                        UserRole::Default
                    } else {
                        UserRole::Reviewer
                    },
                    host_user: (*host).into(),
                    agent: None,
                    agent_model: None,
                    token: None,
                },
            );
        }
        Identities::resolve(&config, &[]).unwrap()
    }

    #[test]
    fn validate_users_accepts_distinct_non_root_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());
        // The default user inherits the bot login; only host_user matters here.
        let identities = identities(&[("bot", "agent"), ("reviewer", "reviewer")]);
        executor.validate_users(&identities).unwrap();
    }

    #[test]
    fn validate_users_rejects_unknown_root_and_duplicate_uids() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());

        // No explicit users is no longer a valid deployment; the resolver
        // rejects it before `validate_users` runs.
        assert!(Identities::resolve(&Config::default(), &[]).is_err());

        let unknown = identities(&[("bot", "agent"), ("reviewer", "ghost")]);
        assert!(executor.validate_users(&unknown).is_err());

        // `dup` and `agent` share UID 1000.
        let duplicate = identities(&[("bot", "agent"), ("reviewer", "dup")]);
        assert!(executor.validate_users(&duplicate).is_err());

        // A root mapping never reaches the executor: identity resolution
        // already rejects it. Assert that so the guard is not silently lost.
        let mut config = Config::default();
        config.forges.forgejo = Some(crate::config::ForgejoConfig {
            bot_username: Some("bot".into()),
            ..Default::default()
        });
        config.users.insert(
            "bot".into(),
            crate::config::UserConfig {
                role: crate::config::UserRole::Default,
                host_user: "root".into(),
                agent: None,
                agent_model: None,
                token: None,
            },
        );
        assert!(Identities::resolve(&config, &[]).is_err());
    }

    #[test]
    fn controlled_env_sets_home_and_lets_the_caller_override() {
        let account = account("agent", 1000, 1000, "/home/agent");
        // A lingering account has /run/user/<uid>: export XDG_RUNTIME_DIR.
        let env = controlled_env_with(
            &account,
            &[
                ("HOME".into(), "/custom".into()),
                ("TOKEN".into(), "t".into()),
            ],
            |_| true,
        );
        assert_eq!(env.get("HOME").map(String::as_str), Some("/custom"));
        assert_eq!(env.get("USER").map(String::as_str), Some("agent"));
        assert_eq!(env.get("LOGNAME").map(String::as_str), Some("agent"));
        assert_eq!(
            env.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("t"));
        let path = env.get("PATH").expect("PATH is always set");
        assert!(path.starts_with("/home/agent/.local/bin:/home/agent/.cargo/bin:"));

        // Without a runtime directory (no lingering) it is not exported, so a
        // D-Bus client fails cleanly instead of chasing a missing path.
        let env = controlled_env_with(&account, &[], |_| false);
        assert!(!env.contains_key("XDG_RUNTIME_DIR"));
    }

    #[test]
    fn ensure_cgroup_root_accepts_a_cgroup_directory() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());
        executor.ensure_cgroup_root().unwrap();
    }

    #[test]
    fn ensure_cgroup_root_rejects_a_plain_directory() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        std::fs::write(&passwd, "agent:x:1000:1000::/home/agent:/bin/bash\n").unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let executor = Executor::from_config(&ExecutorConfig {
            passwd_file: passwd,
            cgroup_root: plain,
            direct: false,
        })
        .unwrap();
        assert!(executor.ensure_cgroup_root().is_err());
        // The same guard runs before a spawn, so it never runs unisolated.
        let error = executor.command(&spec(Some("agent"))).unwrap_err();
        assert!(error.to_string().contains("not a cgroup v2 hierarchy"));
    }

    #[test]
    fn default_executor_is_direct() {
        let executor = Executor::default();
        assert!(executor.account("agent").is_none());
        assert!(executor.command(&spec(None)).unwrap().cgroup.is_none());
    }

    #[test]
    fn prepared_debug_reports_cgroup_presence() {
        let prepared = Executor::direct().command(&ExecSpec::default()).unwrap();
        assert!(format!("{prepared:?}").contains("has_cgroup: false"));
    }

    #[test]
    fn systemd_command_rejects_unknown_and_root_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());
        let error = executor.command(&spec(Some("ghost"))).unwrap_err();
        assert!(error.to_string().contains("unknown Linux account"));

        // A passwd file whose only account is root makes the guard observable.
        let passwd = dir.path().join("root-only");
        std::fs::write(&passwd, "root:x:0:0:root:/root:/bin/bash\n").unwrap();
        let root_executor = Executor::from_config(&ExecutorConfig {
            passwd_file: passwd,
            cgroup_root: dir.path().join("cgroup-root"),
            direct: false,
        })
        .unwrap();
        let error = root_executor.command(&spec(Some("root"))).unwrap_err();
        assert!(error.to_string().contains("root"));
    }

    #[test]
    fn cleanup_cgroup_writes_the_kill_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-x");
        std::fs::create_dir(&path).unwrap();
        cleanup_cgroup(&path);
        // A plain directory is not a cgroup, so `rmdir` keeps failing and the
        // marker file remains; the call must still return without blocking.
        assert_eq!(
            std::fs::read_to_string(path.join("cgroup.kill")).unwrap(),
            "1"
        );
    }

    #[test]
    fn cgroup_root_must_be_creatable() {
        let dir = tempfile::tempdir().unwrap();
        let passwd = dir.path().join("passwd");
        std::fs::write(&passwd, "agent:x:1000:1000::/home/agent:/bin/bash\n").unwrap();
        // A regular file in the path makes `create_dir_all` fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let executor = Executor::from_config(&ExecutorConfig {
            passwd_file: passwd,
            cgroup_root: blocker.join("sub"),
            direct: false,
        })
        .unwrap();
        assert!(executor.ensure_cgroup_root().is_err());
        let error = executor.command(&spec(Some("agent"))).unwrap_err();
        assert!(error.to_string().contains("cannot create cgroup"));
    }

    #[test]
    fn place_in_cgroup_writes_the_pid() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cgroup.procs");
        // A real cgroup always has `cgroup.procs`; `place_in_cgroup` opens it
        // for writing, so the test provides the file.
        std::fs::write(&file, "").unwrap();
        let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
        place_in_cgroup(&path).unwrap();
        let pid: u32 = std::fs::read_to_string(&file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn place_in_cgroup_rejects_a_missing_path() {
        let path = std::ffi::CString::new("/no/such/dir/cgroup.procs").unwrap();
        assert_eq!(
            place_in_cgroup(&path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn place_in_cgroup_rejects_a_failed_write() {
        let path = std::ffi::CString::new("/dev/full").unwrap();
        assert_eq!(
            place_in_cgroup(&path).unwrap_err().raw_os_error(),
            Some(libc::ENOSPC)
        );
    }

    #[tokio::test]
    async fn attachment_failure_prevents_exec_and_workspace_creation() {
        let dir = tempfile::tempdir().unwrap();
        let executor = systemd_executor(dir.path());
        let marker = dir.path().join("must-not-exist");
        let mut prepared = executor
            .command(&ExecSpec {
                program: "/bin/mkdir".into(),
                args: vec![marker.display().to_string()],
                host_user: Some("agent".into()),
                ..Default::default()
            })
            .unwrap();
        // The fake cgroup has root interface markers, but no run-level procs.
        // Spawn must fail before setuid or exec, even when tests run as root.
        assert_eq!(
            prepared.command.spawn().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!marker.exists());
        assert!(
            executor
                .create_dir_all(&marker, Some("agent"))
                .await
                .is_err()
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn workspace_creation_does_not_change_symlink_target_ownership() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let before = std::fs::metadata(&target).unwrap();
        let link = dir.path().join("workspace");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        Executor::direct()
            .create_dir_all(&link, None)
            .await
            .unwrap();
        let after = std::fs::metadata(&target).unwrap();
        assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    }

    #[test]
    fn reply_reader_accepts_only_bounded_owned_regular_files() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("reply");
        std::fs::write(&file, "safe reply").unwrap();
        let uid = unsafe { libc::geteuid() };
        assert_eq!(read_owned_reply(&file, uid).unwrap(), "safe reply");
        assert!(read_owned_reply(&file, uid.wrapping_add(1)).is_err());
        let symlink = dir.path().join("symlink");
        std::os::unix::fs::symlink(&file, &symlink).unwrap();
        assert!(read_owned_reply(&symlink, uid).is_err());
        let hardlink = dir.path().join("hardlink");
        std::fs::hard_link(&file, &hardlink).unwrap();
        assert!(read_owned_reply(&hardlink, uid).is_err());
        std::fs::remove_file(&hardlink).unwrap();
        assert!(read_owned_reply(dir.path(), uid).is_err());
        let fifo = dir.path().join("fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
        assert!(read_owned_reply(&fifo, uid).is_err());
        assert!(read_owned_reply(Path::new("/dev/zero"), uid).is_err());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        assert!(read_owned_reply(&file, uid).is_err());
    }
}
