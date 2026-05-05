//! macOS in-app update, Sparkle-style. Download the notarized DMG,
//! mount it, verify the new .app is signed, copy it to a sibling
//! staging path, then write a tiny bash helper that waits for *this*
//! process to exit, swaps the bundles, removes the backup, and
//! relaunches. We exit immediately after spawning the helper.
//!
//! Why a helper script instead of doing the swap in-process: macOS
//! gets unhappy when you mv a .app over the running one — the dyld
//! shared cache and the kernel's view of the executable can drift,
//! occasionally producing "killed: 9" on relaunch. The helper-after-
//! exit pattern is what Sparkle ships and is the proven safe path.

use crossbeam_channel::Sender;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone)]
pub enum InstallEvent {
    Log(String),
    Phase(String),
    DownloadProgress { bytes: u64, total: u64 },
    Done,
    Error(String),
}

/// Walk up from the running executable to find the enclosing .app.
/// Returns None when running outside a bundle (e.g. `cargo run`),
/// in which case the in-app updater is disabled.
pub fn current_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut cur = exe.as_path();
    while let Some(parent) = cur.parent() {
        if parent.extension().and_then(|e| e.to_str()) == Some("app") {
            return Some(parent.to_path_buf());
        }
        cur = parent;
    }
    None
}

/// Catch the easy "user dragged app out of /Applications and the
/// parent dir isn't writable" case before we waste bandwidth on a
/// 50 MB download.
pub fn check_writable_parent(app: &Path) -> Result<(), String> {
    let parent = app.parent().ok_or_else(|| "no parent directory".to_string())?;
    let probe = parent.join(".create_shorts_write_test");
    fs::File::create(&probe)
        .map_err(|e| format!("cannot write to {}: {}", parent.display(), e))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

pub fn install_macos(url: &str, current_app: &Path, tx: Sender<InstallEvent>) -> Result<(), String> {
    check_writable_parent(current_app)?;

    let _ = tx.send(InstallEvent::Phase("Downloading".into()));
    let dmg = download_to_temp(url, &tx)?;
    let _ = tx.send(InstallEvent::Log(format!("Downloaded {}", dmg.display())));

    let _ = tx.send(InstallEvent::Phase("Mounting".into()));
    let mount = mount_dmg(&dmg)?;
    let _ = tx.send(InstallEvent::Log(format!("Mounted at {}", mount.display())));

    let result = stage_from_mount(&mount, current_app, &tx);
    let _ = detach_mount(&mount);
    let _ = fs::remove_file(&dmg);

    let staging = result?;
    let _ = tx.send(InstallEvent::Phase("Scheduling install".into()));
    spawn_swap_helper(current_app, &staging)?;
    let _ = tx.send(InstallEvent::Done);
    Ok(())
}

fn stage_from_mount(mount: &Path, current_app: &Path, tx: &Sender<InstallEvent>) -> Result<PathBuf, String> {
    let new_app = find_app_in_dir(mount).ok_or_else(|| "no .app inside DMG".to_string())?;

    let _ = tx.send(InstallEvent::Phase("Verifying signature".into()));
    let cs = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&new_app)
        .output()
        .map_err(|e| format!("codesign: {}", e))?;
    if !cs.status.success() {
        return Err(format!(
            "codesign verification failed: {}",
            String::from_utf8_lossy(&cs.stderr).trim()
        ));
    }

    let parent = current_app.parent().ok_or("no parent for current .app")?;
    let app_name = current_app.file_name().and_then(|s| s.to_str()).unwrap_or("create_shorts.app");
    let staging = parent.join(format!(".{}.new.{}", app_name, std::process::id()));
    let _ = fs::remove_dir_all(&staging);

    let _ = tx.send(InstallEvent::Phase("Staging".into()));
    let ditto = Command::new("ditto").arg(&new_app).arg(&staging).status()
        .map_err(|e| format!("ditto: {}", e))?;
    if !ditto.success() {
        return Err(format!("ditto exited with {:?}", ditto.code()));
    }
    Ok(staging)
}

