//! Native INI preferences, with atomic Appearance writes and independent session sections.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{GetLastError, HWND, SetLastError};
use windows_sys::Win32::System::WindowsProgramming::{
    GetPrivateProfileStringW, GetPrivateProfileStructW, WritePrivateProfileSectionW,
    WritePrivateProfileStringW, WritePrivateProfileStructW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, SW_FORCEMINIMIZE, SW_HIDE, SW_MINIMIZE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWMINNOACTIVE, SW_SHOWNORMAL, SetWindowPlacement, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
};
use windows_sys::core::{PCWSTR, w};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartupMode {
    #[default]
    Home,
    LastFolder,
    CustomFolder,
}

#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub mode: StartupMode,
    pub custom_folder: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Terminal {
    pub executable: PathBuf,
    pub arguments: OsString,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("alacritty.exe"),
            arguments: OsString::from("--working-directory \"{folder}\""),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconTheme {
    #[default]
    Windows,
    Lucide,
    Tabler,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Appearance {
    pub icon_theme: IconTheme,
    pub language: crate::i18n::Language,
}

#[derive(Default)]
pub struct Settings {
    pub startup: Startup,
    pub terminal: Terminal,
    pub appearance: Appearance,
    pub last_folder: Option<PathBuf>,
    pub placement: Option<Placement>,
}

pub struct Placement(WINDOWPLACEMENT);

impl Placement {
    fn from_native(mut native: WINDOWPLACEMENT) -> Option<Self> {
        let rect = native.rcNormalPosition;
        if native.length != size_of::<WINDOWPLACEMENT>() as u32
            || rect.right.checked_sub(rect.left)? <= 0
            || rect.bottom.checked_sub(rect.top)? <= 0
        {
            return None;
        }
        let minimized = matches!(
            native.showCmd as i32,
            SW_SHOWMINIMIZED | SW_MINIMIZE | SW_SHOWMINNOACTIVE | SW_FORCEMINIMIZE
        );
        native.showCmd = if native.showCmd == SW_SHOWMAXIMIZED as u32
            || (minimized && native.flags & WPF_RESTORETOMAXIMIZED != 0)
        {
            SW_SHOWMAXIMIZED as u32
        } else {
            SW_SHOWNORMAL as u32
        };
        // Never restore minimization, a minimized icon position, or async flags.
        native.flags = 0;
        Some(Self(native))
    }

    pub fn restore_hidden(&self, hwnd: HWND) -> io::Result<()> {
        let mut native = self.0;
        native.showCmd = SW_HIDE as u32;
        // Keep native workspace coordinates. SetWindowPlacement also adjusts an
        // off-screen rectangle after a monitor or resolution change.
        unsafe {
            SetLastError(0);
            if SetWindowPlacement(hwnd, &native) == 0 {
                return Err(last_error(tr!(
                    "ウィンドウ位置の復元",
                    "Restoring the window position"
                )));
            }
        }
        Ok(())
    }

    pub fn show_command(&self) -> i32 {
        self.0.showCmd as i32
    }
}

pub fn startup_path(settings: &Settings, home: PathBuf) -> PathBuf {
    match settings.startup.mode {
        StartupMode::Home => home,
        StartupMode::LastFolder => settings.last_folder.clone().unwrap_or(home),
        StartupMode::CustomFolder if !settings.startup.custom_folder.as_os_str().is_empty() => {
            settings.startup.custom_folder.clone()
        }
        StartupMode::CustomFolder => home,
    }
}

pub fn load() -> io::Result<Settings> {
    load_at(&settings_path()?)
}

pub fn save_preferences(
    startup: &Startup,
    terminal: &Terminal,
    appearance: &Appearance,
) -> io::Result<()> {
    save_preferences_at(&settings_path()?, startup, terminal, appearance)
}

pub fn save_session(hwnd: HWND, folder: &Path) -> io::Result<()> {
    let mut native = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe {
        SetLastError(0);
        if GetWindowPlacement(hwnd, &mut native) == 0 {
            return Err(last_error(tr!(
                "ウィンドウ位置の取得",
                "Reading the window position"
            )));
        }
    }
    let placement = Placement::from_native(native).ok_or_else(|| {
        io::Error::other(tr!(
            "保存するウィンドウ位置が無効です。",
            "The window position to save is invalid."
        ))
    })?;
    save_session_at(&settings_path()?, folder, &placement)
}

fn settings_path() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(|profile| PathBuf::from(profile).join("AppData").join("Local"))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                tr!("LOCALAPPDATA と USERPROFILE が未設定のため、設定の保存先を決定できません。", "Cannot locate the settings directory because LOCALAPPDATA and USERPROFILE are not set."),
            )
        })?;
    std::path::absolute(local.join("rfm").join("settings.ini"))
}

