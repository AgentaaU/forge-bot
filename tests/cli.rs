//! End-to-end checks of the `forge-bot` binary's CLI.
//!
//! These exercise `src/main.rs` (which unit tests cannot reach) and the
//! startup validation an operator sees before the service runs.

use std::process::Command;

#[test]
fn version_reports_the_build_commit_without_config_or_runtime_git() {
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_forge-bot"))
            .arg(flag)
            .current_dir(dir.path())
            .env("PATH", "")
            .env("FORGE_BOT_CONFIG", dir.path().join("missing.toml"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "forge-bot {} ({})\n",
                env!("CARGO_PKG_VERSION"),
                env!("FORGE_BOT_COMMIT")
            )
        );
    }
}

fn write(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

fn check(config: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_forge-bot"))
        .arg("--config")
        .arg(config)
        .arg("check")
        .output()
        .expect("the forge-bot binary should run")
}

#[test]
fn check_accepts_a_configured_user() {
    let dir = tempfile::tempdir().unwrap();
    let passwd = dir.path().join("passwd");
    write(
        &passwd,
        &format!(
            "agent:x:1000:1000::{}:/bin/bash\n",
            dir.path().join("home/agent").display()
        ),
    );
    // A real cgroup root exposes the cgroup interface files.
    let cgroup_root = dir.path().join("cgroup");
    std::fs::create_dir_all(&cgroup_root).unwrap();
    write(&cgroup_root.join("cgroup.controllers"), "");
    let config = dir.path().join("forge-bot.toml");
    write(
        &config,
        &format!(
            r#"
[forgejo]
base_url = "http://forge.example.com"
bot_username = "forge-bot"

[policy]
allow_all = true

[workspace]
enabled = false

[executor]
cgroup_root = "{}"
passwd_file = "{}"

[users.forge-bot]
role = "default"
host_user = "agent"
token = "secret"
"#,
            cgroup_root.display(),
            passwd.display()
        ),
    );

    let output = check(&config);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("default agent:"));
    assert!(stdout.contains("users:         forge-bot=agent"));
}

#[test]
fn check_rejects_an_unknown_host_user() {
    let dir = tempfile::tempdir().unwrap();
    let passwd = dir.path().join("passwd");
    write(
        &passwd,
        &format!(
            "agent:x:1000:1000::{}:/bin/bash\n",
            dir.path().join("home/agent").display()
        ),
    );
    let config = dir.path().join("forge-bot.toml");
    write(
        &config,
        &format!(
            r#"
[forgejo]
base_url = "http://forge.example.com"
bot_username = "forge-bot"

[policy]
allow_all = true

[workspace]
enabled = false

[executor]
cgroup_root = "{}"
passwd_file = "{}"

[users.forge-bot]
role = "default"
host_user = "ghost"
"#,
            dir.path().join("cgroup").display(),
            passwd.display()
        ),
    );

    let output = check(&config);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown Linux account"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_accepts_a_human_without_a_linux_account() {
    let dir = tempfile::tempdir().unwrap();
    let passwd = dir.path().join("passwd");
    write(
        &passwd,
        &format!(
            "agent:x:1000:1000::{}:/bin/bash\n",
            dir.path().join("home/agent").display()
        ),
    );
    let cgroup_root = dir.path().join("cgroup");
    std::fs::create_dir_all(&cgroup_root).unwrap();
    write(&cgroup_root.join("cgroup.controllers"), "");
    let config = dir.path().join("forge-bot.toml");
    write(
        &config,
        &format!(
            r#"
[forgejo]
base_url = "http://forge.example.com"
bot_username = "forge-bot"

[policy]
allow_all = true

[workspace]
enabled = false

[executor]
cgroup_root = "{}"
passwd_file = "{}"

[users.forge-bot]
role = "default"
host_user = "agent"

[users.alice]
role = "human"
"#,
            cgroup_root.display(),
            passwd.display()
        ),
    );

    let output = check(&config);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("alice="), "{stdout}");
}
