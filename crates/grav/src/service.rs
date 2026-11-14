//! `grav service`: run the gravitond that ships next to this `grav` (inside
//! Graviton.app) as a launch agent, so it starts at login and restarts if it
//! crashes. The same thing `scripts/install-launch-agent.sh --app` does, for
//! Macs without the source.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const LABEL: &str = "dev.graviton.gravitond";

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME isn't set")?;
    Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn log_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).context("HOME isn't set")?;
    Ok(home.join("Library/Logs/Graviton/gravitond.log"))
}

fn domain() -> String {
    // SAFETY: getuid(2) can't fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// The gravitond next to this executable, following the ~/.local/bin link.
fn bundled_daemon() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let daemon = exe.with_file_name("gravitond");
    if !daemon.is_file() {
        bail!("there's no gravitond next to {}; run the grav inside Graviton.app (Graviton.app/Contents/MacOS/grav)", exe.display());
    }
    Ok(daemon)
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install(remote: bool) -> Result<()> {
    let daemon = bundled_daemon()?;
    let plist = plist_path()?;
    let log = log_path()?;
    fs::create_dir_all(plist.parent().unwrap())?;
    fs::create_dir_all(log.parent().unwrap())?;
    let remote_arg = if remote { "<string>--remote</string>" } else { "" };
    fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string>{remote_arg}</array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{}</string>
  <key>StandardOutPath</key><string>{}</string>
</dict>
</plist>
"#,
            xml(&daemon.display().to_string()),
            xml(&log.display().to_string()),
            xml(&log.display().to_string()),
        ),
    )?;
    let service = format!("{}/{LABEL}", domain());
    let _ = Command::new("/bin/launchctl").args(["bootout", &service]).output();
    // bootout returns before the old engine has gone; bootstrap fails (error 5) until it has.
    for _ in 0..50 {
        let loaded = Command::new("/bin/launchctl").args(["print", &service]).output().is_ok_and(|o| o.status.success());
        if !loaded {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let out = Command::new("/bin/launchctl").args(["bootstrap", &domain()]).arg(&plist).output()?;
    if !out.status.success() {
        bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    println!("gravitond is running from {} and starts at login{}.", daemon.display(), if remote { ", accepting paired Macs and phones on port 7843" } else { "" });
    println!("Logs: {}", log.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).output();
    let plist = plist_path()?;
    if plist.exists() {
        fs::remove_file(&plist)?;
    }
    println!("gravitond no longer starts at login, and it's stopped. Your containers and images are kept.");
    Ok(())
}

pub fn status() -> Result<()> {
    let plist = plist_path()?;
    if !plist.exists() {
        println!("Not installed. Run `grav service install --remote` from Graviton.app's grav.");
        return Ok(());
    }
    let text = fs::read_to_string(&plist)?;
    let program = text.split("<string>").nth(2).and_then(|s| s.split("</string>").next()).unwrap_or("?");
    let out = Command::new("/bin/launchctl").args(["print", &format!("{}/{LABEL}", domain())]).output()?;
    let info = String::from_utf8_lossy(&out.stdout);
    let state = info.lines().find_map(|l| l.trim().strip_prefix("state = ")).unwrap_or("not loaded");
    let pid = info.lines().find_map(|l| l.trim().strip_prefix("pid = "));
    println!("Launch agent  {LABEL}");
    println!("Engine        {program}");
    println!("Remote        {}", if text.contains("--remote") { "on (port 7843)" } else { "off" });
    println!("State         {state}{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
    println!("Logs          {}", log_path()?.display());
    Ok(())
}