fn wide_file(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path)?;
    let mut wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "設定ファイルのパスに NUL 文字が含まれています。",
                "The settings file path contains a NUL character."
            ),
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn last_error(operation: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    if code == 0 {
        io::Error::other(tr_format!(
            "{operation} に失敗しました。",
            "{operation} failed."
        ))
    } else {
        let source = io::Error::from_raw_os_error(code as i32);
        io::Error::new(
            source.kind(),
            format!("{operation}: {source} (Win32 {code})"),
        )
    }
}

fn prepare_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // The BOM makes the W profile APIs preserve UTF-16 rather than use ANSI.
    // Deny sharing until it is written so another instance cannot initialize
    // the same new file as ANSI while it is still empty.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(path)
    {
        Ok(mut file) => file.write_all(&[0xff, 0xfe]),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_value(file: &[u16], section: PCWSTR, key: PCWSTR, default: PCWSTR) -> io::Result<Vec<u16>> {
    let mut value = vec![0; 256];
    loop {
        let length = unsafe {
            GetPrivateProfileStringW(
                section,
                key,
                default,
                value.as_mut_ptr(),
                value.len() as u32,
                file.as_ptr(),
            )
        } as usize;
        if length < value.len() - 1 {
            value.truncate(length);
            return Ok(value);
        }
        if value.len() >= 32_768 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!(
                    "設定ファイルの値が長すぎます。",
                    "A settings file value is too long."
                ),
            ));
        }
        value.resize(value.len() * 2, 0);
    }
}

fn load_at(path: &Path) -> io::Result<Settings> {
    match File::open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(error),
    }
    let file = wide_file(path)?;
    let mode = read_value(&file, w!("Startup"), w!("Mode"), w!(""))?;
    let mode = if mode.iter().copied().eq("LastFolder".encode_utf16()) {
        StartupMode::LastFolder
    } else if mode.iter().copied().eq("CustomFolder".encode_utf16()) {
        StartupMode::CustomFolder
    } else {
        StartupMode::Home
    };
    let custom = read_value(&file, w!("Startup"), w!("CustomFolder"), w!(""))?;
    let last = read_value(&file, w!("Session"), w!("LastFolder"), w!(""))?;
    let executable = read_value(&file, w!("Terminal"), w!("Executable"), w!("alacritty.exe"))?;
    let arguments = read_value(
        &file,
        w!("Terminal"),
        w!("Arguments"),
        w!("--working-directory \"{folder}\""),
    )?;
    let icon_theme = read_value(&file, w!("Appearance"), w!("IconTheme"), w!("Windows"))?;
    let icon_theme = if icon_theme.iter().copied().eq("Lucide".encode_utf16()) {
        IconTheme::Lucide
    } else if icon_theme.iter().copied().eq("Tabler".encode_utf16()) {
        IconTheme::Tabler
    } else {
        IconTheme::Windows
    };
    let language = crate::i18n::Language::from_setting(&read_value(
        &file,
        w!("Appearance"),
        w!("Language"),
        w!("System"),
    )?);
    let mut native = WINDOWPLACEMENT::default();
    let placement = unsafe {
        GetPrivateProfileStructW(
            w!("Window"),
            w!("Placement"),
            (&mut native as *mut WINDOWPLACEMENT).cast(),
            size_of::<WINDOWPLACEMENT>() as u32,
            file.as_ptr(),
        )
    };
    Ok(Settings {
        startup: Startup {
            mode,
            custom_folder: PathBuf::from(OsString::from_wide(&custom)),
        },
        terminal: Terminal {
            executable: PathBuf::from(OsString::from_wide(&executable)),
            arguments: OsString::from_wide(&arguments),
        },
        appearance: Appearance {
            icon_theme,
            language,
        },
        last_folder: (!last.is_empty()).then(|| PathBuf::from(OsString::from_wide(&last))),
        placement: (placement != 0)
            .then(|| Placement::from_native(native))
            .flatten(),
    })
}

