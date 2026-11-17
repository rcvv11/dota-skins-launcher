//! Embeds the launcher icon (res/icon.ico) into the exe with MinGW's windres.
//! Without windres the exe simply has no icon (the window still gets one).

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app_icon.o");
    let ok = std::process::Command::new("windres")
        .args(["--input-format=rc", "-O", "coff", "-I", "res", "-i", "res/app.rc", "-o"])
        .arg(&out)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        // only the launcher, not gencatalog
        println!("cargo:rustc-link-arg-bin=dota-skins-launcher={}", out.display());
    } else {
        println!("cargo:warning=windres not found: the exe gets no icon");
    }
}
