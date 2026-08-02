// upgrade.rs — `claurst upgrade` subcommand.
//
// Downloads the latest release from GitHub, extracts it, and atomically
// replaces the running binary.  Mirrors the logic in install.sh / install.ps1
// so that an `upgrade` from inside Claurst feels identical to a fresh install.
//
// Extraction shells out to `tar` (Linux/macOS) or PowerShell `Expand-Archive`
// (Windows) — both are present on every modern system and saves us pulling
// `flate2`/`tar`/`zip` into the cli crate just for one command.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "Kuberwastaken/claurst";
const APP: &str = "claurst";

pub async fn run_upgrade(args: &[String]) -> Result<()> {
    // -------- arg parsing --------
    let mut requested_version: Option<String> = None;
    let mut force = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                requested_version = iter.next().cloned();
                if requested_version.is_none() {
                    bail!("--version requires an argument");
                }
            }
            "--force" | "-f" => force = true,
            unknown => bail!("Unknown option: {}", unknown),
        }
    }

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    println!("Current version: {}", current_version);

    // -------- detect target --------
    let target = detect_target()?;
    println!("Detected target: {}", target);

    // -------- resolve target version --------
    let target_version = if let Some(v) = requested_version {
        v.trim_start_matches('v').to_string()
    } else {
        fetch_latest_version().await?
    };
    println!("Target version:  {}", target_version);

    if target_version == current_version && !force {
        println!("\nAlready running v{} — nothing to do.", current_version);
        println!("Use --force to reinstall the same version, or --version <v> to install a specific one.");
        return Ok(());
    }

    // -------- locate current exe --------
    let exe_path =
        std::env::current_exe().context("could not determine current executable path")?;
    let exe_path = std::fs::canonicalize(&exe_path).unwrap_or(exe_path);
    println!("Installed at:    {}", exe_path.display());

    // -------- download --------
    let (archive_name, is_zip) = archive_name_for_target(&target);
    let url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        REPO, target_version, archive_name
    );

    let tmp_dir = tempdir_for_upgrade()?;
    let archive_path = tmp_dir.join(&archive_name);

    println!("\nDownloading {}", url);
    download_to_file(&url, &archive_path).await?;

    // -------- extract --------
    let extract_dir = tmp_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir)?;
    println!("Extracting…");
    extract_archive(&archive_path, &extract_dir, is_zip)?;

    // -------- locate the new binary --------
    let bin_name = if cfg!(target_os = "windows") {
        "claurst.exe"
    } else {
        "claurst"
    };
    let new_binary = extract_dir.join(bin_name);
    if !new_binary.exists() {
        // Some archives may extract into a subdirectory — search shallowly.
        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in walkdir_shallow(&extract_dir, 3) {
            if entry.file_name().and_then(|n| n.to_str()) == Some(bin_name) {
                candidates.push(entry);
            }
        }
        if candidates.is_empty() {
            bail!(
                "Archive did not contain expected binary '{}'.\nExtracted to: {}",
                bin_name,
                extract_dir.display()
            );
        }
        std::fs::copy(&candidates[0], &new_binary)?;
    }

    // -------- swap --------
    println!("Replacing running binary…");
    swap_binary(&exe_path, &new_binary)?;

    // -------- macOS quarantine strip --------
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&exe_path)
            .status();
    }

    // -------- cleanup --------
    let _ = std::fs::remove_dir_all(&tmp_dir);

    println!("\nUpgraded to v{}.", target_version);
    println!("Run `claurst --version` in a new shell to verify.");
    Ok(())
}

fn print_help() {
    println!(
        "Usage: claurst upgrade [options]\n\n\
         Options:\n\
           -v, --version <v>   Install a specific version (default: latest)\n\
           -f, --force         Reinstall even if already up to date\n\
           -h, --help          Show this help\n\n\
         Downloads the latest claurst release from GitHub and replaces this\n\
         binary in place. Your settings and sessions are preserved."
    );
}

// ---------------------------------------------------------------------------
// Target detection
// ---------------------------------------------------------------------------

fn detect_target() -> Result<String> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        bail!("Unsupported OS for upgrade");
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        bail!("Unsupported architecture for upgrade");
    };

    Ok(format!("{}-{}", os, arch))
}

fn archive_name_for_target(target: &str) -> (String, bool) {
    if target.starts_with("windows") {
        (format!("{}-{}.zip", APP, target), true)
    } else {
        (format!("{}-{}.tar.gz", APP, target), false)
    }
}

// ---------------------------------------------------------------------------
// GitHub API: latest version
// ---------------------------------------------------------------------------