fn append_quoted_value(value: &mut Vec<u16>, text: &OsStr) -> io::Result<()> {
    value.push(b'"' as u16);
    for unit in text.encode_wide() {
        if matches!(unit, 0 | 10 | 13) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!(
                    "保存する設定値に NUL または改行文字が含まれています。",
                    "A settings value contains a NUL character or line break."
                ),
            ));
        }
        value.push(unit);
    }
    // Native reads remove these outer quotes without trimming path whitespace.
    value.extend([b'"' as u16, 0]);
    Ok(())
}

fn save_preferences_at(
    path: &Path,
    startup: &Startup,
    terminal: &Terminal,
    appearance: &Appearance,
) -> io::Result<()> {
    let mode = match startup.mode {
        StartupMode::Home => "Mode=Home\0CustomFolder=",
        StartupMode::LastFolder => "Mode=LastFolder\0CustomFolder=",
        StartupMode::CustomFolder => "Mode=CustomFolder\0CustomFolder=",
    };
    let mut section = Vec::with_capacity(mode.len() + startup.custom_folder.as_os_str().len() + 4);
    section.extend(mode.encode_utf16());
    append_quoted_value(&mut section, startup.custom_folder.as_os_str())?;
    section.push(0);
    // WritePrivateProfileSectionW limits the entire section to 65,535 bytes.
    if section.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "保存する起動設定が長すぎます。",
                "The startup settings are too long to save."
            ),
        ));
    }
    let mut terminal_section: Vec<u16> = "Executable=".encode_utf16().collect();
    append_quoted_value(&mut terminal_section, terminal.executable.as_os_str())?;
    terminal_section.extend("Arguments=".encode_utf16());
    append_quoted_value(&mut terminal_section, &terminal.arguments)?;
    terminal_section.push(0);
    if terminal_section.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "保存するターミナル設定が長すぎます。",
                "The terminal settings are too long to save."
            ),
        ));
    }
    use crate::i18n::Language;
    let appearance_section = match (appearance.icon_theme, appearance.language) {
        (IconTheme::Windows, Language::System) => w!("IconTheme=Windows\0Language=System\0"),
        (IconTheme::Windows, Language::Japanese) => w!("IconTheme=Windows\0Language=Japanese\0"),
        (IconTheme::Windows, Language::English) => w!("IconTheme=Windows\0Language=English\0"),
        (IconTheme::Lucide, Language::System) => w!("IconTheme=Lucide\0Language=System\0"),
        (IconTheme::Lucide, Language::Japanese) => w!("IconTheme=Lucide\0Language=Japanese\0"),
        (IconTheme::Lucide, Language::English) => w!("IconTheme=Lucide\0Language=English\0"),
        (IconTheme::Tabler, Language::System) => w!("IconTheme=Tabler\0Language=System\0"),
        (IconTheme::Tabler, Language::Japanese) => w!("IconTheme=Tabler\0Language=Japanese\0"),
        (IconTheme::Tabler, Language::English) => w!("IconTheme=Tabler\0Language=English\0"),
    };
    let file = wide_file(path)?;
    prepare_file(path)?;
    unsafe {
        SetLastError(0);
        // A section write is atomic and never replaces Session or Window.
        if WritePrivateProfileSectionW(w!("Startup"), section.as_ptr(), file.as_ptr()) == 0 {
            return Err(last_error(tr!(
                "起動設定の保存",
                "Saving the startup settings"
            )));
        }
        SetLastError(0);
        if WritePrivateProfileSectionW(w!("Terminal"), terminal_section.as_ptr(), file.as_ptr())
            == 0
        {
            return Err(last_error(tr!(
                "ターミナル設定の保存",
                "Saving the terminal settings"
            )));
        }
        SetLastError(0);
        if WritePrivateProfileSectionW(w!("Appearance"), appearance_section, file.as_ptr()) == 0 {
            return Err(last_error(tr!(
                "外観設定の保存",
                "Saving the appearance settings"
            )));
        }
    }
    Ok(())
}

