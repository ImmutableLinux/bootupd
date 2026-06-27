use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use log::debug;

//RPM (Fedora, Opensuse)
const LEGACY_RPMOSTREE_DBPATH: &str = "usr/share/rpm";
const SYSIMAGE_RPM_DBPATH: &str = "usr/lib/sysimage/rpm";

//DPKG (Debian, Ubuntu)
const LEGACY_APT_DBPATH: &str = "var/lib/dpkg";
const SYSIMAGE_APT_DBPATH: &str = "usr/lib/sysimage/dpkg";

//PACMAN (Arch Linux)
const LEGACY_PACMAN_DBPATH: &str = "var/lib/pacman";
const SYSIMAGE_PACMAN_DBPATH: &str = "usr/lib/sysimage/pacman";

//APK (Alpine Linux)
const APK_DBPATH: &str = "usr/lib/apk/db/installed";

pub(crate) struct ManifestEntry {
    package: String,
    time: i64,
}

pub(crate) const MANIFEST_PATH: &str = "usr/lib/bootupd/manifest";

#[derive(Debug, PartialEq)]
enum PackageManager {
    Rpm,
    Dpkg,
    Pacman,
    Apk,
}

fn is_nonempty_file(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

fn is_nonempty_dir(path: &Path) -> bool {
    path.read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

fn find_rpm_dbpath(sysroot_path: &str) -> Option<std::path::PathBuf> {
    let sysroot = Path::new(sysroot_path);
    for dbpath in [SYSIMAGE_RPM_DBPATH, LEGACY_RPMOSTREE_DBPATH] {
        let p = sysroot.join(dbpath);
        if is_nonempty_dir(&p) {
            return Some(p);
        }
    }
    None
}

fn find_dpkg_dbpath(sysroot_path: &str) -> Option<std::path::PathBuf> {
    let sysroot = Path::new(sysroot_path);
    for dbpath in [SYSIMAGE_APT_DBPATH, LEGACY_APT_DBPATH] {
        let p = sysroot.join(dbpath);
        if is_nonempty_file(&p) {
            return Some(p);
        }
    }
    None
}

fn find_pacman_dbpath(sysroot_path: &str) -> Option<std::path::PathBuf> {
    let sysroot = Path::new(sysroot_path);
    for dbpath in [SYSIMAGE_PACMAN_DBPATH, LEGACY_PACMAN_DBPATH] {
        let p = sysroot.join(dbpath);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn detect_package_manager(sysroot_path: &str) -> Result<PackageManager> {
    if let Some(p) = find_rpm_dbpath(sysroot_path) {
        debug!("Detected RPM (dbpath: {})", p.display());
        return Ok(PackageManager::Rpm);
    }

    if let Some(p) = find_dpkg_dbpath(sysroot_path) {
        debug!("Detected DPKG (dbpath: {})", p.display());
        return Ok(PackageManager::Dpkg);
    }

    if let Some(p) = find_pacman_dbpath(sysroot_path) {
        debug!("Detected Pacman (dbpath: {})", p.display());
        return Ok(PackageManager::Pacman);
    }

    if Path::new(sysroot_path).join(APK_DBPATH).exists() {
        debug!("Detected APK");
        return Ok(PackageManager::Apk);
    }

    bail!(
        "No supported package manager found in sysroot '{}' \
         (checked: rpm, dpkg, pacman, apk)",
        sysroot_path
    )
}

fn query_rpm(sysroot_path: &str, file: &Path) -> Result<ManifestEntry> {
    let dbpath = find_rpm_dbpath(sysroot_path)
        .ok_or_else(|| anyhow::anyhow!("RPM database not found in sysroot '{}'", sysroot_path))?;

    let out = Command::new("rpm")
        .arg(format!("--dbpath={}", dbpath.display()))
        .args(["-qf", "--queryformat", "%{nevra},%{buildtime}"])
        .arg(file)
        .output()
        .context("Failed to run rpm")?;

    if !out.status.success() {
        bail!(
            "rpm -qf failed for {}: {}",
            file.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let line = std::str::from_utf8(&out.stdout)
        .context("rpm output is not valid UTF-8")?
        .trim()
        .to_string();

    parse_manifest_entry(&line).with_context(|| format!("Failed to parse rpm output: '{}'", line))
}

fn query_dpkg(sysroot_path: &str, file: &Path) -> Result<ManifestEntry> {
    // Uzywa tej samej sciezki bazy co detect_package_manager
    let dbpath = find_dpkg_dbpath(sysroot_path)
        .ok_or_else(|| anyhow::anyhow!("DPKG database not found in sysroot '{}'", sysroot_path))?;

    let out = Command::new("dpkg")
        .arg(format!("--admindir={}", dbpath.parent().unwrap().display()))
        .args(["-S", &file.to_string_lossy()])
        .output()
        .context("Failed to run dpkg -S")?;

    if !out.status.success() {
        bail!("dpkg -S found no package owning {}", file.display());
    }

    // Format: "pakiet: /sciezka"
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pkg = stdout
        .lines()
        .next()
        .and_then(|l| l.split(':').next())
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse dpkg -S output: '{}'", stdout.trim()))?;

    let ver_out = Command::new("dpkg-query")
        .arg(format!("--admindir={}", dbpath.parent().unwrap().display()))
        .args(["-W", "-f=${Package}-${Version}", &pkg])
        .output()
        .context("Failed to run dpkg-query")?;

    let package = std::str::from_utf8(&ver_out.stdout)
        .context("dpkg-query output is not valid UTF-8")?
        .trim()
        .to_string();

    // dpkg nie przechowuje buildtime — uzywa mtime pliku jako przyblizenia
    let time = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        })
        .unwrap_or(0);

    Ok(ManifestEntry { package, time })
}

fn query_pacman(sysroot_path: &str, file: &Path) -> Result<ManifestEntry> {
    let dbpath = find_pacman_dbpath(sysroot_path).ok_or_else(|| {
        anyhow::anyhow!("Pacman database not found in sysroot '{}'", sysroot_path)
    })?;

    let out = Command::new("pacman")
        .arg(format!("--dbpath={}", dbpath.display()))
        .args(["-Qo", &file.to_string_lossy()])
        .output()
        .context("Failed to run pacman -Qo")?;

    if !out.status.success() {
        bail!("pacman -Qo found no package owning {}", file.display());
    }

    // Format: "/usr/sbin/grub-install is owned by grub 2:2.12-1"
    let stdout = String::from_utf8_lossy(&out.stdout);
    let words: Vec<&str> = stdout.trim().split_whitespace().collect();
    if words.len() < 2 {
        bail!("Unexpected pacman -Qo output: '{}'", stdout.trim());
    }
    let pkg = words[words.len() - 2];
    let ver = words[words.len() - 1];
    let package = format!("{}-{}", pkg, ver);

    // BUILDDATE z <dbpath>/local/<pkg>-<ver>/desc to Unix timestamp
    // Uzywa tej samej sciezki bazy co wykrycie
    let desc_path = dbpath
        .join("local")
        .join(format!("{}-{}", pkg, ver))
        .join("desc");

    let time = if desc_path.exists() {
        let content = std::fs::read_to_string(&desc_path)
            .with_context(|| format!("Failed to read {}", desc_path.display()))?;
        let mut found = false;
        let mut ts = 0i64;
        for line in content.lines() {
            if line == "%BUILDDATE%" {
                found = true;
                continue;
            }
            if found {
                ts = line.parse().unwrap_or(0);
                break;
            }
        }
        ts
    } else {
        std::fs::metadata(file)
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            })
            .unwrap_or(0)
    };

    Ok(ManifestEntry { package, time })
}

fn query_apk(sysroot_path: &str, file: &Path) -> Result<ManifestEntry> {
    let out = Command::new("apk")
        .arg(format!("--root={}", sysroot_path))
        .args(["info", "--who-owns", &file.to_string_lossy()])
        .output()
        .context("Failed to run apk info --who-owns")?;

    if !out.status.success() {
        bail!("apk found no package owning {}", file.display());
    }

    // Format: "/usr/sbin/grub-install is owned by grub-2.12-r0"
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pkg_ver = stdout
        .trim()
        .split(" is owned by ")
        .nth(1)
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("Unexpected apk output: '{}'", stdout.trim()))?;

    let pkg_name = pkg_ver
        .rsplitn(3, '-')
        .last()
        .unwrap_or(&pkg_ver)
        .to_string();

    let ts_out = Command::new("apk")
        .arg(format!("--root={}", sysroot_path))
        .args(["info", "-t", &pkg_name])
        .output()
        .context("Failed to run apk info -t")?;

    let time = std::str::from_utf8(&ts_out.stdout)
        .unwrap_or("")
        .trim()
        .parse::<i64>()
        .unwrap_or(0);

    Ok(ManifestEntry {
        package: pkg_ver,
        time,
    })
}

fn parse_manifest_entry(entry: &str) -> Result<ManifestEntry> {
    let mut parts = entry.splitn(2, ',');
    let package = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing package name"))?
        .to_string();
    let time = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing time"))?
        .trim()
        .parse::<i64>()
        .context("Failed to parse time as integer")?;
    Ok(ManifestEntry { package, time })
}

fn query_file_owner(sysroot_path: &str, pm: &PackageManager, file: &Path) -> Result<ManifestEntry> {
    match pm {
        PackageManager::Rpm => query_rpm(sysroot_path, file),
        PackageManager::Dpkg => query_dpkg(sysroot_path, file),
        PackageManager::Pacman => query_pacman(sysroot_path, file),
        PackageManager::Apk => query_apk(sysroot_path, file),
    }
}

pub(crate) fn generate_manifest(sysroot_path: &str, files: &[&Path]) -> Result<()> {
    if files.is_empty() {
        bail!("No files specified for manifest generation");
    }

    let pm = detect_package_manager(sysroot_path).context("Failed to detect package manager")?;
    println!("Detected package manager: {:?}", pm);

    let mut entries: BTreeMap<String, i64> = BTreeMap::new();

    for file in files {
        if !file.exists() {
            println!("File not found, skipping: {}", file.display());
            continue;
        }
        match query_file_owner(sysroot_path, &pm, file) {
            Ok(entry) => {
                println!(
                    "  {} -> {} (buildtime: {})",
                    file.display(),
                    entry.package,
                    entry.time
                );
                entries
                    .entry(entry.package)
                    .and_modify(|ts| {
                        if entry.time > *ts {
                            *ts = entry.time;
                        }
                    })
                    .or_insert(entry.time);
            }
            Err(e) => {
                println!(
                    "Warning: failed to query owner of {}: {:#}",
                    file.display(),
                    e
                );
            }
        }
    }

    if entries.is_empty() {
        bail!("No packages found for the given files, manifest not written");
    }

    let content: String = entries
        .iter()
        .map(|(pkg, ts)| format!("{},{} ", pkg, ts))
        .collect();

    let manifest_path = Path::new(sysroot_path).join(MANIFEST_PATH);
    std::fs::create_dir_all(manifest_path.parent().expect("manifest has parent dir"))
        .with_context(|| format!("Failed to create manifest directory"))?;
    std::fs::write(&manifest_path, content.trim_end())
        .with_context(|| format!("Failed to write manifest: {}", manifest_path.display()))?;

    println!("Manifest written to: {}", manifest_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_sysroot_with_rpm(dir: &TempDir) {
        let dbpath = dir.path().join(SYSIMAGE_RPM_DBPATH);
        std::fs::create_dir_all(&dbpath).unwrap();
        std::fs::write(dbpath.join("Packages"), b"dummy").unwrap();
    }

    fn make_sysroot_with_dpkg(dir: &TempDir) {
        let dbpath = dir.path().join(LEGACY_APT_DBPATH);
        std::fs::create_dir_all(dbpath.parent().unwrap()).unwrap();
        std::fs::write(&dbpath, b"Package: grub-efi-amd64\n").unwrap();
    }

    fn make_sysroot_with_pacman(dir: &TempDir) {
        let dbpath = dir.path().join(LEGACY_PACMAN_DBPATH);
        std::fs::create_dir_all(&dbpath).unwrap();
        // Musi byc niepusty zeby is_nonempty_dir zwrocilo true
        std::fs::write(dbpath.join("dummy"), b"dummy").unwrap();
    }

    fn make_sysroot_with_apk(dir: &TempDir) {
        let dbpath = dir.path().join(APK_DBPATH);
        std::fs::create_dir_all(dbpath.parent().unwrap()).unwrap();
        std::fs::write(&dbpath, b"P:grub\n").unwrap();
    }

    #[test]
    fn test_detect_rpm() {
        let dir = TempDir::new().unwrap();
        make_sysroot_with_rpm(&dir);
        assert_eq!(
            detect_package_manager(dir.path().to_str().unwrap()).unwrap(),
            PackageManager::Rpm
        );
    }

    #[test]
    fn test_detect_dpkg() {
        let dir = TempDir::new().unwrap();
        make_sysroot_with_dpkg(&dir);
        assert_eq!(
            detect_package_manager(dir.path().to_str().unwrap()).unwrap(),
            PackageManager::Dpkg
        );
    }

    #[test]
    fn test_detect_pacman() {
        let dir = TempDir::new().unwrap();
        make_sysroot_with_pacman(&dir);
        assert_eq!(
            detect_package_manager(dir.path().to_str().unwrap()).unwrap(),
            PackageManager::Pacman
        );
    }

    #[test]
    fn test_detect_apk() {
        let dir = TempDir::new().unwrap();
        make_sysroot_with_apk(&dir);
        assert_eq!(
            detect_package_manager(dir.path().to_str().unwrap()).unwrap(),
            PackageManager::Apk
        );
    }

    #[test]
    fn test_detect_none() {
        let dir = TempDir::new().unwrap();
        assert!(detect_package_manager(dir.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn test_find_rpm_dbpath_sysimage_wins() {
        let dir = TempDir::new().unwrap();
        // Oba istnieja — sysimage powinien wygrac (jest pierwszy w liscie)
        let sysimage = dir.path().join(SYSIMAGE_RPM_DBPATH);
        std::fs::create_dir_all(&sysimage).unwrap();
        std::fs::write(sysimage.join("Packages"), b"dummy").unwrap();
        let legacy = dir.path().join(LEGACY_RPMOSTREE_DBPATH);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("Packages"), b"dummy").unwrap();

        let found = find_rpm_dbpath(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(found, sysimage);
    }

    #[test]
    fn test_find_rpm_dbpath_falls_back_to_legacy() {
        let dir = TempDir::new().unwrap();
        // Tylko legacy istnieje
        let legacy = dir.path().join(LEGACY_RPMOSTREE_DBPATH);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("Packages"), b"dummy").unwrap();

        let found = find_rpm_dbpath(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(found, legacy);
    }

    #[test]
    fn test_find_dpkg_dbpath_sysimage_wins() {
        let dir = TempDir::new().unwrap();
        let sysimage = dir.path().join(SYSIMAGE_APT_DBPATH);
        std::fs::create_dir_all(sysimage.parent().unwrap()).unwrap();
        std::fs::write(&sysimage, b"Package: grub\n").unwrap();
        let legacy = dir.path().join(LEGACY_APT_DBPATH);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"Package: grub\n").unwrap();

        let found = find_dpkg_dbpath(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(found, sysimage);
    }

    #[test]
    fn test_parse_manifest_entry() {
        let e = parse_manifest_entry("grub2-1:2.12-28.fc42,1710000000").unwrap();
        assert_eq!(e.package, "grub2-1:2.12-28.fc42");
        assert_eq!(e.time, 1710000000);

        let e = parse_manifest_entry("shim-x64-15.8-3.x86_64,1700000000").unwrap();
        assert_eq!(e.package, "shim-x64-15.8-3.x86_64");
        assert_eq!(e.time, 1700000000);
    }

    #[test]
    fn test_generate_manifest_no_files() {
        let dir = TempDir::new().unwrap();
        make_sysroot_with_rpm(&dir);
        assert!(generate_manifest(dir.path().to_str().unwrap(), &[]).is_err());
    }
}
