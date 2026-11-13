// wxWidgets imports GetWindowSubclass, which only comctl32 v6 exports. Without
// this manifest the exe dies before main with 0xC0000139.
fn main() {
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
