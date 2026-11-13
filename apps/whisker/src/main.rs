// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    // Nothing is logged unless asked for (RUST_LOG=warn): warnings name
    // accounts (hidden ones too) and rooms, and a desktop session often
    // keeps an app's output in its journal on disk.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "off".into()))
        .init();
    if std::env::args().any(|a| a == "--version") {
        println!("whisker {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let dir = match std::env::var_os("WHISKER_DATA_DIR") {
        Some(d) => d.into(),
        None => directories::ProjectDirs::from("io.github", "anti-ghostsec", "Whisker")
            .map(|d| d.data_dir().to_owned())
            .ok_or_else(|| anyhow::anyhow!("no home directory; set WHISKER_DATA_DIR"))?,
    };
    // Files (or links) shared from another app. If Whisker is already open,
    // they go to it and this copy stops here.
    let shares = whisker::share::from_args(std::env::args_os().skip(1));
    if whisker::share::hand_off(&dir, &shares) {
        return Ok(());
    }
    whisker::share::deliver(shares);
    whisker::run(dir)
}
