//! On Windows: the app's icon (resource 1, which GPUI loads for its windows) and version in
//! the exe. GPUI's own build embeds the manifest.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../packaging/windows");
    let icon = root.join("dataman.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut n = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0)).chain(std::iter::repeat(0));
    let numeric = format!("{},{},{},0", n.next().unwrap(), n.next().unwrap(), n.next().unwrap());
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "Dataman\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "dataman\0"
            VALUE "OriginalFilename", "dataman.exe\0"
            VALUE "ProductName", "Dataman\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("dataman.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().unwrap();
}
