//! Updating Whisker from its own releases.
//!
//! Asks GitHub which release is the newest, and when it's newer than this
//! one downloads this system's file from it and installs it. Through a
//! proxy when one is given (the caller passes an account's), so GitHub then
//! sees the proxy, not this device.
//!
//! Nothing is installed unless it's signed with Whisker's update key (the
//! public half is below; the other half signs releases when they're built).
//! The signature covers the file and the version it claims to be, so a file
//! from an older release can't be passed off as a new one. Someone who took
//! over the releases page couldn't get anything installed without that key.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// Where the newest release is described.
const LATEST: &str = "https://api.github.com/repos/anti-ghostsec/whisker/releases/latest";
/// Every file of a release is under here: nothing is fetched from anywhere else.
const DOWNLOADS: &str = "https://github.com/anti-ghostsec/whisker/releases/download/";
/// The public half of the key releases are signed with (Ed25519).
const SIGNING_KEY: [u8; 32] = [
    0x8d, 0x9d, 0x66, 0x62, 0x9e, 0x3c, 0xe1, 0x8a, 0x35, 0xf7, 0xa5, 0x01, 0x10, 0xde, 0x51, 0x8d, 0x7c, 0xa3, 0xb0, 0xd5, 0xac, 0xc6, 0x1e, 0x57, 0xc8, 0x5f, 0x61, 0x27, 0x79, 0x27, 0xff, 0x79,
];
/// What the requests say they come from: a common browser, not this app.
const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Biggest file taken.
const MAX_BYTES: u64 = 600 * 1024 * 1024;

/// A release newer than this Whisker.
#[derive(Clone, Debug)]
pub struct Update {
    /// Its version (`0.42.0`).
    pub version: String,
    /// This system's file in it and that file's signature, when it has them.
    file: Option<(String, String)>,
    /// The file's size, as the release says.
    pub size: Option<u64>,
}

impl Update {
    /// Whether it can be installed from here (it has a signed file for this system).
    pub fn installable(&self) -> bool {
        self.file.is_some()
    }
}

/// The name of this system's file in a release, if Whisker can update itself here.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(target_os = "android") {
        cfg!(target_arch = "aarch64").then_some("whisker.apk")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("whisker-linux-x86_64.bin")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("whisker-windows-x86_64.exe")
    } else {
        None
    }
}

/// This Whisker's version.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether version `a` is newer than `b` (`0.10.0` is newer than `0.9.3`).
/// Anything that isn't numbers and dots isn't newer than anything.
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<u64>>>();
    match (parts(a), parts(b)) {
        (Some(a), Some(b)) if !a.is_empty() => a > b,
        _ => false,
    }
}

fn http(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut b = crate::tls::http_builder().user_agent(AGENT).connect_timeout(Duration::from_secs(30));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).context("that proxy address can't be used")?);
    }
    Ok(b.build()?)
}

/// The newest release, if it's newer than this Whisker.
pub async fn check(proxy: Option<&str>) -> Result<Option<Update>> {
    let v: serde_json::Value = http(proxy)?
        .get(LATEST)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("couldn't reach GitHub")?
        .error_for_status()
        .context("GitHub didn't say what the newest release is")?
        .json()
        .await
        .context("couldn't read GitHub's answer")?;
    Ok(newest(&v, current(), asset_name()))
}

/// [`check`]'s answer from GitHub's description of a release.
fn newest(release: &serde_json::Value, current: &str, asset: Option<&str>) -> Option<Update> {
    let version = release.get("tag_name")?.as_str()?.trim_start_matches('v').to_owned();
    if !newer(&version, current) {
        return None;
    }
    let assets = release.get("assets").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    let named = |name: &str| assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name)).cloned();
    // Only addresses on the releases page itself.
    let url = |a: &serde_json::Value| a.get("browser_download_url").and_then(|u| u.as_str()).filter(|u| u.starts_with(DOWNLOADS)).map(str::to_owned);
    let file = asset.and_then(|name| Some((named(name)?, named(&format!("{name}.sig"))?)));
    let size = file.as_ref().and_then(|(f, _)| f.get("size").and_then(|s| s.as_u64()));
    let file = file.and_then(|(f, s)| Some((url(&f)?, url(&s)?)));
    Some(Update { version, file, size })
}

/// What a release's signature is of: the file's SHA-256, then the version.
fn signed_message(digest: &[u8], version: &str) -> Vec<u8> {
    [digest, version.as_bytes()].concat()
}

/// Checks `signature` is Whisker's update key's, for a file with this
/// SHA-256 in release `version`.
fn verify(digest: &[u8], version: &str, signature: &[u8]) -> Result<()> {
    let key = ed25519_dalek::VerifyingKey::from_bytes(&SIGNING_KEY).context("the update key is damaged")?;
    let signature = ed25519_dalek::Signature::from_slice(signature).map_err(|_| anyhow::anyhow!("the update's signature isn't one"))?;
    key.verify_strict(&signed_message(digest, version), &signature).map_err(|_| anyhow::anyhow!("the update isn't signed with Whisker's key, so it wasn't installed"))
}