async fn fetch_latest_version() -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("claurst-upgrade/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("GitHub API returned {} for {}", resp.status(), url);
    }
    let json: serde_json::Value = resp.json().await?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no tag_name in GitHub API response"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

async fn download_to_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!("claurst-upgrade/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!(
            "Download failed: HTTP {} for {}\n\
             Check that this version exists in the releases page.",
            resp.status(),
            url
        );
    }
    let bytes = resp.bytes().await?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Extraction (shells out — tar is on every modern system, including Windows 10+)
// ---------------------------------------------------------------------------

fn extract_archive(archive: &Path, dest: &Path, is_zip: bool) -> Result<()> {
    if is_zip {
        // Windows: prefer PowerShell Expand-Archive; fall back to tar -xf.
        let ps_status = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    archive.display(),
                    dest.display()
                ),
            ])
            .status();
        if let Ok(s) = ps_status {
            if s.success() {
                return Ok(());
            }
        }
        // tar -xf works on Windows 10+ via bsdtar.
        let status = std::process::Command::new("tar")
            .args([
                "-xf",
                &archive.to_string_lossy(),
                "-C",
                &dest.to_string_lossy(),
            ])
            .status()
            .context("failed to spawn tar")?;
        if !status.success() {
            bail!("tar -xf returned non-zero exit status: {}", status);
        }
        Ok(())
    } else {
        let status = std::process::Command::new("tar")
            .args([
                "-xzf",
                &archive.to_string_lossy(),
                "-C",
                &dest.to_string_lossy(),
            ])
            .status()
            .context("failed to spawn tar")?;
        if !status.success() {
            bail!("tar -xzf returned non-zero exit status: {}", status);
        }
        Ok(())
    }
}

// Shallow walker (avoids pulling walkdir crate just for this).
fn walkdir_shallow(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    fn rec(dir: &Path, depth_left: usize, out: &mut Vec<PathBuf>) {
        if depth_left == 0 {
            return;
        }
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rec(&path, depth_left - 1, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    rec(root, max_depth, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Atomic binary swap
// ---------------------------------------------------------------------------

#[cfg(unix)]
struct StagedBinary {
    path: PathBuf,
}

#[cfg(unix)]
impl StagedBinary {
    fn create_next_to(current: &Path) -> Result<(Self, std::fs::File)> {
        use std::ffi::OsString;
        use std::fs::OpenOptions;
        use std::io::ErrorKind;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_STAGE_ID: AtomicU64 = AtomicU64::new(0);
        const MAX_ATTEMPTS: usize = 128;

        let parent = current
            .parent()
            .ok_or_else(|| anyhow!("installed binary path has no parent: {}", current.display()))?;
        let file_name = current.file_name().ok_or_else(|| {
            anyhow!(
                "installed binary path has no file name: {}",
                current.display()
            )
        })?;

        for _ in 0..MAX_ATTEMPTS {
            let id = NEXT_STAGE_ID.fetch_add(1, Ordering::Relaxed);
            let mut staged_name = OsString::from(".");
            staged_name.push(file_name);
            staged_name.push(format!(".upgrade-{}-{}", std::process::id(), id));
            let path = parent.join(staged_name);

            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create upgrade staging file next to {}",
                            current.display()
                        )
                    });
                }
            }
        }

        bail!(
            "failed to create a unique upgrade staging file next to {} after {} attempts",
            current.display(),
            MAX_ATTEMPTS
        )
    }
}

