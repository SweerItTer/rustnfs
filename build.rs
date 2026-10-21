use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/bin/rustnfs-gui.manifest");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") { return; }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/bin/rustnfs-gui.manifest");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=rustnfs-gui=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=rustnfs-gui=/MANIFESTINPUT:{}", manifest.display());
    } else {
        let out = PathBuf::from(env::var("OUT_DIR").unwrap());
        let rc = out.join("rustnfs-gui.rc");
        let object = out.join("rustnfs-gui-res.o");
        std::fs::write(&rc, format!("1 24 \"{}\"\n", manifest.display().to_string().replace('\\', "/"))).unwrap();
        let compiler = env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
        let status = Command::new(&compiler).args(["-i"]).arg(&rc).args(["-O", "coff", "-o"]).arg(&object).status()
            .expect("Windows GNU resource compiler (windres) is required");
        assert!(status.success(), "Failed to compile the Windows GUI manifest");
        println!("cargo:rustc-link-arg-bin=rustnfs-gui={}", object.display());
    }
}
