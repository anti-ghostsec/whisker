// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
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
    whisker::run(dir)
}
