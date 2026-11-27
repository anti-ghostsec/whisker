use std::fmt::Write;

fn main() {
    // One widget style everywhere. Left to itself Slint picks Material on
    // Android, whose widgets size differently from the ones the pages were
    // laid out with.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");
    emoji_table();
    android_libcxx();
    windows_icon();
}

/// Windows: assets/icon.png, as the icon inside whisker.exe.
fn windows_icon() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("whisker.ico");
    let png = image::open("assets/icon.png").expect("assets/icon.png");
    let frames: Vec<_> = [256u32, 64, 48, 32, 16]
        .into_iter()
        .map(|n| {
            let small = png.resize_exact(n, n, image::imageops::FilterType::Lanczos3).into_rgba8();
            image::codecs::ico::IcoFrame::as_png(small.as_raw(), n, n, image::ExtendedColorType::Rgba8).expect("icon frame")
        })
        .collect();
    let file = std::fs::File::create(&out).expect("icon file");
    image::codecs::ico::IcoEncoder::new(file).encode_images(&frames).expect("icon");
    winresource::WindowsResource::new().set_icon(out.to_str().unwrap()).compile().expect("Windows resources");
}

/// Android: links the C++ library of the NDK the build uses (ANDROID_NDK_HOME).
/// Skia is C++ built against that NDK's libc++, while xbuild links with the
/// older one it brings itself. What Skia needs and the older one lacks is
/// then left unresolved, and Android refuses to load the app's library: it
/// opens and closes at once. Only the two C++ archives are put ahead of
/// xbuild's own folder; the rest of the NDK's folder has a static libc.
fn android_libcxx() {
    for v in ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "ANDROID_NDK"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let triple = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "aarch64-linux-android",
        "arm" => "arm-linux-androideabi",
        "x86" => "i686-linux-android",
        "x86_64" => "x86_64-linux-android",
        other => panic!("no Android NDK libraries for {other}"),
    };
    let ndk = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "ANDROID_NDK"]
        .iter()
        .find_map(|v| std::env::var_os(v).filter(|p| !p.is_empty()))
        .map(std::path::PathBuf::from)
        .expect("set ANDROID_NDK_HOME to the Android NDK: its C++ library is linked into Whisker");
    let hosts = ndk.join("toolchains/llvm/prebuilt");
    let libs = std::fs::read_dir(&hosts)
        .unwrap_or_else(|e| panic!("{}: {e}", hosts.display()))
        .filter_map(|e| Some(e.ok()?.path().join("sysroot/usr/lib").join(triple)))
        .find(|d| d.join("libc++_static.a").is_file())
        .unwrap_or_else(|| panic!("no libc++_static.a for {triple} under {}", hosts.display()));
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("ndk-libcxx");
    std::fs::create_dir_all(&out).unwrap();
    for name in ["libc++_static.a", "libc++abi.a"] {
        let from = libs.join(name);
        println!("cargo:rerun-if-changed={}", from.display());
        std::fs::copy(&from, out.join(name)).unwrap_or_else(|e| panic!("{}: {e}", from.display()));
    }
    println!("cargo:rustc-link-search=native={}", out.display());
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