fn save_session_at(path: &Path, folder: &Path, placement: &Placement) -> io::Result<()> {
    let mut value = Vec::with_capacity(folder.as_os_str().len() + 3);
    append_quoted_value(&mut value, folder.as_os_str())?;
    if value.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "保存するフォルダーのパスが長すぎます。",
                "The folder path is too long to save."
            ),
        ));
    }
    let file = wide_file(path)?;
    prepare_file(path)?;
    unsafe {
        SetLastError(0);
        if WritePrivateProfileStringW(
            w!("Session"),
            w!("LastFolder"),
            value.as_ptr(),
            file.as_ptr(),
        ) == 0
        {
            return Err(last_error(tr!(
                "前回のフォルダーの保存",
                "Saving the last folder"
            )));
        }
        SetLastError(0);
        if WritePrivateProfileStructW(
            w!("Window"),
            w!("Placement"),
            (&placement.0 as *const WINDOWPLACEMENT).cast(),
            size_of::<WINDOWPLACEMENT>() as u32,
            file.as_ptr(),
        ) == 0
        {
            return Err(last_error(tr!(
                "ウィンドウ位置の保存",
                "Saving the window position"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;
    use windows_sys::Win32::Foundation::RECT;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "rfm-settings-{}-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("temporary directory: {error}"),
                }
            }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn native_placement(show: i32, flags: u32) -> WINDOWPLACEMENT {
        WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            showCmd: show as u32,
            flags,
            rcNormalPosition: RECT {
                left: -1600,
                top: -200,
                right: -400,
                bottom: 600,
            },
            ..Default::default()
        }
    }

    #[test]
    fn missing_settings_and_empty_destinations_use_home() {
        let temp = Temp::new();
        let mut settings = load_at(&temp.0.join("missing.ini")).unwrap();
        let home = PathBuf::from(r"C:\home");
        assert_eq!(settings.startup.mode, StartupMode::Home);
        assert_eq!(settings.appearance.language, crate::i18n::Language::System);
        assert!(settings.placement.is_none());
        assert_eq!(startup_path(&settings, home.clone()), home);
        settings.startup.mode = StartupMode::LastFolder;
        assert_eq!(startup_path(&settings, home.clone()), home);
        settings.startup.mode = StartupMode::CustomFolder;
        assert_eq!(startup_path(&settings, home.clone()), home);
        let unavailable = PathBuf::from(r"Z:\未接続\folder");
        settings.startup.custom_folder = unavailable.clone();
        assert_eq!(startup_path(&settings, home.clone()), unavailable);
        settings.startup.mode = StartupMode::LastFolder;
        settings.last_folder = Some(unavailable.clone());
        assert_eq!(startup_path(&settings, home), unavailable);
    }

    #[test]
    fn unicode_paths_round_trip_without_utf16_loss() {
        let temp = Temp::new();
        let file = temp.0.join("日本語の設定.ini");
        let mut units: Vec<u16> = r"C:\日本語\🙂\".encode_utf16().collect();
        units.extend([0xd800, b' ' as u16]);
        let folder = PathBuf::from(OsString::from_wide(&units));
        let startup = Startup {
            mode: StartupMode::CustomFolder,
            custom_folder: folder.clone(),
        };
        let terminal = Terminal {
            executable: folder.join("terminal.exe"),
            arguments: OsString::from_wide(
                &r#"--title "日本語 & title" --working-directory "{folder}" "#
                    .encode_utf16()
                    .chain([0xd800])
                    .collect::<Vec<_>>(),
            ),
        };
        save_preferences_at(&file, &startup, &terminal, &Appearance::default()).unwrap();
        let placement = Placement::from_native(native_placement(SW_SHOWNORMAL, 0)).unwrap();
        save_session_at(&file, &folder, &placement).unwrap();
        let loaded = load_at(&file).unwrap();
        assert_eq!(loaded.startup.mode, startup.mode);
        assert_eq!(loaded.startup.custom_folder, folder);
        assert_eq!(loaded.last_folder.as_ref(), Some(&folder));
        assert_eq!(loaded.terminal.executable, terminal.executable);
        assert_eq!(loaded.terminal.arguments, terminal.arguments);
    }

    #[test]
    fn startup_and_session_writes_preserve_each_other_in_either_order() {
        let temp = Temp::new();
        let file = temp.0.join("settings.ini");
        let mut startup = Startup {
            mode: StartupMode::LastFolder,
            custom_folder: PathBuf::from(r"C:\指定フォルダー"),
        };
        let placement = Placement::from_native(native_placement(SW_SHOWMAXIMIZED, 0)).unwrap();
        let first = Path::new(r"D:\前回");
        let second = Path::new(r"E:\今回");
        save_session_at(&file, first, &placement).unwrap();
        save_preferences_at(
            &file,
            &startup,
            &Terminal::default(),
            &Appearance::default(),
        )
        .unwrap();
        let loaded = load_at(&file).unwrap();
        assert_eq!(loaded.last_folder.as_deref(), Some(first));
        assert_eq!(loaded.placement.unwrap().show_command(), SW_SHOWMAXIMIZED);
        save_session_at(&file, second, &placement).unwrap();
        let loaded = load_at(&file).unwrap();
        assert_eq!(loaded.startup.mode, StartupMode::LastFolder);
        assert_eq!(loaded.startup.custom_folder, startup.custom_folder);
        startup.mode = StartupMode::Home;
        save_preferences_at(
            &file,
            &startup,
            &Terminal::default(),
            &Appearance::default(),
        )
        .unwrap();
        let loaded = load_at(&file).unwrap();
        assert_eq!(loaded.startup.mode, StartupMode::Home);
        assert_eq!(loaded.last_folder.as_deref(), Some(second));
        assert_eq!(loaded.placement.unwrap().0.rcNormalPosition.left, -1600);
    }

    #[test]
    fn placement_validates_rectangles_and_never_restores_minimized() {
        for (show, flags, expected) in [
            (SW_SHOWNORMAL, 0, SW_SHOWNORMAL),
            (SW_SHOWMAXIMIZED, 0, SW_SHOWMAXIMIZED),
            (SW_SHOWMINIMIZED, 0, SW_SHOWNORMAL),
            (SW_SHOWMINIMIZED, WPF_RESTORETOMAXIMIZED, SW_SHOWMAXIMIZED),
        ] {
            let placement = Placement::from_native(native_placement(show, flags)).unwrap();
            assert_eq!(placement.show_command(), expected);
            assert_eq!(placement.0.rcNormalPosition.left, -1600);
        }
        let mut invalid = native_placement(SW_SHOWNORMAL, 0);
        invalid.length = 0;
        assert!(Placement::from_native(invalid).is_none());
        invalid.length = size_of::<WINDOWPLACEMENT>() as u32;
        invalid.rcNormalPosition.right = invalid.rcNormalPosition.left;
        assert!(Placement::from_native(invalid).is_none());
        invalid.rcNormalPosition.left = i32::MIN;
        invalid.rcNormalPosition.right = i32::MAX;
        assert!(Placement::from_native(invalid).is_none());
    }

    #[test]
    fn adding_terminal_settings_preserves_legacy_startup_and_empty_arguments() {
        let temp = Temp::new();
        let file = temp.0.join("old-settings.ini");
        let legacy = "[Startup]\r\nMode=LastFolder\r\nCustomFolder=\"C:\\custom\"\r\n[Session]\r\nLastFolder=\"D:\\previous\"\r\n";
        let encoded: Vec<u8> = std::iter::once(0xfeffu16)
            .chain(legacy.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        fs::write(&file, encoded).unwrap();
        let old = load_at(&file).unwrap();
        assert_eq!(old.startup.mode, StartupMode::LastFolder);
        assert_eq!(old.last_folder.as_deref(), Some(Path::new(r"D:\previous")));
        assert_eq!(old.terminal.executable, Path::new("alacritty.exe"));
        assert_eq!(old.appearance.icon_theme, IconTheme::Windows);
        assert_eq!(old.appearance.language, crate::i18n::Language::System);
        let terminal = Terminal {
            executable: PathBuf::from(r"C:\別のターミナル\terminal.exe"),
            arguments: OsString::new(),
        };
        save_preferences_at(&file, &old.startup, &terminal, &old.appearance).unwrap();
        let placement = Placement::from_native(native_placement(SW_SHOWNORMAL, 0)).unwrap();
        save_session_at(&file, Path::new(r"E:\current"), &placement).unwrap();
        let loaded = load_at(&file).unwrap();
        assert_eq!(loaded.startup.mode, StartupMode::LastFolder);
        assert_eq!(loaded.startup.custom_folder, Path::new(r"C:\custom"));
        assert_eq!(loaded.terminal.executable, terminal.executable);
        assert!(loaded.terminal.arguments.is_empty());
        assert_eq!(
            loaded.last_folder.as_deref(),
            Some(Path::new(r"E:\current"))
        );
    }

    #[test]
    fn all_appearance_choices_round_trip_and_survive_session_saves() {
        let temp = Temp::new();
        let file = temp.0.join("appearance.ini");
        let startup = Startup {
            mode: StartupMode::CustomFolder,
            custom_folder: PathBuf::from(r"C:\起動先"),
        };
        let terminal = Terminal {
            executable: PathBuf::from(r"C:\端末\terminal.exe"),
            arguments: OsString::new(),
        };
        let folder = Path::new(r"D:\作業中");
        let placement = Placement::from_native(native_placement(SW_SHOWMAXIMIZED, 0)).unwrap();
        save_session_at(&file, folder, &placement).unwrap();
        for icon_theme in [IconTheme::Lucide, IconTheme::Tabler, IconTheme::Windows] {
            for language in [
                crate::i18n::Language::Japanese,
                crate::i18n::Language::English,
                crate::i18n::Language::System,
            ] {
                let appearance = Appearance {
                    icon_theme,
                    language,
                };
                save_preferences_at(&file, &startup, &terminal, &appearance).unwrap();
                let loaded = load_at(&file).unwrap();
                assert_eq!(loaded.appearance.icon_theme, icon_theme);
                assert_eq!(loaded.appearance.language, language);
                assert_eq!(loaded.last_folder.as_deref(), Some(folder));
                assert_eq!(loaded.placement.unwrap().show_command(), SW_SHOWMAXIMIZED);

                save_session_at(&file, folder, &placement).unwrap();
                let loaded = load_at(&file).unwrap();
                assert_eq!(loaded.appearance.icon_theme, icon_theme);
                assert_eq!(loaded.appearance.language, language);
                assert_eq!(loaded.startup.mode, startup.mode);
                assert_eq!(loaded.startup.custom_folder, startup.custom_folder);
                assert_eq!(loaded.terminal.executable, terminal.executable);
                assert!(loaded.terminal.arguments.is_empty());
            }
        }
    }

    #[test]
    fn missing_empty_and_unknown_language_preserve_legacy_icon_theme() {
        let temp = Temp::new();
        for (index, setting) in ["", "Language=\r\n", "Language=Unknown\r\n"]
            .into_iter()
            .enumerate()
        {
            let file = temp.0.join(format!("legacy-appearance-{index}.ini"));
            let contents = format!("[Appearance]\r\nIconTheme=Lucide\r\n{setting}");
            let encoded: Vec<u8> = std::iter::once(0xfeffu16)
                .chain(contents.encode_utf16())
                .flat_map(u16::to_le_bytes)
                .collect();
            fs::write(&file, encoded).unwrap();
            let loaded = load_at(&file).unwrap();
            assert_eq!(loaded.appearance.icon_theme, IconTheme::Lucide);
            assert_eq!(loaded.appearance.language, crate::i18n::Language::System);
        }
    }

    #[test]
    fn write_failures_are_reported() {
        let temp = Temp::new();
        assert!(
            save_preferences_at(
                &temp.0,
                &Startup::default(),
                &Terminal::default(),
                &Appearance::default(),
            )
            .is_err()
        );
    }
}
