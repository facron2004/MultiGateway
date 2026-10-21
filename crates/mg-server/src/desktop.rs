use mg_core::Config;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DesktopSettings {
    pub auto_start: bool,
    pub start_silently: bool,
    pub close_to_tray: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            auto_start: false,
            start_silently: false,
            close_to_tray: true,
        }
    }
}

pub fn load(data_dir: &Path) -> io::Result<DesktopSettings> {
    match fs::read(data_dir.join("desktop-settings.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DesktopSettings::default()),
        Err(error) => Err(error),
    }
}

fn startup_command(config: &Config, executable: &Path) -> io::Result<String> {
    let data_dir = std::path::absolute(&config.data_dir)?;
    Ok(format!(
        "\"{}\" --host \"{}\" --port {} --data-dir \"{}\"",
        executable.display(),
        config.host,
        config.port,
        data_dir.display()
    ))
}

pub fn save(config: &Config, settings: &DesktopSettings) -> io::Result<()> {
    let previous = load(&config.data_dir)?;
    // Touch the registry only when autostart actually changed: a registry failure
    // (or a platform without support) must not block saving unrelated settings.
    let autostart_changed = settings.auto_start != previous.auto_start;
    if autostart_changed {
        set_auto_start(config, settings.auto_start)?;
    }
    let path = config.data_dir.join("desktop-settings.json");
    let temporary = config.data_dir.join("desktop-settings.json.tmp");
    let result = fs::write(&temporary, serde_json::to_vec_pretty(settings)?)
        .and_then(|_| fs::rename(&temporary, &path));
    if result.is_err() && autostart_changed {
        let _ = set_auto_start(config, previous.auto_start);
    }
    result
}

#[cfg(windows)]
fn set_auto_start(config: &Config, enabled: bool) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
            HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        },
    };
    let wide = |text: &str| {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let value_name = wide("MultiGatewayRust");
    let command = startup_command(config, &std::env::current_exe()?)?;
    if enabled && command.encode_utf16().count() > 260 {
        return Err(io::Error::other(
            "开机启动命令过长，请将程序和数据移至较短路径",
        ));
    }
    let mut key = ptr::null_mut();
    unsafe {
        let status = if enabled {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_SET_VALUE, &mut key)
        };
        if !enabled && status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let status = if enabled {
            let bytes = wide(&command);
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr().cast(),
                (bytes.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(key, value_name.as_ptr())
        };
        RegCloseKey(key);
        if status == ERROR_SUCCESS || (!enabled && status == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

#[cfg(not(windows))]
fn set_auto_start(_: &Config, enabled: bool) -> io::Result<()> {
    if enabled {
        Err(io::Error::other("当前平台不支持开机启动"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{load, startup_command};
    use mg_core::Config;
    use std::path::Path;

    #[test]
    fn startup_command_preserves_executable_and_data_paths_with_spaces() {
        let config = Config {
            data_dir: Path::new(r"E:\User Data\Gateway").into(),
            port: 8792,
            ..Config::default()
        };
        let command = startup_command(&config, Path::new(r"E:\MG Rust\MultiGateway.exe")).unwrap();
        assert!(command.starts_with("\"E:\\MG Rust\\MultiGateway.exe\" "));
        assert!(command.contains("--port 8792"));
        assert!(command.ends_with("--data-dir \"E:\\User Data\\Gateway\""));
        let settings = load(&std::env::temp_dir().join(uuid::Uuid::new_v4().to_string())).unwrap();
        assert!(settings.close_to_tray);
        assert!(!settings.auto_start);
    }

    #[test]
    fn save_round_trips_without_touching_autostart_when_unchanged() {
        use super::{save, DesktopSettings};
        let dir = std::env::temp_dir().join(format!("mg-rust-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        // auto_start stays at its default (false), so the registry must not be
        // involved and the save cannot fail because of it.
        let settings = DesktopSettings {
            auto_start: false,
            start_silently: true,
            close_to_tray: false,
        };
        save(&config, &settings).unwrap();
        assert_eq!(load(&dir).unwrap(), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
