use std::process::Command;

fn is_package_checkout() -> bool {
    let Some(root) = git(&["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .is_ok_and(|package| package == root)
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Git searches parent directories. A source archive inside another checkout
    // must not inherit that checkout's commit or watch its metadata.
    if !is_package_checkout() {
        println!("cargo:rustc-env=FORGE_BOT_COMMIT=unknown");
        return;
    }

    // Resolve paths through Git so linked worktrees and detached HEADs work too.
    // Watch refs as well as HEAD: committing on a branch changes its ref only.
    for name in ["HEAD", "refs", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let commit = git(&["rev-parse", "--verify", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=FORGE_BOT_COMMIT={commit}");
}