fn download_to_temp(url: &str, tx: &Sender<InstallEvent>) -> Result<PathBuf, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("create_shorts_gui-installer")
        .timeout(None)
        .build()
        .map_err(|e| format!("client: {}", e))?;
    let mut resp = client.get(url).send().map_err(|e| format!("GET: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("create_shorts_update_{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir tmp: {}", e))?;
    let path = dir.join("create_shorts.dmg");
    let mut file = fs::File::create(&path).map_err(|e| format!("create dmg: {}", e))?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| format!("read: {}", e))?;
        if n == 0 { break; }
        file.write_all(&buf[..n]).map_err(|e| format!("write: {}", e))?;
        written += n as u64;
        if written - last_emit >= 256 * 1024 || (total > 0 && written == total) {
            let _ = tx.send(InstallEvent::DownloadProgress { bytes: written, total });
            last_emit = written;
        }
    }
    Ok(path)
}

fn mount_dmg(dmg: &Path) -> Result<PathBuf, String> {
    let mount = std::env::temp_dir().join(format!("create_shorts_mount_{}", std::process::id()));
    let _ = fs::create_dir_all(&mount);
    let out = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .output()
        .map_err(|e| format!("hdiutil: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(mount)
}

fn detach_mount(mount: &Path) -> Result<(), String> {
    let out = Command::new("hdiutil").args(["detach", "-quiet"]).arg(mount).output()
        .map_err(|e| format!("hdiutil detach: {}", e))?;
    if !out.status.success() {
        let _ = Command::new("hdiutil").args(["detach", "-force", "-quiet"]).arg(mount).status();
    }
    let _ = fs::remove_dir(mount);
    Ok(())
}

fn find_app_in_dir(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("app") {
            return Some(p);
        }
    }
    None
}

fn spawn_swap_helper(current_app: &Path, staging: &Path) -> Result<(), String> {
    let pid = std::process::id();
    let helper = std::env::temp_dir().join(format!("create_shorts_install_{}.sh", pid));
    let log = std::env::temp_dir().join(format!("create_shorts_install_{}.log", pid));
    let bak = current_app.with_file_name(format!(
        "{}.bak.{}",
        current_app.file_name().and_then(|s| s.to_str()).unwrap_or("create_shorts.app"),
        pid
    ));

    let script = format!(
        "#!/bin/bash\n\
         set -u\n\
         exec >{log_q} 2>&1\n\
         echo waiting for parent {pid}\n\
         for i in $(seq 1 100); do\n\
           if ! kill -0 {pid} 2>/dev/null; then break; fi\n\
           sleep 0.1\n\
         done\n\
         echo swapping bundle\n\
         rm -rf {bak_q}\n\
         mv {cur_q} {bak_q} && mv {new_q} {cur_q}\n\
         rc=$?\n\
         if [ $rc -ne 0 ]; then\n\
           echo swap failed: rc=$rc\n\
           if [ -d {bak_q} ] && [ ! -d {cur_q} ]; then mv {bak_q} {cur_q}; fi\n\
           exit $rc\n\
         fi\n\
         rm -rf {bak_q}\n\
         echo relaunching\n\
         /usr/bin/open {cur_q}\n\
         rm -- \"$0\"\n",
        log_q = shell_quote(&log),
        pid = pid,
        cur_q = shell_quote(current_app),
        new_q = shell_quote(staging),
        bak_q = shell_quote(&bak),
    );

    fs::write(&helper, script).map_err(|e| format!("write helper: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&helper).map_err(|e| format!("perms: {}", e))?.permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&helper, perms);
    }

    Command::new("/bin/bash")
        .arg(&helper)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn helper: {}", e))?;
    Ok(())
}

fn shell_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}
