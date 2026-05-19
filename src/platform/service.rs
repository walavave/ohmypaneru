use std::{
    env, fs,
    io::{Cursor, Error, ErrorKind, Result, Write},
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose};
use plist::{Dictionary, Value};
use tracing::{info, warn};

use crate::util::exe_path;

/// The bundle identifier for the `paneru` service.
pub const ID: &str = "com.github.karinushka.paneru";
const CONTROL_CENTER_PLIST: &str = "Library/Group Containers/group.com.apple.controlcenter/Library/Preferences/group.com.apple.controlcenter.plist";

/// `Service` manages the installation, uninstallation, starting, and stopping of the `paneru` application as a launchd service.
/// It encapsulates the `launchctl::Service` and the path to the executable.
#[derive(Debug)]
pub struct Service {
    /// The underlying `launchctl::Service` instance.
    pub raw: launchctl::Service,
    /// The absolute path to the `paneru` executable.
    pub bin_path: PathBuf,
    /// The user's home directory.
    home_dir: PathBuf,
}

impl Service {
    /// Creates a new `Service` instance.
    /// It determines the executable path and constructs the `launchctl::Service` with appropriate settings.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the service (e.g., "com.github.karinushka.paneru").
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the service is created successfully, otherwise `Err(Error)` if the executable path or home directory cannot be found.
    pub fn try_new(name: &str) -> Result<Self> {
        let home_dir = env::home_dir().ok_or(Error::new(
            ErrorKind::NotFound,
            "Cannot find home directory.",
        ))?;
        Ok(Self {
            bin_path: exe_path().ok_or(Error::new(
                ErrorKind::NotFound,
                "Cannot find current executable path.",
            ))?,
            raw: launchctl::Service::builder()
                .name(name)
                .uid(unsafe { libc::getuid() }.to_string())
                .plist_path(format!(
                    "{home}/Library/LaunchAgents/{name}.plist",
                    home = home_dir.display()
                ))
                .build(),
            home_dir,
        })
    }

    /// Returns the path to the launchd plist file for this service.
    #[must_use]
    pub fn plist_path(&self) -> &Path {
        Path::new(&self.raw.plist_path)
    }

    /// Returns the stable app bundle path used when Paneru runs as a service.
    #[must_use]
    pub fn app_bundle_path(&self) -> PathBuf {
        self.home_dir.join("Applications/Paneru.app")
    }

    /// Returns the executable inside the stable app bundle.
    #[must_use]
    pub fn app_executable_path(&self) -> PathBuf {
        self.app_bundle_path().join("Contents/MacOS/paneru")
    }