/// Downloads `update`'s file to `to` and checks its signature; `progress`
/// hears how many bytes have come. The file is removed again if anything
/// is wrong with it.
pub async fn download(update: &Update, proxy: Option<&str>, to: &Path, progress: impl Fn(u64)) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let Some((file, sig)) = &update.file else { bail!("this release has no signed file for this system") };
    let http = http(proxy)?;
    let signature = http.get(sig).timeout(Duration::from_secs(60)).send().await?.error_for_status()?.bytes().await.context("couldn't get the update's signature")?;
    let mut res = http.get(file).send().await.context("couldn't reach GitHub")?.error_for_status().context("couldn't get the update")?;
    let mut out = tokio::fs::File::create(to).await.with_context(|| format!("couldn't write to {}", to.display()))?;
    let got = async {
        let (mut hash, mut n) = (Sha256::new(), 0u64);
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), res.chunk()).await.context("the download stopped")?? {
            n += chunk.len() as u64;
            if n > MAX_BYTES {
                bail!("the update is too big to be right");
            }
            hash.update(&chunk);
            out.write_all(&chunk).await?;
            progress(n);
        }
        out.flush().await?;
        out.sync_all().await?;
        verify(&hash.finalize(), &update.version, &signature)
    }
    .await;
    drop(out);
    if got.is_err() {
        let _ = tokio::fs::remove_file(to).await;
    }
    got
}

/// Where a downloaded update goes before it's installed: beside the
/// program, so it can take its place in one step.
#[cfg(not(target_os = "android"))]
pub fn staging_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).context("couldn't find Whisker's own file")?;
    let name = exe.file_name().context("couldn't find Whisker's own file")?.to_string_lossy().into_owned();
    Ok(exe.with_file_name(format!(".{name}.update")))
}

/// Puts the downloaded (and checked) file at `new` in the running
/// program's place. The one running carries on; the new one runs from the
/// next start.
#[cfg(not(target_os = "android"))]
pub fn install(new: &Path) -> Result<()> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).context("couldn't find Whisker's own file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(new, &exe).with_context(|| format!("couldn't replace {}", exe.display()))?;
    }
    #[cfg(not(unix))]
    {
        // A running program can't be written over, but it can be moved aside.
        let old = old_path(&exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).with_context(|| format!("couldn't move {} aside", exe.display()))?;
        if let Err(e) = std::fs::rename(new, &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(e).with_context(|| format!("couldn't replace {}", exe.display()));
        }
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "android")))]
fn old_path(exe: &Path) -> std::path::PathBuf {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    exe.with_file_name(format!(".{name}.old"))
}

/// Removes what an earlier update left behind (the program it replaced, or
/// a download that never finished). Called when Whisker starts.
pub fn clean_up() {
    #[cfg(not(target_os = "android"))]
    if let Ok(staged) = staging_path() {
        let _ = std::fs::remove_file(staged);
    }
    #[cfg(not(any(unix, target_os = "android")))]
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.42.0", "0.41.9"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.41.4", "0.41.4"));
        assert!(!newer("0.41.3", "0.41.4"));
        assert!(!newer("", "0.41.4"));
        assert!(!newer("0.42.0-beta", "0.41.4"));
        assert!(!newer("latest", "0.41.4"));
    }

    #[test]
    fn reads_a_release() {
        let at = |name: &str| format!("{DOWNLOADS}v9.0.0/{name}");
        let release = serde_json::json!({
            "tag_name": "v9.0.0",
            "assets": [
                {"name": "whisker-linux-x86_64.bin", "size": 123, "browser_download_url": at("whisker-linux-x86_64.bin")},
                {"name": "whisker-linux-x86_64.bin.sig", "size": 64, "browser_download_url": at("whisker-linux-x86_64.bin.sig")},
                {"name": "whisker.apk", "size": 5, "browser_download_url": "https://example.org/whisker.apk"},
                {"name": "whisker.apk.sig", "size": 64, "browser_download_url": at("whisker.apk.sig")},
            ]
        });
        let u = newest(&release, "0.41.4", Some("whisker-linux-x86_64.bin")).unwrap();
        assert_eq!((u.version.as_str(), u.size, u.installable()), ("9.0.0", Some(123), true));
        // A file that isn't on the releases page isn't taken.
        assert!(!newest(&release, "0.41.4", Some("whisker.apk")).unwrap().installable());
        // Nor one with no signature beside it, or a system with no file.
        assert!(!newest(&release, "0.41.4", Some("whisker-windows-x86_64.exe")).unwrap().installable());
        assert!(!newest(&release, "0.41.4", None).unwrap().installable());
        // Not newer: nothing.
        assert!(newest(&release, "9.0.0", Some("whisker.apk")).is_none());
        assert!(newest(&serde_json::json!({"tag_name": "nightly"}), "0.41.4", None).is_none());
    }

    /// A signature made the way releases are signed (`openssl pkeyutl -sign
    /// -rawin`, with the real key) over a file holding `abc`, as version 1.2.3.
    #[test]
    fn checks_signatures() {
        let digest = Sha256::digest(b"abc");
        let signature = hex(SAMPLE_SIGNATURE);
        verify(&digest, "1.2.3", &signature).unwrap();
        // Another version, another file, or a damaged signature: refused.
        assert!(verify(&digest, "1.2.4", &signature).is_err());
        assert!(verify(&Sha256::digest(b"abd"), "1.2.3", &signature).is_err());
        let mut bad = signature.clone();
        bad[10] ^= 1;
        assert!(verify(&digest, "1.2.3", &bad).is_err());
        assert!(verify(&digest, "1.2.3", &signature[..63]).is_err());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    const SAMPLE_SIGNATURE: &str = "47f257a77a0f1172b41e3fdcada6f68893aee23e1f6e6dfa199f52b836a8971d3b41c779e92dd68a2380ef74808fc87c1456afc4374b038107a6660ac1c88504";
}
