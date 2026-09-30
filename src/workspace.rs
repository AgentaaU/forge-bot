//! Repository checkout management.
//!
//! Before invoking an agent we can prepare a working copy of the repository so
//! that CLI agents start inside the code they are asked to change. Preparing a
//! workspace is optional; with `workspace.enabled = false` agents run in an
//! empty directory and are expected to clone/access the forge themselves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use url::Url;

use crate::config::WorkspaceConfig;
use crate::error::{BotError, Result};
use crate::executor::{ExecSpec, Executor, Prepared};
use crate::forge::ForgeMessage;
use crate::location::ForgeKind;

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

/// Name of the checkout directory for one message.
fn workspace_dir_name(message: &ForgeMessage) -> String {
    let owner = sanitize(message.owner());
    let repo = sanitize(message.repo_name());
    let suffix = message
        .number
        .map(|n| format!("-{n}"))
        .unwrap_or_else(|| "-latest".to_owned());
    format!("{owner}__{repo}{suffix}")
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
            number: Some(3),
            is_pull_request: false,
            linked_issue: None,
            event: "issue_comment".into(),
            title: None,
            reply_target: Default::default(),
        }
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