#[cfg(unix)]
impl Drop for StagedBinary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn swap_binary(current: &Path, new: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Windows holds an exclusive lock on the running .exe — we can rename
        // it but not unlink it, so move-aside, then move-in-place.
        let mut sidelined = current.to_path_buf();
        sidelined.set_extension("exe.old");
        let _ = std::fs::remove_file(&sidelined);
        std::fs::rename(current, &sidelined).with_context(|| {
            format!("failed to sideline current exe to {}", sidelined.display())
        })?;
        if let Err(e) = std::fs::copy(new, current) {
            // Try to roll back the rename so the user isn't left without claurst.
            let _ = std::fs::rename(&sidelined, current);
            bail!("failed to install new binary: {}", e);
        }
        // Best-effort cleanup of the old exe (Windows may keep it locked
        // until the running process exits — that's fine).
        let _ = std::fs::remove_file(&sidelined);
        Ok(())
    }

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // std::fs::copy() writes into the existing inode (open + O_TRUNC), and
        // that inode is the running process's own text segment — the kernel
        // rejects the write with ETXTBSY ("Text file busy"). rename() instead
        // swaps the directory entry to point at a new inode, which the kernel
        // allows even while the old inode is still mapped and executing.
        // rename() requires src/dest on the same filesystem, so stage the
        // temp file next to `current` rather than relying on the OS tmp dir.
        let (staged, mut staged_file) = StagedBinary::create_next_to(current)?;
        let mut source = std::fs::File::open(new)
            .with_context(|| format!("failed to open new binary at {}", new.display()))?;
        std::io::copy(&mut source, &mut staged_file)
            .with_context(|| format!("failed to stage new binary at {}", staged.path.display()))?;
        staged_file
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .with_context(|| {
                format!(
                    "failed to make staged binary executable at {}",
                    staged.path.display()
                )
            })?;
        staged_file.flush().with_context(|| {
            format!("failed to flush staged binary at {}", staged.path.display())
        })?;
        staged_file.sync_all().with_context(|| {
            format!("failed to sync staged binary at {}", staged.path.display())
        })?;
        drop(staged_file);

        std::fs::rename(&staged.path, current)
            .with_context(|| format!("failed to swap new binary into {}", current.display()))?;
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    {
        let _ = (current, new);
        bail!("Unsupported OS for upgrade")
    }
}

// ---------------------------------------------------------------------------
// Tempdir
// ---------------------------------------------------------------------------

fn tempdir_for_upgrade() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = base.join(format!("claurst-upgrade-{}-{}", pid, now));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{swap_binary, StagedBinary};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "claurst-upgrade-test-{}-{}-{}",
                std::process::id(),
                name,
                id
            ));
            std::fs::create_dir(&path).expect("failed to create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "spawned by swap_binary_replaces_a_running_executable"]
    fn running_executable_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn staging_files_are_unique_and_removed_on_drop() {
        let dir = TestDir::new("unique-stage");
        let current = dir.path().join("claurst");

        let (first, first_file) =
            StagedBinary::create_next_to(&current).expect("first staging file");
        let first_path = first.path.clone();
        let (second, second_file) =
            StagedBinary::create_next_to(&current).expect("second staging file");
        let second_path = second.path.clone();

        assert_ne!(first_path, second_path);
        assert!(first_path.exists());
        assert!(second_path.exists());

        drop(first_file);
        drop(second_file);
        drop(first);
        drop(second);

        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    // Regression test for the ETXTBSY bug: swap_binary must be able to
    // replace a binary while it is actively running (mirrors `claurst
    // upgrade` replacing its own executable). The current test harness is
    // itself a portable native executable, so copying and spawning it avoids
    // assumptions about whether system tools live in /bin or /usr/bin.
    #[test]
    fn swap_binary_replaces_a_running_executable() {
        let dir = TestDir::new("running-executable");
        let current = dir.path().join("running");
        let replacement = dir.path().join("replacement");
        let legacy_staging_path = current.with_extension("new");
        let replacement_bytes = b"#!/bin/sh\nprintf 'replacement binary\\n'\n";

        std::fs::copy(
            std::env::current_exe().expect("current test executable"),
            &current,
        )
        .expect("failed to copy test executable");
        std::fs::write(&replacement, replacement_bytes).expect("failed to write replacement");
        std::fs::write(&legacy_staging_path, b"must not be overwritten")
            .expect("failed to write legacy staging sentinel");

        let mut child = Command::new(&current)
            .args([
                "--ignored",
                "--exact",
                "upgrade::tests::running_executable_fixture",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn fixture binary");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            child.try_wait().expect("failed to inspect fixture process"),
            None,
            "fixture process exited before the swap"
        );

        // Sanity check: overwriting the file in place while it's running
        // is exactly what triggers ETXTBSY. This confirms the fixture
        // actually reproduces the bug this test guards against.
        #[cfg(target_os = "linux")]
        {
            let direct_overwrite = std::fs::copy(&replacement, &current);
            assert!(
                direct_overwrite.is_err(),
                "expected direct overwrite of a running binary to fail with ETXTBSY"
            );
        }

        let result = swap_binary(&current, &replacement);

        child.kill().ok();
        child.wait().ok();
        result.expect("swap_binary should replace a running binary via rename, not in-place write");

        assert_eq!(
            std::fs::read(&current).expect("failed to read installed replacement"),
            replacement_bytes
        );
        assert_eq!(
            std::fs::metadata(&current)
                .expect("failed to stat installed replacement")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::read(&legacy_staging_path).expect("failed to read staging sentinel"),
            b"must not be overwritten"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .expect("failed to inspect test directory")
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".running.upgrade-")),
            "staging file was not cleaned up"
        );
    }
}