    /// Checks if the service is currently installed (i.e., its plist file exists).
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.plist_path().is_file()
    }

    /// Installs or updates the service as a launch agent by writing its plist file.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is installed or updated successfully, otherwise `Err(Error)` if a file system error occurs.
    pub fn install(&self) -> Result<()> {
        self.install_app_bundle()?;
        let plist_path = self.plist_path();
        let dir = plist_path.parent().ok_or(Error::last_os_error())?;
        if !dir.exists() {
            fs::create_dir_all(dir)?;
        }

        let launchd_plist = self.launchd_plist();
        if self.is_installed() && fs::read_to_string(plist_path).is_ok_and(|s| s == launchd_plist) {
            info!(
                "launch agent already up to date at `{}`",
                plist_path.display()
            );
            return Ok(());
        }

        let mut plist = fs::File::create(plist_path)?;
        plist.write_all(launchd_plist.as_bytes())?;
        info!(
            "installed or updated launch agent at `{}`",
            plist_path.display()
        );
        info!("check logfile /tmp/com.github.karinushka.paneru*.log for potential error messages");
        Ok(())
    }

    /// Uninstalls the service by removing its plist file.
    /// If the service is not installed, a warning is logged, and uninstallation is skipped.
    /// It also attempts to stop the service before removing the file.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is uninstalled successfully or not found, otherwise `Err(Error)` if a file system error occurs.
    pub fn uninstall(&self) -> Result<()> {
        let plist_path = self.plist_path();
        if !self.is_installed() {
            warn!(
                "no launch agent detected at `{}`, skipping uninstallation",
                plist_path.display(),
            );
            self.remove_app_bundle()?;
            self.remove_menu_bar_settings_entries();
            return Ok(());
        }

        if let Err(e) = self.stop() {
            warn!("failed to stop service: {e:?}");
        }

        fs::remove_file(plist_path)?;
        info!(
            "removed existing launch agent at `{}`",
            plist_path.display()
        );
        self.remove_app_bundle()?;
        self.remove_menu_bar_settings_entries();
        Ok(())
    }

    /// Reinstalls the service by first uninstalling it and then installing it again.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service is reinstalled successfully, otherwise `Err(Error)` from underlying install/uninstall operations.
    pub fn reinstall(&self) -> Result<()> {
        self.uninstall()?;
        self.install()
    }

    /// Starts the service using `launchctl`.
    /// If the service is not installed, it will be installed first.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service starts successfully, otherwise `Err(Error)` from `launchctl`.
    pub fn start(&self) -> Result<()> {
        self.install()?;
        info!("starting service...");
        self.raw.start()?;
        info!("service started");
        Ok(())
    }

    /// Stops the service using `launchctl`.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service stops successfully, otherwise `Err(Error)` from `launchctl`.
    pub fn stop(&self) -> Result<()> {
        info!("stopping service...");
        self.raw.stop()?;
        info!("service stopped");
        Ok(())
    }

    /// Restarts the service by first stopping it and then starting it again.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the service restarts successfully, otherwise `Err(Error)` from underlying stop/start operations.
    pub fn restart(&self) -> Result<()> {
        self.stop()?;
        self.start()
    }

    fn install_app_bundle(&self) -> Result<()> {
        let app_path = self.app_bundle_path();
        let contents_path = app_path.join("Contents");
        let macos_path = contents_path.join("MacOS");
        fs::create_dir_all(&macos_path)?;

        let app_executable_path = self.app_executable_path();
        if !paths_equal(&self.bin_path, &app_executable_path)? {
            fs::copy(&self.bin_path, &app_executable_path)?;
        }

        let info_plist_path = contents_path.join("Info.plist");
        fs::write(info_plist_path, self.app_info_plist())?;

        info!(
            "installed or updated app bundle at `{}`",
            app_path.display()
        );
        Ok(())
    }

    fn remove_app_bundle(&self) -> Result<()> {
        let app_path = self.app_bundle_path();
        if !app_path.exists() {
            return Ok(());
        }

        fs::remove_dir_all(&app_path)?;
        info!("removed app bundle at `{}`", app_path.display());
        Ok(())
    }

    fn remove_menu_bar_settings_entries(&self) {
        let plist_path = self.home_dir.join(CONTROL_CENTER_PLIST);
        match remove_menu_bar_entries_from_control_center_plist(&plist_path) {
            Ok(removed) if removed > 0 => {
                info!(
                    "removed {removed} Paneru menu bar settings entries from `{}`",
                    plist_path.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    "failed to remove Paneru menu bar settings entries from `{}`: {error}",
                    plist_path.display()
                );
            }
        }
    }

    fn app_info_plist(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>CFBundleExecutable</key>
    <string>paneru</string>
    <key>CFBundleIdentifier</key>
    <string>{}</string>
    <key>CFBundleName</key>
    <string>Paneru</string>
    <key>CFBundleDisplayName</key>
    <string>Paneru</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>CFBundleVersion</key>
    <string>{}</string>
    <key>LSUIElement</key>
    <true />
    <key>NSHumanReadableCopyright</key>
    <string>Copyright (c) 2025 Karinushka@github. All rights reserved.</string>
  </dict>
</plist>
"#,
            self.raw.name,
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_VERSION"),
        )
    }

    /// Generates the content of the launchd plist file for this service.
    /// This string is formatted with the service name, executable path, and log paths.
    #[must_use]
    pub fn launchd_plist(&self) -> String {
        let xdg_config_home = env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| format!("{}/.config", self.home_dir.display()));
        format!(
            include_str!("../../assets/launchd.plist"),
            name = self.raw.name,
            bin_path = self.app_executable_path().display(),
            out_log_path = self.raw.out_log_path,
            error_log_path = self.raw.error_log_path,
            xdg_config_home = xdg_config_home,
        )
    }
}

fn paths_equal(a: &Path, b: &Path) -> Result<bool> {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => Ok(a == b),
        (Ok(_), Err(error)) if error.kind() == ErrorKind::NotFound => Ok(false),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn remove_menu_bar_entries_from_control_center_plist(plist_path: &Path) -> Result<usize> {
    let mut root = Value::from_file(plist_path).map_err(to_io_error)?;
    let Some(root_dict) = root.as_dictionary_mut() else {
        return Ok(0);
    };
    let Some(tracked_applications) = root_dict.get_mut("trackedApplications") else {
        return Ok(0);
    };

    let tracked_data = tracked_applications_data(tracked_applications)?;
    let mut tracked = Value::from_reader_xml(Cursor::new(tracked_data.clone()))
        .or_else(|_| Value::from_reader(Cursor::new(tracked_data)))
        .map_err(to_io_error)?;
    let Some(items) = tracked.as_array_mut() else {
        return Ok(0);
    };

    let before_len = items.len();
    let mut stripped_menu_locations = 0;
    for item in items.iter_mut() {
        stripped_menu_locations += strip_paneru_menu_item_locations(item);
    }
    items.retain(|item| !value_contains_paneru(item));
    let removed = before_len - items.len() + stripped_menu_locations;

    if removed == 0 {
        return Ok(0);
    }

    let mut encoded_tracked = Vec::new();
    tracked
        .to_writer_binary(&mut encoded_tracked)
        .map_err(to_io_error)?;
    *tracked_applications = Value::Data(encoded_tracked);
    root.to_file_binary(plist_path).map_err(to_io_error)?;
    Ok(removed)
}

fn tracked_applications_data(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Data(data) => Ok(data.clone()),
        Value::String(data) => general_purpose::STANDARD
            .decode(data)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error)),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "trackedApplications is neither data nor base64 string",
        )),
    }
}

