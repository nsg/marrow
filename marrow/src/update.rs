use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const GITHUB_API_URL: &str = "https://api.github.com/repos/nsg/marrow/releases/latest";

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn current_target() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64-unknown-linux-gnu"
    }
}

fn binary_name() -> Result<String, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("could not determine binary name")?
        .to_string();
    Ok(name)
}

pub async fn check_for_update() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let release: Release = client
        .get(GITHUB_API_URL)
        .header("User-Agent", format!("marrow/{current}"))
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if is_newer(latest, current) {
        Ok(Some(latest.to_string()))
    } else {
        Ok(None)
    }
}

pub async fn check_and_update() -> Result<bool, Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    eprintln!("[update] checking for updates (current: v{current})");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let release: Release = client
        .get(GITHUB_API_URL)
        .header("User-Agent", format!("marrow/{current}"))
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if !is_newer(latest, current) {
        eprintln!("[update] already up to date (v{current})");
        return Ok(false);
    }

    eprintln!("[update] v{current} → v{latest}, downloading...");

    let target = current_target();
    let expected_asset = format!("marrow-{target}.tar.gz");

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_asset)
        .ok_or_else(|| format!("no asset '{expected_asset}' found in release"))?;

    let bytes = client
        .get(&asset.browser_download_url)
        .header("User-Agent", format!("marrow/{current}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let bin_name = binary_name()?;
    let current_exe = std::env::current_exe()?;
    let own_dir = current_exe
        .parent()
        .ok_or("could not determine binary directory")?;

    // Update self
    update_binary_at(&bytes, &bin_name, &current_exe)?;
    eprintln!("[update] updated {bin_name} to v{latest}");

    // Update sibling binaries found on the system
    for sibling in list_binaries_in_tarball(&bytes)? {
        if sibling == bin_name {
            continue;
        }
        if let Some(path) = find_binary_on_system(&sibling, own_dir) {
            match update_binary_at(&bytes, &sibling, &path) {
                Ok(()) => eprintln!("[update] updated {sibling} at {}", path.display()),
                Err(e) => eprintln!("[update] could not update {sibling}: {e}"),
            }
        }
    }

    Ok(true)
}

fn update_binary_at(
    tarball: &[u8],
    binary_name: &str,
    target_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let bin_bytes = extract_binary_from_tarball(tarball, binary_name)?;
    let temp_path = temp_path_for(target_path);
    std::fs::write(&temp_path, &bin_bytes)?;
    std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&temp_path, target_path)?;
    Ok(())
}

fn list_binaries_in_tarball(tarball: &[u8]) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(tarball);
    let mut archive = Archive::new(decoder);
    let mut names = Vec::new();

    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.push(name.to_string());
        }
    }

    Ok(names)
}

fn find_binary_on_system(name: &str, own_dir: &std::path::Path) -> Option<PathBuf> {
    let candidate = own_dir.join(name);
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

fn extract_binary_from_tarball(
    tarball: &[u8],
    binary_name: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    use tar::Archive;

    let decoder = GzDecoder::new(tarball);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name == binary_name {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }

    Err(format!("binary '{binary_name}' not found in archive").into())
}

fn temp_path_for(exe: &std::path::Path) -> PathBuf {
    let mut temp = exe.to_path_buf();
    temp.set_extension("update.tmp");
    temp
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').filter_map(|s| s.parse().ok()).collect() };
    let l = parse(latest);
    let c = parse(current);
    l > c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.1.0", "1.0.9"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn test_current_target() {
        let target = current_target();
        assert!(
            target == "x86_64-unknown-linux-gnu" || target == "aarch64-unknown-linux-gnu",
            "unexpected target: {target}"
        );
    }

    #[test]
    fn test_extract_binary_from_tarball() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());

        let content = b"#!/bin/fake-binary";
        let mut header = tar::Header::new_gnu();
        header.set_path("marrow").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();

        let tar_bytes = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let gz_bytes = encoder.finish().unwrap();

        let result = extract_binary_from_tarball(&gz_bytes, "marrow").unwrap();
        assert_eq!(result, content);

        let err = extract_binary_from_tarball(&gz_bytes, "nonexistent");
        assert!(err.is_err());
    }

    fn make_test_tarball(names: &[&str]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());
        for name in names {
            let content = format!("fake-{name}");
            let mut header = tar::Header::new_gnu();
            header.set_path(*name).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, content.as_bytes()).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn test_list_binaries_in_tarball() {
        let gz = make_test_tarball(&["marrow", "marrow-discord", "marrow-dash"]);
        let mut names = list_binaries_in_tarball(&gz).unwrap();
        names.sort();
        assert_eq!(names, vec!["marrow", "marrow-dash", "marrow-discord"]);
    }

    #[test]
    fn test_list_binaries_empty_tarball() {
        let gz = make_test_tarball(&[]);
        let names = list_binaries_in_tarball(&gz).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn test_find_binary_in_own_dir() {
        let dir = std::env::temp_dir().join("marrow-test-find-bin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bin_path = dir.join("marrow-discord");
        std::fs::write(&bin_path, b"fake").unwrap();

        assert_eq!(
            find_binary_on_system("marrow-discord", &dir),
            Some(bin_path)
        );
        assert_eq!(find_binary_on_system("nonexistent", &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_update_binary_at() {
        let dir = std::env::temp_dir().join("marrow-test-update-at");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("marrow");
        std::fs::write(&target, b"old-content").unwrap();

        let gz = make_test_tarball(&["marrow", "marrow-discord"]);
        update_binary_at(&gz, "marrow", &target).unwrap();

        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, "fake-marrow");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
