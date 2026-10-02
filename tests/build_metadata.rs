//! Exercise the actual build script against isolated Git layouts.

use std::path::Path;
use std::process::Command;

fn run(command: &mut Command) -> String {
    let output = command.output().expect("command should run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(Command::new("git")
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(args))
}

#[test]
fn build_metadata_rejects_parent_repositories_and_accepts_linked_worktrees() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("build-script");
    run(
        Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--edition=2024")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .arg("-o")
            .arg(&script),
    );

    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

    let archive = repo.join("archive");
    std::fs::create_dir(&archive).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"),
        archive.join("build.rs"),
    )
    .unwrap();
    let metadata = |path: &Path| {
        run(Command::new(&script)
            .current_dir(path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE"))
    };
    assert_eq!(
        metadata(&archive),
        "cargo:rerun-if-changed=build.rs\ncargo:rustc-env=FORGE_BOT_COMMIT=unknown\n"
    );

    assert!(metadata(&repo).contains(&format!("FORGE_BOT_COMMIT={commit}\n")));

    let worktree = dir.path().join("worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
            "HEAD",
        ],
    );
    let output = metadata(&worktree);
    assert!(output.contains(&format!("FORGE_BOT_COMMIT={commit}\n")));
    assert!(output.contains("/worktrees/worktree/HEAD"));
}
