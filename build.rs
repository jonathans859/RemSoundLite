// wxWidgets imports GetWindowSubclass, which only comctl32 v6 exports. Without
// this manifest the exe dies before main with 0xC0000139.
fn main() {
    embed_commit();
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
}

/// The commit being built, as REMSOUNDLITE_COMMIT: the development update channel compares it
/// with the commits listed on the rolling `latest` release. Empty outside a git checkout.
fn embed_commit() {
    let commit = std::env::var("GITHUB_SHA").ok().filter(|c| !c.is_empty()).or_else(|| {
        let output = std::process::Command::new("git").args(["rev-parse", "HEAD"]).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=REMSOUNDLITE_COMMIT={}", commit.unwrap_or_default());
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    // HEAD names a branch; the branch's ref file changes on every commit.
    let git = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".git");
    println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed={}", git.join(reference).display());
    }
}
