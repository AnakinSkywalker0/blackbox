//! `bb update`: look for a newer release on GitHub and install it.
//!
//! This is the only code in blackbox that touches the network, and it only runs
//! when you type `bb update`. It sends nothing about you or your machine: one
//! request for the latest release and one download.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Duration;

type Res = Result<(), String>;

const REPO: &str = "AnakinSkywalker0/blackbox";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Largest download or unpacked file we will accept.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The release asset suffix for this platform, if `bb update` can install it.
const PLATFORM: Option<&str> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some("windows-x86_64")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("linux-x86_64")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("macos-arm64")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("macos-x86_64")
} else {
    None
};

/// Windows releases are zips containing bb.exe; the others are tarballs containing bb.
const ARCHIVE_EXT: &str = if cfg!(windows) { "zip" } else { "tar.gz" };

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

// ---- pure helpers (tested) -------------------------------------------------------

/// Parses `v1.2.3`, `1.2.3` or `1.2.3-rc1` into (major, minor, patch).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let v = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(v)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The zip and checksum assets for `version` on `platform`, as (zip url, checksum url, zip name).
fn find_assets<'a>(rel: &'a Release, version: &str, platform: &str) -> Option<(&'a str, &'a str, &'a str)> {
    find_assets_as(rel, version, platform, ARCHIVE_EXT)
}

fn find_assets_as<'a>(rel: &'a Release, version: &str, platform: &str, ext: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let zip_name = format!("bb-v{version}-{platform}.{ext}");
    let sha_name = format!("{zip_name}.sha256");
    let find = |n: &str| rel.assets.iter().find(|a| a.name == n);
    Some((&find(&zip_name)?.browser_download_url, &find(&sha_name)?.browser_download_url, &find(&zip_name)?.name))
}

