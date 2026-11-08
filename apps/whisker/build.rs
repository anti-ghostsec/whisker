use std::fmt::Write;

fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI failed to compile");
    emoji_table();
}

/// Embeds the emoji pictures (assets/emoji/emoji_u*.png) as a table from
/// file stem ("emoji_u1f600") to PNG bytes.
fn emoji_table() {
    // Read when the script runs, not when it was compiled: a cached build
    // script would otherwise keep looking in the folder's old place after
    // the project is moved or renamed, and find no pictures.
    let dir =
        std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/emoji");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("emoji_u") && n.ends_with(".png")).collect())
        .unwrap_or_default();
    names.sort();
    assert!(!names.is_empty(), "no emoji pictures in {}", dir.display());
    let mut out = String::from(
        "/// Emoji pictures by file stem, sorted.\npub static PICTURES: &[(&str, &[u8])] = &[\n",
    );
    for n in &names {
        let path = dir.join(n);
        writeln!(out, "    ({:?}, include_bytes!({:?})),", n.trim_end_matches(".png"), path.display().to_string()).unwrap();
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("emoji_pictures.rs");
    std::fs::write(dest, out).unwrap();
}