fn strip_paneru_menu_item_locations(value: &mut Value) -> usize {
    let Value::Dictionary(dict) = value else {
        return 0;
    };
    let Some(Value::Array(locations)) = dict.get_mut("menuItemLocations") else {
        return 0;
    };

    let before_len = locations.len();
    locations.retain(|location| !value_contains_paneru(location));
    before_len - locations.len()
}

fn value_contains_paneru(value: &Value) -> bool {
    match value {
        Value::String(value) => {
            let value = value.to_lowercase();
            value.contains("paneru") || value.contains("karinushka")
        }
        Value::Array(values) => values.iter().any(value_contains_paneru),
        Value::Dictionary(values) => dictionary_contains_paneru(values),
        _ => false,
    }
}

fn dictionary_contains_paneru(values: &Dictionary) -> bool {
    values
        .iter()
        .any(|(key, value)| key.to_lowercase().contains("paneru") || value_contains_paneru(value))
}

fn to_io_error(error: plist::Error) -> Error {
    Error::new(ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use plist::{Dictionary, Value};

    use super::{ID, Service, remove_menu_bar_entries_from_control_center_plist};

    #[test]
    fn launchd_plist_uses_stable_app_bundle_executable() {
        let service = Service::try_new(ID).expect("service should be constructible");
        let plist = service.launchd_plist();
        let app_executable = service.app_executable_path();

        assert!(
            plist.contains(&format!("<string>{}</string>", app_executable.display())),
            "launchd plist should point at stable app bundle executable: {plist}",
        );
        assert!(
            !plist.contains(&format!("<string>{}</string>", service.bin_path.display())),
            "launchd plist should not point directly at the command-line executable: {plist}",
        );
    }

    #[test]
    fn removes_only_paneru_menu_bar_settings_entries() {
        let path = std::env::temp_dir().join(format!(
            "paneru-control-center-test-{}.plist",
            std::process::id()
        ));
        let root = control_center_plist(vec![
            bundle_item("com.example.keep"),
            menu_item(
                "com.example.host",
                vec![
                    bundle_item("com.example.host"),
                    bundle_item("com.github.karinushka.paneru"),
                ],
            ),
            adhoc_item("file:///opt/homebrew/Cellar/paneru/0.4.1/bin/paneru"),
            menu_item(
                "com.example.keep-menu",
                vec![bundle_item("com.example.keep-menu")],
            ),
        ]);
        root.to_file_binary(&path)
            .expect("test plist should be written");

        let removed = remove_menu_bar_entries_from_control_center_plist(&path)
            .expect("control center plist should be cleaned");

        assert_eq!(removed, 2);
        let cleaned = fs::read(&path).expect("cleaned plist should exist");
        let cleaned =
            Value::from_reader(std::io::Cursor::new(cleaned)).expect("cleaned plist should parse");
        let tracked = cleaned
            .as_dictionary()
            .and_then(|dict| dict.get("trackedApplications"))
            .and_then(Value::as_data)
            .expect("trackedApplications should be data");
        let tracked = Value::from_reader(std::io::Cursor::new(tracked))
            .expect("trackedApplications should parse");
        let rendered = format!("{tracked:?}");
        assert!(!rendered.to_lowercase().contains("paneru"));
        assert!(rendered.contains("com.example.keep"));
        assert!(rendered.contains("com.example.host"));
        assert!(rendered.contains("com.example.keep-menu"));

        let _ = fs::remove_file(path);
    }

    fn control_center_plist(items: Vec<Value>) -> Value {
        let mut tracked = Vec::new();
        Value::Array(items)
            .to_writer_binary(&mut tracked)
            .expect("nested plist should be writable");

        let mut root = Dictionary::new();
        root.insert("trackedApplications".to_owned(), Value::Data(tracked));
        Value::Dictionary(root)
    }

    fn bundle_item(bundle_id: &str) -> Value {
        let mut inner = Dictionary::new();
        inner.insert("_0".to_owned(), Value::String(bundle_id.to_owned()));

        let mut item = Dictionary::new();
        item.insert("bundle".to_owned(), Value::Dictionary(inner));
        Value::Dictionary(item)
    }

    fn adhoc_item(path: &str) -> Value {
        let mut relative = Dictionary::new();
        relative.insert("relative".to_owned(), Value::String(path.to_owned()));

        let mut inner = Dictionary::new();
        inner.insert("_0".to_owned(), Value::Dictionary(relative));

        let mut item = Dictionary::new();
        item.insert("adhocBinary".to_owned(), Value::Dictionary(inner));
        Value::Dictionary(item)
    }

    fn menu_item(location_bundle_id: &str, menu_item_locations: Vec<Value>) -> Value {
        let mut item = Dictionary::new();
        item.insert("isAllowed".to_owned(), Value::Boolean(true));
        item.insert("location".to_owned(), bundle_item(location_bundle_id));
        item.insert(
            "menuItemLocations".to_owned(),
            Value::Array(menu_item_locations),
        );
        Value::Dictionary(item)
    }
}
