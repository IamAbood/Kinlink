//! Auto-update from the repo's GitHub Releases: compare versions, download the new exe, check its
//! SHA-256 (published in the release notes), swap it in place, restart.
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

// The full list, not /latest: GitHub's "latest" skips pre-releases, and our versions are betas.
const RELEASES: &str = "https://api.github.com/repos/IamAbood/Kinlink/releases?per_page=20";
pub const ASSET: &str = "Kinlink.exe";
/// Releases made before the rename.
const OLD_ASSET: &str = "ParsecClone.exe";

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// Version of this build (PCLONE_FAKE_VERSION lets tests pretend to be older).
pub fn current() -> String {
    std::env::var("PCLONE_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parse(v: &str) -> Option<((u64, u64, u64), Option<String>)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (v, None),
    };
    let mut n = core.split('.').map(|x| x.parse::<u64>().ok());
    Some(((n.next()??, n.next()??, n.next().flatten().unwrap_or(0)), pre))
}

/// True if `a` is a newer version than `b` (semver-ish: 1.0.0 > 1.0.0-beta > 0.9.0).
pub fn newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some((na, pa)), Some((nb, pb))) if na != nb => na > nb,
        (Some((_, pa)), Some((_, pb))) => match (pa, pb) {
            (None, Some(_)) => true,
            (Some(x), Some(y)) => x > y,
            _ => false,
        },
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().into()
}

/// Asks GitHub for the latest release. Ok(None) = we're up to date.
pub fn check() -> Result<Option<Release>> {
    let url = std::env::var("PCLONE_UPDATE_URL").unwrap_or_else(|_| RELEASES.to_string());
    let body = agent().get(&url).header("User-Agent", "parsec-clone-updater").header("Accept", "application/vnd.github+json").call()?.body_mut().read_to_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    // A list of releases (or a single one): take the newest published version that has our exe.
    let list = v.as_array().cloned().unwrap_or_else(|| vec![v]);
    let Some(j) = list
        .into_iter()
        .filter(|r| r["draft"] != true && r["tag_name"].is_string())
        .filter(|r| r["assets"].as_array().is_some_and(|a| a.iter().any(|x| (x["name"] == ASSET || x["name"] == OLD_ASSET))))
        .reduce(|a, b| if newer(b["tag_name"].as_str().unwrap(), a["tag_name"].as_str().unwrap()) { b } else { a })
    else {
        return Ok(None);
    };
    let version = j["tag_name"].as_str().context("no tag")?.trim_start_matches('v').to_string();
    if !newer(&version, &current()) {
        return Ok(None);
    }
    let asset = j["assets"].as_array().and_then(|a| a.iter().find(|x| (x["name"] == ASSET || x["name"] == OLD_ASSET))).context("release has no Kinlink.exe")?;
    // The release script writes "sha256: <hex>" into the release notes.
    let notes = j["body"].as_str().unwrap_or_default();
    let sha256 = notes
        .lines()
        .find_map(|l| l.trim().strip_prefix("sha256:").map(|h| h.trim().to_lowercase()))
        .filter(|h| h.len() == 64)
        .context("release notes have no sha256")?;
    Ok(Some(Release { version, url: asset["browser_download_url"].as_str().context("no url")?.to_string(), size: asset["size"].as_u64().unwrap_or(0), sha256 }))
}

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

fn old_path(exe: &std::path::Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// Downloads, verifies, swaps the exe and starts the new one. `progress` gets 0..1.
/// On success the caller should exit right away (the new copy waits for us).
pub fn install(r: &Release, progress: impl Fn(f32)) -> Result<()> {
    let exe = exe()?;
    let probe = exe.with_extension("write.test");
    if std::fs::File::create(&probe).is_ok() {
        let _ = std::fs::remove_file(&probe);
        swap(r, progress)?;
    } else {
        // Installed in Program Files: do the swap in an elevated copy (one UAC prompt).
        progress(0.5);
        match crate::syshelper::run_elevated_wait("--update-swap") {
            Some(0) => {}
            Some(_) => bail!("the update failed (download or checksum)"),
            None => bail!("the update needs administrator permission"),
        }
    }
    std::process::Command::new(&exe).arg("--after-update").arg(std::process::id().to_string()).spawn().context("couldn't start the new version")?;
    Ok(())
}

/// Downloads and verifies the new exe, then puts it in place of the running one.
pub fn swap(r: &Release, progress: impl Fn(f32)) -> Result<()> {
    let exe = exe()?;
    let tmp = exe.with_extension("update.tmp");
    let mut resp = agent().get(&r.url).header("User-Agent", "parsec-clone-updater").call()?;
    let mut reader = resp.body_mut().with_config().limit(200 * 1024 * 1024).reader();
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("can't write next to {} (no permission?)", exe.display()))?;
    let (mut hasher, mut done, mut buf) = (Sha256::new(), 0u64, vec![0u8; 64 * 1024]);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if r.size > 0 {
            progress(done as f32 / r.size as f32);
        }
    }
    drop(file);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != r.sha256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("download corrupted (checksum mismatch), nothing was changed");
    }
    // Windows lets a running exe be renamed (not overwritten): park the old one, move the new in.
    let old = old_path(&exe);
    if std::fs::remove_file(&old).is_err() && old.exists() {
        // Still running (the helper service keeps its exe open until a reboot): park it under
        // another name; after_update deletes it once it's free.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = std::fs::rename(&old, exe.with_extension(format!("exe.old{stamp}")));
    }
    std::fs::rename(&exe, &old).context("couldn't move the old version aside")?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::rename(&old, &exe); // put it back
        return Err(anyhow!("couldn't install the update: {e}"));
    }
    Ok(())
}

/// Run first thing at startup: after an update, wait for the old process to exit, then delete it.
pub fn after_update(args: &[String]) {
    if let Some(pid) = args.iter().position(|a| a == "--after-update").and_then(|i| args.get(i + 1)).and_then(|p| p.parse::<u32>().ok()) {
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                WaitForSingleObject(h, 15_000);
            }
        }
    }
    // Delete every leftover old copy that's no longer running (Kinlink.exe.old, Kinlink.exe.old<time>).
    if let Ok(exe) = exe() {
        let name = format!("{}.old", exe.file_name().unwrap_or_default().to_string_lossy().to_lowercase());
        for f in exe.parent().and_then(|d| std::fs::read_dir(d).ok()).into_iter().flatten().flatten() {
            if f.file_name().to_string_lossy().to_lowercase().starts_with(&name) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_order() {
        assert!(newer("0.10.0-beta", "0.9.0-beta"));
        assert!(newer("v1.0.0", "1.0.0-beta"));
        assert!(newer("0.9.1-beta", "0.9.0-beta"));
        assert!(!newer("0.9.0-beta", "0.9.0-beta"));
        assert!(!newer("0.8.0", "0.9.0-beta"));
        assert!(!newer("garbage", "0.9.0"));
    }
}
