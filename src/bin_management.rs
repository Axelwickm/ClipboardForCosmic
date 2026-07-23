// SPDX-License-Identifier: MIT

use crate::{APP_ID, APP_NAME, ICON};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BINARY_NAME: &str = "clipboard-for-cosmic";
const SERVICE_NAME: &str = "clipboard-for-cosmic.service";

pub fn install() -> Result<(), Box<dyn Error>> {
    let paths = Paths::new()?;
    fs::create_dir_all(&paths.bin_dir)?;
    fs::create_dir_all(&paths.applications_dir)?;
    fs::create_dir_all(&paths.icons_dir)?;
    fs::create_dir_all(&paths.systemd_user_dir)?;
    fs::create_dir_all(&paths.autostart_dir)?;

    install_binary(&std::env::current_exe()?, &paths.binary)?;
    fs::write(&paths.icon, ICON)?;
    remove_file_if_present(&paths.legacy_flash_icon)?;
    remove_file_if_present(&paths.legacy_symbolic_flash_icon)?;
    fs::write(&paths.desktop, desktop_entry(&paths.binary))?;
    fs::write(&paths.service, service_unit(&paths.binary))?;
    fs::write(&paths.autostart, autostart_entry())?;
    remove_file_if_present(&paths.legacy_data_control_environment)?;
    systemctl(&["daemon-reload"])?;
    // Login startup is owned by the visible XDG autostart entry. The service
    // still owns and supervises the actual process.
    systemctl(&["disable", SERVICE_NAME])?;
    systemctl(&["restart", SERVICE_NAME])?;

    println!("{APP_NAME} installed and started.");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    let paths = Paths::new()?;
    // Stop the service before removing the executable it owns.
    systemctl(&["disable", "--now", SERVICE_NAME])?;
    remove_file_if_present(&paths.desktop)?;
    remove_file_if_present(&paths.autostart)?;
    remove_file_if_present(&paths.service)?;
    remove_file_if_present(&paths.icon)?;
    remove_file_if_present(&paths.legacy_flash_icon)?;
    remove_file_if_present(&paths.legacy_symbolic_flash_icon)?;
    remove_file_if_present(&paths.binary)?;
    remove_file_if_present(&paths.legacy_data_control_environment)?;
    systemctl(&["daemon-reload"])?;

    println!("{APP_NAME} stopped and removed from the user-local installation.");
    Ok(())
}

struct Paths {
    bin_dir: PathBuf,
    applications_dir: PathBuf,
    icons_dir: PathBuf,
    systemd_user_dir: PathBuf,
    autostart_dir: PathBuf,
    binary: PathBuf,
    desktop: PathBuf,
    autostart: PathBuf,
    service: PathBuf,
    icon: PathBuf,
    legacy_flash_icon: PathBuf,
    legacy_symbolic_flash_icon: PathBuf,
    legacy_data_control_environment: PathBuf,
}

impl Paths {
    fn new() -> Result<Self, Box<dyn Error>> {
        let home = dirs::home_dir().ok_or("could not determine the home directory")?;
        let data_dir = dirs::data_dir().ok_or("could not determine the user data directory")?;
        let bin_dir = home.join(".local/bin");
        let applications_dir = data_dir.join("applications");
        let icons_dir = data_dir.join("icons/hicolor/scalable/apps");
        let systemd_user_dir = home.join(".config/systemd/user");
        let autostart_dir = home.join(".config/autostart");

        Ok(Self {
            binary: bin_dir.join(BINARY_NAME),
            desktop: applications_dir.join(format!("{APP_ID}.desktop")),
            autostart: autostart_dir.join(format!("{APP_ID}.desktop")),
            service: systemd_user_dir.join(SERVICE_NAME),
            icon: icons_dir.join(format!("{APP_ID}-symbolic.svg")),
            legacy_flash_icon: icons_dir.join(format!("{APP_ID}-flash.svg")),
            legacy_symbolic_flash_icon: icons_dir.join(format!("{APP_ID}-flash-symbolic.svg")),
            legacy_data_control_environment: home
                .join(".config/environment.d/clipboard-for-cosmic.conf"),
            bin_dir,
            applications_dir,
            icons_dir,
            systemd_user_dir,
            autostart_dir,
        })
    }
}

/// Autostart can only be managed by the installed copy of the application.
pub fn is_installed_instance() -> bool {
    let Ok(paths) = Paths::new() else {
        return false;
    };
    let Ok(current) = std::env::current_exe().and_then(fs::canonicalize) else {
        return false;
    };
    fs::canonicalize(paths.binary).is_ok_and(|installed| installed == current)
}

pub fn autostart_enabled() -> bool {
    Paths::new().is_ok_and(|paths| {
        fs::read_to_string(paths.autostart)
            .is_ok_and(|entry| !entry.lines().any(|line| line.trim() == "Hidden=true"))
    })
}

pub fn set_autostart(enabled: bool) -> Result<(), Box<dyn Error>> {
    if !is_installed_instance() {
        return Err("autostart can only be changed by the installed application".into());
    }

    if enabled {
        let paths = Paths::new()?;
        fs::create_dir_all(&paths.autostart_dir)?;
        fs::write(paths.autostart, autostart_entry())?;
    } else {
        remove_file_if_present(&Paths::new()?.autostart)?;
    }
    Ok(())
}

pub fn show_command() -> Result<String, Box<dyn Error>> {
    Ok(format!("{} show", Paths::new()?.binary.display()))
}

fn install_binary(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    // The installed service may be running. Replacing its inode through
    // an atomic rename avoids Linux's ETXTBSY error.
    let staged = destination.with_extension("new");
    fs::copy(source, &staged)?;
    fs::rename(staged, destination)?;
    Ok(())
}

fn desktop_entry(binary: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={APP_NAME}\nComment=Clipboard history for COSMIC\nExec={}\nIcon={APP_ID}-symbolic\nNoDisplay=true\nTerminal=false\nCategories=Utility;\nActions=Show;\n\n[Desktop Action Show]\nName=Show ClipboardForCosmic\nExec={} show\n",
        binary.display(),
        binary.display()
    )
}

fn autostart_entry() -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={APP_NAME}\nComment=Start ClipboardForCosmic in the background\nExec=systemctl --user start {SERVICE_NAME}\nIcon={APP_ID}-symbolic\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    )
}

fn service_unit(binary: &Path) -> String {
    format!(
        "[Unit]\nDescription={APP_NAME}\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy=graphical-session.target\n",
        binary.display()
    )
}

fn systemctl(arguments: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let detail = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "systemctl --user {} failed: {}",
        arguments.join(" "),
        detail.trim()
    )
    .into())
}

fn remove_file_if_present(path: &Path) -> Result<(), Box<dyn Error>> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