/// Pulls the hash out of a `sha256sum`-style line (`<hash>  <file>`), or a bare hash.
fn parse_sha_file(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_lowercase();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Finds `bb.exe` inside a release zip and returns its bytes.
fn extract_exe(zip_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| format!("the download isn't a valid zip: {e}"))?;
    for i in 0..zip.len() {
        let file = zip.by_index(i).map_err(|e| e.to_string())?;
        let is_exe = file.enclosed_name().and_then(|p| p.file_name().map(|n| n == "bb.exe")).unwrap_or(false);
        if is_exe {
            let mut out = Vec::new();
            file.take(MAX_BYTES).read_to_end(&mut out).map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
    Err("bb.exe wasn't found inside the download".into())
}

/// Finds the `bb` program inside a release tarball and returns its bytes.
fn extract_tar_bb(tar_gz: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(tar_gz)));
    for entry in archive.entries().map_err(|e| format!("the download isn't a valid tarball: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let is_bb = entry.path().ok().and_then(|p| p.file_name().map(|n| n == "bb")).unwrap_or(false);
        if is_bb && entry.header().entry_type().is_file() {
            let mut out = Vec::new();
            entry.take(MAX_BYTES).read_to_end(&mut out).map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
    Err("bb wasn't found inside the download".into())
}

/// Unpacks the release archive for this platform and returns the program's bytes.
fn extract_program(archive: &[u8]) -> Result<Vec<u8>, String> {
    if cfg!(windows) { extract_exe(archive) } else { extract_tar_bb(archive) }
}

// ---- network ---------------------------------------------------------------------

fn api_url() -> String {
    // Debug builds can be pointed at a local test server. Release builds can't be redirected.
    #[cfg(debug_assertions)]
    if let Ok(u) = std::env::var("BB_UPDATE_API") {
        return u;
    }
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// Downloads must come from this repo's releases, whatever the API response says.
fn trusted_download(url: &str) -> bool {
    #[cfg(debug_assertions)]
    if std::env::var("BB_UPDATE_API").is_ok() {
        return true;
    }
    url.starts_with(&format!("https://github.com/{REPO}/releases/download/"))
}

fn get(url: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).build().into();
    agent.get(url).header("User-Agent", format!("bb/{CURRENT}")).header("Accept", "application/vnd.github+json").call()
}

fn fetch_latest() -> Result<Option<Release>, String> {
    match get(&api_url()) {
        Ok(mut r) => r.body_mut().read_json().map(Some).map_err(|e| format!("unexpected response from GitHub: {e}")),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(format!("couldn't reach GitHub: {e}")),
    }
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    if !trusted_download(url) {
        return Err(format!("refusing to download from an unexpected address: {url}"));
    }
    let mut r = get(url).map_err(|e| format!("download failed: {e}"))?;
    r.body_mut().with_config().limit(MAX_BYTES).read_to_vec().map_err(|e| format!("download failed: {e}"))
}

// ---- the command -----------------------------------------------------------------

pub fn run(db: &Path, check_only: bool) -> Res {
    println!("Current version: {CURRENT}");
    let Some(rel) = fetch_latest()? else {
        println!("No releases found. Nothing to update to yet.");
        return Ok(());
    };
    let latest = rel.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&latest, CURRENT) {
        println!("You're up to date.");
        return Ok(());
    }
    println!("Update available: {CURRENT} -> {latest}");
    if check_only {
        println!("Run `bb update` to install it.");
        return Ok(());
    }
    let Some(platform) = PLATFORM else {
        println!("Automatic install isn't supported on this platform yet.");
        println!("Download it from https://github.com/{REPO}/releases/latest");
        return Ok(());
    };
    let (zip_url, sha_url, zip_name) =
        find_assets(&rel, &latest, platform).ok_or_else(|| format!("release v{latest} has no {platform} download"))?;

    println!("Downloading {zip_name}...");
    let zip_bytes = download(zip_url)?;
    let expected = parse_sha_file(&String::from_utf8_lossy(&download(sha_url)?)).ok_or("the checksum file is malformed")?;
    let actual = sha256_hex(&zip_bytes);
    if actual != expected {
        return Err(format!("checksum mismatch, so nothing was installed.\n  expected {expected}\n  got      {actual}"));
    }
    println!("Checksum OK ({} KB).", zip_bytes.len() / 1024);
    let exe_bytes = extract_program(&zip_bytes)?;
    install_new(db, &exe_bytes, &latest)
}

/// Writes the new binary somewhere safe, proves it runs and reports the right
/// version, then swaps it in for the running one.
fn install_new(db: &Path, exe_bytes: &[u8], version: &str) -> Res {
    let dir = std::env::temp_dir().join(format!("bb-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let staged = dir.join(if cfg!(windows) { "bb.exe" } else { "bb" });
    let result = (|| {
        std::fs::write(&staged, exe_bytes).map_err(|e| format!("can't stage the update: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        }
        let out = std::process::Command::new(&staged).arg("--version").output().map_err(|e| format!("the downloaded program won't run: {e}"))?;
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if said != format!("bb {version}") {
            return Err(format!("the downloaded program reports '{said}', expected 'bb {version}'. Nothing was changed."));
        }
        replace_self(db, &staged, version)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(windows)]
fn replace_self(db: &Path, staged: &Path, version: &str) -> Res {
    use std::process::Command;
    // Read before renaming: after the rename Windows reports the new (.old) name.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = exe.with_file_name("bb.exe.old");

    let was_recording = crate::install::running_pid(db).is_some();
    if was_recording {
        crate::install::stop(db, true)?;
    }
    let restart = |why: &str| {
        if was_recording {
            let _ = Command::new(&exe).arg("--db").arg(db).arg("start").arg("--quiet").status();
        }
        why.to_string()
    };

    // A running program can be renamed but not overwritten, so move it aside first.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| restart(&format!("can't replace {}: {e}", exe.display())))?;
    if let Err(e) = std::fs::copy(staged, &exe) {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::rename(&old, &exe);
        return Err(restart(&format!("couldn't write the new version, so the old one was put back: {e}")));
    }

    println!("Updated {} to {version}.", exe.display());
    if was_recording {
        let ok = Command::new(&exe).arg("--db").arg(db).arg("start").arg("--quiet").status().map(|s| s.success()).unwrap_or(false);
        println!("{}", if ok { "Recording restarted." } else { "Couldn't restart recording. Run `bb start`." });
    }
    if exe.parent() != Some(crate::install::install_dir().as_path()) {
        println!("Note: this isn't the installed copy. Run `bb install` to put this version on your PATH.");
    }
    Ok(())
}

#[cfg(unix)]
fn replace_self(db: &Path, staged: &Path, version: &str) -> Res {
    use std::process::Command;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let was_recording = crate::install::running_pid(db).is_some();
    if was_recording {
        crate::install::stop(db, true)?;
    }
    let restart = |why: &str| {
        if was_recording {
            let _ = Command::new(&exe).arg("--db").arg(db).arg("start").arg("--quiet").status();
        }
        why.to_string()
    };
    // Copy next to the target, then rename over it. A rename is atomic and works while the
    // old program is running, so there is never a half-written bb.
    let beside = exe.with_file_name("bb.update");
    if let Err(e) = std::fs::copy(staged, &beside) {
        return Err(restart(&format!("can't write to {}: {e}. Is it in a folder you can write to?", exe.parent().map_or(".".into(), |p| p.display().to_string()))));
    }
    if let Err(e) = std::fs::rename(&beside, &exe) {
        let _ = std::fs::remove_file(&beside);
        return Err(restart(&format!("can't replace {}: {e}", exe.display())));
    }
    println!("Updated {} to {version}.", exe.display());
    if was_recording {
        let ok = Command::new(&exe).arg("--db").arg(db).arg("start").arg("--quiet").status().map(|s| s.success()).unwrap_or(false);
        println!("{}", if ok { "Recording restarted." } else { "Couldn't restart recording. Run `bb start`." });
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn replace_self(_db: &Path, _staged: &Path, _version: &str) -> Res {
    Err("automatic install isn't supported on this platform yet".into())
}

/// Removes the previous version left behind by an update, once nothing is running it.
pub fn clean_up_old_version() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_file_name("bb.exe.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3"), Some((1, 10, 3)));
        assert_eq!(parse_version("2.0.0-rc1"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn newer_means_strictly_newer() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("v0.10.0", "0.9.9")); // numeric, not text, comparison
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    fn release() -> Release {
        let asset = |n: &str| Asset { name: n.into(), browser_download_url: format!("https://x/{n}") };
        Release {
            tag_name: "v0.3.0".into(),
            assets: vec![
                asset("bb-v0.3.0-linux-x86_64.tar.gz"),
                asset("bb-v0.3.0-windows-x86_64.zip"),
                asset("bb-v0.3.0-windows-x86_64.zip.sha256"),
            ],
        }
    }

    #[test]
    fn picks_the_right_assets() {
        let r = release();
        // Pass the extension explicitly so the test means the same on every platform.
        let (zip, sha, name) = find_assets_as(&r, "0.3.0", "windows-x86_64", "zip").unwrap();
        assert_eq!(name, "bb-v0.3.0-windows-x86_64.zip");
        assert!(zip.ends_with(".zip") && sha.ends_with(".zip.sha256"));
        assert!(find_assets_as(&r, "0.3.0", "macos-arm64", "zip").is_none());
        assert!(find_assets_as(&r, "0.4.0", "windows-x86_64", "zip").is_none());
    }

    #[test]
    fn a_release_missing_its_checksum_is_refused() {
        let mut r = release();
        r.assets.retain(|a| !a.name.ends_with(".sha256"));
        assert!(find_assets_as(&r, "0.3.0", "windows-x86_64", "zip").is_none());
    }

    #[test]
    fn checksum_files_parse() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha_file(&format!("{h}  bb-v0.3.0-windows-x86_64.zip\n")), Some(h.clone()));
        assert_eq!(parse_sha_file(&format!("{} *file.zip", h.to_uppercase())), Some(h.clone()));
        assert_eq!(parse_sha_file(&h), Some(h));
        assert_eq!(parse_sha_file("tooshort file"), None);
        assert_eq!(parse_sha_file(""), None);
        assert_eq!(parse_sha_file(&format!("{} f", "z".repeat(64))), None);
    }

    #[test]
    fn sha256_matches_a_known_value() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extracts_the_exe_from_the_release_folder() {
        let z = make_zip(&[("bb-v0.3.0-windows-x86_64/README.md", b"hi"), ("bb-v0.3.0-windows-x86_64/bb.exe", b"MZ-binary")]);
        assert_eq!(extract_exe(&z).unwrap(), b"MZ-binary");
    }

    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut b = tar::Builder::new(&mut gz);
            for (name, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                b.append_data(&mut h, name, *data).unwrap();
            }
            b.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    #[test]
    fn extracts_bb_from_a_release_tarball() {
        let t = make_tar_gz(&[("bb-v0.4.0-linux-x86_64/README.md", b"hi"), ("bb-v0.4.0-linux-x86_64/bb", b"\x7fELF-binary")]);
        assert_eq!(extract_tar_bb(&t).unwrap(), b"\x7fELF-binary");
    }

    #[test]
    fn tarball_without_bb_or_garbage_is_rejected() {
        assert!(extract_tar_bb(&make_tar_gz(&[("README.md", b"x"), ("bb-v1/bb.txt", b"x")])).is_err());
        assert!(extract_tar_bb(b"not gzip").is_err());
    }

    #[test]
    fn tarball_assets_are_found_for_unix_platforms() {
        let asset = |n: &str| Asset { name: n.into(), browser_download_url: format!("https://x/{n}") };
        let rel = Release {
            tag_name: "v0.4.0".into(),
            assets: vec![
                asset("bb-v0.4.0-macos-arm64.tar.gz"),
                asset("bb-v0.4.0-macos-arm64.tar.gz.sha256"),
                asset("bb-v0.4.0-linux-x86_64.tar.gz"),
            ],
        };
        let (url, sha, name) = find_assets_as(&rel, "0.4.0", "macos-arm64", "tar.gz").unwrap();
        assert!(url.ends_with(".tar.gz") && sha.ends_with(".sha256") && name == "bb-v0.4.0-macos-arm64.tar.gz");
        // Linux has no checksum asset in this fake release, so it must be refused.
        assert!(find_assets_as(&rel, "0.4.0", "linux-x86_64", "tar.gz").is_none());
    }

    #[test]
    fn zip_without_an_exe_or_garbage_is_rejected() {
        assert!(extract_exe(&make_zip(&[("README.md", b"x")])).is_err());
        assert!(extract_exe(b"not a zip").is_err());
    }

    #[test]
    fn zip_entries_cannot_escape_via_path_tricks() {
        // Only the file name is used, so a hostile path still yields bytes, never a path.
        let z = make_zip(&[("../../evil/bb.exe", b"x")]);
        assert!(extract_exe(&z).is_err() || extract_exe(&z).unwrap() == b"x");
    }

    #[test]
    fn only_this_repos_release_downloads_are_trusted() {
        // (Debug test builds only honour the override when BB_UPDATE_API is set; it isn't here.)
        assert!(trusted_download(&format!("https://github.com/{REPO}/releases/download/v1.0.0/bb.zip")));
        assert!(!trusted_download("https://evil.example/bb.zip"));
        assert!(!trusted_download(&format!("http://github.com/{REPO}/releases/download/v1.0.0/bb.zip")));
        assert!(!trusted_download("https://github.com/someone-else/blackbox/releases/download/v1/bb.zip"));
    }

    #[test]
    fn release_json_from_github_parses() {
        let json = r#"{"tag_name":"v0.3.0","name":"x","assets":[{"name":"a.zip","browser_download_url":"https://github.com/o/r/releases/download/v0.3.0/a.zip","size":5,"extra":1}],"other":true}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!((r.tag_name.as_str(), r.assets.len()), ("v0.3.0", 1));
    }
}
