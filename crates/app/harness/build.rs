//! The icon and version block of `bowyer.exe` on Windows. Without the Windows SDK the
//! executable only lacks its icon; the build does not fail.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=bowyer.ico");
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(std::path::PathBuf::from) else { return };
    let icon = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bowyer.ico");
    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let numeric = parts.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let text = format!(
        "1 ICON \"{icon}\"\n1 VERSIONINFO\n FILEVERSION {numeric}\n PRODUCTVERSION {numeric}\n FILEOS 0x40004L\n FILETYPE 0x1L\nBEGIN\n BLOCK \"StringFileInfo\"\n BEGIN\n  BLOCK \"040904b0\"\n  BEGIN\n   VALUE \"FileDescription\", \"Bowyer\"\n   VALUE \"FileVersion\", \"{version}\"\n   VALUE \"ProductName\", \"Bowyer\"\n   VALUE \"ProductVersion\", \"{version}\"\n  END\n END\n BLOCK \"VarFileInfo\"\n BEGIN\n  VALUE \"Translation\", 0x409, 1200\n END\nEND\n",
        icon = icon.to_string_lossy().replace('\\', "/"),
    );
    let rc = out_dir.join("bowyer.rc");
    if let Err(error) = std::fs::write(&rc, text) {
        println!("cargo:warning=the executable has no icon: {error}");
        return;
    }
    if let Err(error) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=the executable has no icon: {error}");
    }
}
