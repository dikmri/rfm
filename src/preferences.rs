use crate::{
    i18n::Language,
    quick_access,
    settings::{self, Appearance, IconTheme, Startup, StartupMode, Terminal},
    terminal,
};
use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    io,
    mem::{size_of, zeroed},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const CLASS_NAME: *const u16 = windows_sys::core::w!("rfm.preferences");
const ID_GROUP: i32 = 4100;
const ID_HOME: i32 = 4101;
const ID_LAST: i32 = 4102;
const ID_CUSTOM: i32 = 4103;
const ID_FOLDER: i32 = 4104;
const ID_BROWSE: i32 = 4105;
const ID_NOTICE: i32 = 4106;
const ID_TERMINAL_GROUP: i32 = 4107;
const ID_EXECUTABLE_LABEL: i32 = 4108;
const ID_EXECUTABLE: i32 = 4109;
const ID_EXECUTABLE_BROWSE: i32 = 4110;
const ID_ARGUMENTS_LABEL: i32 = 4111;
const ID_ARGUMENTS: i32 = 4112;
const ID_TERMINAL_HELP: i32 = 4113;
const ID_APPEARANCE_GROUP: i32 = 4114;
const ID_ICON_THEME_LABEL: i32 = 4115;
const ID_ICON_THEME: i32 = 4116;
const ID_APPEARANCE_NOTICE: i32 = 4117;
const ID_LANGUAGE_LABEL: i32 = 4118;
const ID_LANGUAGE: i32 = 4119;
const ID_LANGUAGE_NOTICE: i32 = 4120;
const CLIENT_WIDTH: i32 = 560;
const CLIENT_HEIGHT: i32 = 626;

struct Dialog {
    done: Cell<bool>,
    busy: Cell<bool>,
    mode: Cell<StartupMode>,
    default_button: Cell<u16>,
    dpi: Cell<u32>,
    base_dpi: u32,
    base_font: LOGFONTW,
    font: Cell<HFONT>,
}

impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font.get());
        }
    }
}

// Reentrant window messages only access Cells through a shared reference.
struct Busy<'a>(&'a Cell<bool>);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

struct ModalWindow {
    hwnd: HWND,
    owner: HWND,
    owner_was_enabled: bool,
}

impl Drop for ModalWindow {
    fn drop(&mut self) {
        unsafe {
            let active = GetActiveWindow() == self.hwnd;
            if self.owner_was_enabled && IsWindow(self.owner) != 0 {
                EnableWindow(self.owner, 1);
            }
            if IsWindow(self.hwnd) != 0 {
                DestroyWindow(self.hwnd);
            }
            if active && self.owner_was_enabled && IsWindow(self.owner) != 0 {
                SetActiveWindow(self.owner);
            }
        }
    }
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn edit_text(edit: HWND) -> OsString {
    unsafe {
        let mut value = vec![0; GetWindowTextLengthW(edit).max(0) as usize + 1];
        let length = GetWindowTextW(edit, value.as_mut_ptr(), value.len() as i32);
        OsString::from_wide(&value[..length.max(0) as usize])
    }
}

fn report(hwnd: HWND, message: &str) {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(message).as_ptr(),
            tr_w!("設定", "Settings"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn select_mode(hwnd: HWND, state: &Dialog, mode: StartupMode) {
    state.mode.set(mode);
    let selected = match mode {
        StartupMode::Home => ID_HOME,
        StartupMode::LastSession => ID_LAST,
        StartupMode::CustomFolder => ID_CUSTOM,
    };
    unsafe {
        CheckRadioButton(hwnd, ID_HOME, ID_CUSTOM, selected);
        let enabled = i32::from(mode == StartupMode::CustomFolder);
        EnableWindow(GetDlgItem(hwnd, ID_FOLDER), enabled);
        EnableWindow(GetDlgItem(hwnd, ID_BROWSE), enabled);
    }
}

fn accept(hwnd: HWND, state: &Dialog) {
    let edit = unsafe { GetDlgItem(hwnd, ID_FOLDER) };
    let folder = PathBuf::from(edit_text(edit));
    if state.mode.get() == StartupMode::CustomFolder {
        let error = if folder.as_os_str().is_empty() {
            Some(
                tr!(
                    "起動時に開くフォルダーを入力してください。",
                    "Enter the folder to open at startup."
                )
                .to_owned(),
            )
        } else if !folder.is_absolute() {
            Some(tr!("指定フォルダーには絶対パスを入力してください。\n例: C:\\Users\\ユーザー名\\Documents", "Enter an absolute path for the custom folder.\nExample: C:\\Users\\username\\Documents").to_owned())
        } else {
            match std::fs::metadata(&folder) {
                Ok(metadata) if metadata.is_dir() => None,
                Ok(_) => Some(
                    tr!(
                        "指定されたパスはフォルダーではありません。フォルダーを指定してください。",
                        "The specified path is not a folder. Select a folder."
                    )
                    .to_owned(),
                ),
                Err(error) => Some(tr_format!(
                    "指定フォルダーが存在しないか、アクセスできません。\n\n{error}",
                    "The custom folder does not exist or cannot be accessed.\n\n{error}"
                )),
            }
        };
        if let Some(error) = error {
            report(hwnd, &error);
            unsafe {
                SetFocus(edit);
            }
            return;
        }
    }
    let startup = Startup {
        mode: state.mode.get(),
        custom_folder: folder,
    };
    let config = Terminal {
        executable: PathBuf::from(edit_text(unsafe { GetDlgItem(hwnd, ID_EXECUTABLE) })),
        arguments: edit_text(unsafe { GetDlgItem(hwnd, ID_ARGUMENTS) }),
    };
    if let Err(error) = terminal::validate(&config) {
        report(
            hwnd,
            &tr_format!(
                "ターミナルの設定を確認してください。\n\n{error}",
                "Check the terminal settings.\n\n{error}"
            ),
        );
        unsafe {
            SetFocus(GetDlgItem(hwnd, ID_EXECUTABLE));
        }
        return;
    }
    let icon_theme =
        match unsafe { SendMessageW(GetDlgItem(hwnd, ID_ICON_THEME), CB_GETCURSEL, 0, 0) } {
            1 => IconTheme::Lucide,
            2 => IconTheme::Tabler,
            _ => IconTheme::Windows,
        };
    let language = match unsafe { SendMessageW(GetDlgItem(hwnd, ID_LANGUAGE), CB_GETCURSEL, 0, 0) }
    {
        1 => Language::Japanese,
        2 => Language::English,
        _ => Language::System,
    };
    let appearance = Appearance {
        icon_theme,
        language,
    };
    match settings::save_preferences(&startup, &config, &appearance) {
        Ok(()) => state.done.set(true),
        Err(error) => report(
            hwnd,
            &tr_format!(
                "設定を保存できませんでした。入力内容は保持されています。\n\n{error}",
                "Could not save the settings. Your entries have been kept.\n\n{error}"
            ),
        ),
    }
}

fn browse(hwnd: HWND) {
    match quick_access::pick_folder(hwnd) {
        Ok(Some(folder)) => unsafe {
            let edit = GetDlgItem(hwnd, ID_FOLDER);
            SetWindowTextW(edit, wide(folder.as_os_str()).as_ptr());
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL, 0, -1);
        },
        Ok(None) => {}
        Err(error) => report(
            hwnd,
            &tr_format!(
                "フォルダーを選択できませんでした。\n\n{error}",
                "Could not select a folder.\n\n{error}"
            ),
        ),
    }
}

fn browse_executable(hwnd: HWND) {
    match quick_access::pick_executable(hwnd) {
        Ok(Some(executable)) => unsafe {
            let edit = GetDlgItem(hwnd, ID_EXECUTABLE);
            SetWindowTextW(edit, wide(executable.as_os_str()).as_ptr());
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL, 0, -1);
        },
        Ok(None) => {}
        Err(error) => report(
            hwnd,
            &tr_format!(
                "ターミナルの実行ファイルを選択できませんでした。\n\n{error}",
                "Could not select the terminal executable.\n\n{error}"
            ),
        ),
    }
}

fn layout(hwnd: HWND, state: &Dialog) {
    let scale = |n: i32| (i64::from(n) * i64::from(state.dpi.get()) / 96) as i32;
    for (id, x, y, width, height) in [
        (ID_GROUP, 14, 12, 532, 188),
        (ID_HOME, 28, 34, 504, 24),
        (ID_LAST, 28, 62, 504, 24),
        (ID_CUSTOM, 28, 90, 504, 24),
        (ID_FOLDER, 48, 122, 390, 26),
        (ID_BROWSE, 448, 121, 84, 28),
        (ID_NOTICE, 28, 158, 504, 36),
        (ID_TERMINAL_GROUP, 14, 210, 532, 184),
        (ID_EXECUTABLE_LABEL, 28, 230, 504, 20),
        (ID_EXECUTABLE, 28, 252, 410, 26),
        (ID_EXECUTABLE_BROWSE, 448, 251, 84, 28),
        (ID_ARGUMENTS_LABEL, 28, 282, 504, 20),
        (ID_ARGUMENTS, 28, 304, 504, 26),
        (ID_TERMINAL_HELP, 28, 338, 504, 48),
        (ID_APPEARANCE_GROUP, 14, 404, 532, 164),
        (ID_ICON_THEME_LABEL, 28, 430, 76, 24),
        (ID_ICON_THEME, 112, 426, 420, 130),
        (ID_APPEARANCE_NOTICE, 28, 458, 504, 24),
        (ID_LANGUAGE_LABEL, 28, 490, 76, 24),
        (ID_LANGUAGE, 112, 486, 420, 130),
        (ID_LANGUAGE_NOTICE, 28, 522, 504, 36),
        (IDOK, 350, 582, 90, 30),
        (IDCANCEL, 450, 582, 90, 30),
    ] {
        unsafe {
            let child = GetDlgItem(hwnd, id);
            SetWindowPos(
                child,
                null_mut(),
                scale(x),
                scale(y),
                scale(width),
                scale(height),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            SendMessageW(child, WM_SETFONT, state.font.get() as usize, 1);
        }
    }
}

fn change_dpi(hwnd: HWND, state: &Dialog, dpi: u32, rect: &RECT) {
    state.dpi.set(dpi.max(96));
    let scale =
        |n: i32| (i64::from(n) * i64::from(state.dpi.get()) / i64::from(state.base_dpi)) as i32;
    let mut font = state.base_font;
    font.lfHeight = scale(font.lfHeight);
    font.lfWidth = scale(font.lfWidth);
    unsafe {
        let next_font = CreateFontIndirectW(&font);
        let previous = if next_font.is_null() {
            null_mut()
        } else {
            state.font.replace(next_font)
        };
        SetWindowPos(
            hwnd,
            null_mut(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        layout(hwnd, state);
        if !previous.is_null() {
            DeleteObject(previous);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Dialog;
        if !ptr.is_null() {
            let state = &*ptr;
            match message {
                WM_COMMAND => {
                    let id = (w & 0xffff) as i32;
                    let notification = ((w >> 16) & 0xffff) as u32;
                    if notification == BN_CLICKED && !state.busy.replace(true) {
                        let _busy = Busy(&state.busy);
                        match id {
                            ID_HOME => select_mode(hwnd, state, StartupMode::Home),
                            ID_LAST => select_mode(hwnd, state, StartupMode::LastSession),
                            ID_CUSTOM => select_mode(hwnd, state, StartupMode::CustomFolder),
                            ID_BROWSE => browse(hwnd),
                            ID_EXECUTABLE_BROWSE => browse_executable(hwnd),
                            IDOK => accept(hwnd, state),
                            IDCANCEL => state.done.set(true),
                            _ => {}
                        }
                    }
                    return 0;
                }
                DM_GETDEFID => {
                    return (usize::from(state.default_button.get())
                        | ((DC_HASDEFID as usize) << 16)) as LRESULT;
                }
                DM_SETDEFID => {
                    let id = w as u16;
                    if [IDOK, IDCANCEL, ID_BROWSE, ID_EXECUTABLE_BROWSE].contains(&i32::from(id)) {
                        let previous = state.default_button.replace(id);
                        if previous != id {
                            SendMessageW(
                                GetDlgItem(hwnd, i32::from(previous)),
                                BM_SETSTYLE,
                                BS_PUSHBUTTON as usize,
                                1,
                            );
                            SendMessageW(
                                GetDlgItem(hwnd, i32::from(id)),
                                BM_SETSTYLE,
                                BS_DEFPUSHBUTTON as usize,
                                1,
                            );
                        }
                    }
                    return 1;
                }
                WM_DPICHANGED => {
                    change_dpi(hwnd, state, (w & 0xffff) as u32, &*(l as *const RECT));
                    return 0;
                }
                WM_CLOSE => {
                    if !state.busy.get() {
                        state.done.set(true);
                    }
                    return 0;
                }
                WM_DESTROY => state.done.set(true),
                WM_NCDESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

/// Displays preferences without saving canceled edits.
/// Language selection is persisted only; it takes effect on the next application start.
pub fn show(owner: HWND, font: HFONT, current_folder: &Path) -> io::Result<()> {
    let settings = settings::load()?;
    let startup = settings.startup;
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: LoadIconW(instance, 1 as _),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
            lpszClassName: CLASS_NAME,
            ..zeroed()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(io::Error::last_os_error());
        }
        let mut base_font: LOGFONTW = zeroed();
        if GetObjectW(
            font,
            size_of::<LOGFONTW>() as i32,
            (&mut base_font as *mut LOGFONTW).cast(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let dialog_font = CreateFontIndirectW(&base_font);
        if dialog_font.is_null() {
            return Err(io::Error::last_os_error());
        }
        let dpi = GetDpiForWindow(owner).max(96);
        let state = Dialog {
            done: Cell::new(false),
            busy: Cell::new(false),
            mode: Cell::new(startup.mode),
            default_button: Cell::new(IDOK as u16),
            dpi: Cell::new(dpi),
            base_dpi: dpi,
            base_font,
            font: Cell::new(dialog_font),
        };
        let scale = |n: i32| (i64::from(n) * i64::from(dpi) / 96) as i32;
        let style = WS_CAPTION | WS_SYSMENU | WS_POPUP;
        let ex_style = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
        let mut bounds = RECT {
            left: 0,
            top: 0,
            right: scale(CLIENT_WIDTH),
            bottom: scale(CLIENT_HEIGHT),
        };
        if AdjustWindowRectExForDpi(&mut bounds, style, 0, ex_style, dpi) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut parent_rect: RECT = zeroed();
        if GetWindowRect(owner, &mut parent_rect) == 0 {
            return Err(io::Error::last_os_error());
        }
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        let mut x = parent_rect.left + (parent_rect.right - parent_rect.left - width) / 2;
        let mut y = parent_rect.top + (parent_rect.bottom - parent_rect.top - height) / 2;
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..zeroed()
        };
        if GetMonitorInfoW(
            MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        ) != 0
        {
            x = x.clamp(
                monitor.rcWork.left,
                (monitor.rcWork.right - width).max(monitor.rcWork.left),
            );
            y = y.clamp(
                monitor.rcWork.top,
                (monitor.rcWork.bottom - height).max(monitor.rcWork.top),
            );
        }
        let hwnd = CreateWindowExW(
            ex_style,
            CLASS_NAME,
            tr_w!("設定", "Settings"),
            style,
            x,
            y,
            width,
            height,
            owner,
            null_mut(),
            instance,
            (&state as *const Dialog).cast(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        let modal = ModalWindow {
            hwnd,
            owner,
            owner_was_enabled: IsWindowEnabled(owner) != 0,
        };
        EnableWindow(owner, 0);
        let initial_folder = if startup.custom_folder.as_os_str().is_empty() {
            current_folder
        } else {
            &startup.custom_folder
        };
        let folder_label = wide(initial_folder.as_os_str());
        let executable_label = wide(settings.terminal.executable.as_os_str());
        let arguments_label = wide(&settings.terminal.arguments);
        for (id, class, label, style, ex_style) in [
            (
                ID_GROUP,
                windows_sys::core::w!("BUTTON"),
                tr_w!("起動時に開く場所", "Startup folder"),
                BS_GROUPBOX as u32,
                0,
            ),
            (
                ID_HOME,
                windows_sys::core::w!("BUTTON"),
                tr_w!("ホーム（既定）(&H)", "&Home (default)"),
                WS_GROUP | WS_TABSTOP | BS_AUTORADIOBUTTON as u32,
                0,
            ),
            (
                ID_LAST,
                windows_sys::core::w!("BUTTON"),
                tr_w!(
                    "前回終了時のタブをすべて復元(&L)",
                    "Restore all tabs from the &last session"
                ),
                WS_TABSTOP | BS_AUTORADIOBUTTON as u32,
                0,
            ),
            (
                ID_CUSTOM,
                windows_sys::core::w!("BUTTON"),
                tr_w!("指定フォルダー(&F)", "Custom &folder"),
                WS_TABSTOP | BS_AUTORADIOBUTTON as u32,
                0,
            ),
            (
                ID_FOLDER,
                windows_sys::core::w!("EDIT"),
                folder_label.as_ptr(),
                WS_GROUP | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                WS_EX_CLIENTEDGE,
            ),
            (
                ID_BROWSE,
                windows_sys::core::w!("BUTTON"),
                tr_w!("参照(&B)…", "&Browse…"),
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
            ),
            (
                ID_NOTICE,
                windows_sys::core::w!("STATIC"),
                tr_w!(
                    "起動先の変更は次回起動から適用されます。\r\nフォルダーを指定して起動した場合は、その指定が優先されます。",
                    "Startup changes apply the next time rfm starts.\r\nA folder specified on the command line takes priority."
                ),
                0,
                0,
            ),
            (
                ID_TERMINAL_GROUP,
                windows_sys::core::w!("BUTTON"),
                tr_w!("ターミナル", "Terminal"),
                BS_GROUPBOX as u32,
                0,
            ),
            (
                ID_EXECUTABLE_LABEL,
                windows_sys::core::w!("STATIC"),
                tr_w!("実行ファイル (.exe)(&E)", "&Executable (.exe)"),
                0,
                0,
            ),
            (
                ID_EXECUTABLE,
                windows_sys::core::w!("EDIT"),
                executable_label.as_ptr(),
                WS_GROUP | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                WS_EX_CLIENTEDGE,
            ),
            (
                ID_EXECUTABLE_BROWSE,
                windows_sys::core::w!("BUTTON"),
                tr_w!("参照(&R)…", "B&rowse…"),
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
            ),
            (
                ID_ARGUMENTS_LABEL,
                windows_sys::core::w!("STATIC"),
                tr_w!("起動引数(&A)", "&Arguments"),
                0,
                0,
            ),
            (
                ID_ARGUMENTS,
                windows_sys::core::w!("EDIT"),
                arguments_label.as_ptr(),
                WS_TABSTOP | ES_AUTOHSCROLL as u32,
                WS_EX_CLIENTEDGE,
            ),
            (
                ID_TERMINAL_HELP,
                windows_sys::core::w!("STATIC"),
                tr_w!(
                    "{folder} は選択フォルダー（未選択時は現在地）に置換されます。\r\nAlacritty の例: --working-directory \"{folder}\"\r\n保存後、次のターミナル操作から適用されます。",
                    "{folder}: selected folder, or current folder if none is selected.\r\nAlacritty example: --working-directory \"{folder}\"\r\nApplies to the next terminal action after saving."
                ),
                0,
                0,
            ),
            (
                ID_APPEARANCE_GROUP,
                windows_sys::core::w!("BUTTON"),
                tr_w!("外観", "Appearance"),
                BS_GROUPBOX as u32,
                0,
            ),
            (
                ID_ICON_THEME_LABEL,
                windows_sys::core::w!("STATIC"),
                tr_w!("アイコン(&I)", "&Icons"),
                0,
                0,
            ),
            (
                ID_ICON_THEME,
                windows_sys::core::w!("COMBOBOX"),
                windows_sys::core::w!(""),
                WS_GROUP | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                0,
            ),
            (
                ID_APPEARANCE_NOTICE,
                windows_sys::core::w!("STATIC"),
                tr_w!(
                    "アイコンは保存後すぐに適用され、次回起動時も維持されます。",
                    "Icons apply immediately after saving and persist across restarts."
                ),
                0,
                0,
            ),
            (
                ID_LANGUAGE_LABEL,
                windows_sys::core::w!("STATIC"),
                tr_w!("言語(&G)", "Lan&guage"),
                0,
                0,
            ),
            (
                ID_LANGUAGE,
                windows_sys::core::w!("COMBOBOX"),
                windows_sys::core::w!(""),
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                0,
            ),
            (
                ID_LANGUAGE_NOTICE,
                windows_sys::core::w!("STATIC"),
                tr_w!(
                    "言語の変更は次回起動から適用されます。",
                    "Language changes take effect the next time rfm starts."
                ),
                0,
                0,
            ),
            (
                IDOK,
                windows_sys::core::w!("BUTTON"),
                tr_w!("OK", "OK"),
                WS_GROUP | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                0,
            ),
            (
                IDCANCEL,
                windows_sys::core::w!("BUTTON"),
                tr_w!("キャンセル(&C)", "&Cancel"),
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
            ),
        ] {
            let child = CreateWindowExW(
                ex_style,
                class,
                label,
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                0,
                0,
                hwnd,
                id as usize as HMENU,
                instance,
                null(),
            );
            if child.is_null() {
                return Err(io::Error::last_os_error());
            }
        }
        for id in [ID_FOLDER, ID_EXECUTABLE, ID_ARGUMENTS] {
            SendMessageW(GetDlgItem(hwnd, id), EM_SETLIMITTEXT, 32767, 0);
        }
        let combo = GetDlgItem(hwnd, ID_ICON_THEME);
        for label in [
            tr_w!(
                "Windows標準（Explorer風）",
                "Windows default (Explorer style)"
            ),
            tr_w!("Lucide", "Lucide"),
            tr_w!("Tabler", "Tabler"),
        ] {
            let result = SendMessageW(combo, CB_ADDSTRING, 0, label as LPARAM);
            if result == CB_ERR as LRESULT || result == CB_ERRSPACE as LRESULT {
                return Err(io::Error::other(tr!(
                    "アイコンの選択肢を追加できませんでした。",
                    "Could not add the icon theme choices."
                )));
            }
        }
        let selected_theme = match settings.appearance.icon_theme {
            IconTheme::Windows => 0,
            IconTheme::Lucide => 1,
            IconTheme::Tabler => 2,
        };
        if SendMessageW(combo, CB_SETCURSEL, selected_theme, 0) == CB_ERR as LRESULT {
            return Err(io::Error::other(tr!(
                "アイコンの設定を選択できませんでした。",
                "Could not select the icon theme setting."
            )));
        }
        let language_combo = GetDlgItem(hwnd, ID_LANGUAGE);
        for label in [
            tr_w!(
                "自動（Windows の表示言語）",
                "Auto (Windows display language)"
            ),
            tr_w!("日本語", "日本語"),
            tr_w!("English", "English"),
        ] {
            let result = SendMessageW(language_combo, CB_ADDSTRING, 0, label as LPARAM);
            if result == CB_ERR as LRESULT || result == CB_ERRSPACE as LRESULT {
                return Err(io::Error::other(tr!(
                    "言語の選択肢を追加できませんでした。",
                    "Could not add the language choices."
                )));
            }
        }
        let selected_language = match settings.appearance.language {
            Language::System => 0,
            Language::Japanese => 1,
            Language::English => 2,
        };
        if SendMessageW(language_combo, CB_SETCURSEL, selected_language, 0) == CB_ERR as LRESULT {
            return Err(io::Error::other(tr!(
                "言語の設定を選択できませんでした。",
                "Could not select the language setting."
            )));
        }
        layout(hwnd, &state);
        select_mode(hwnd, &state, startup.mode);
        ShowWindow(hwnd, SW_SHOW);
        let selected = match startup.mode {
            StartupMode::Home => ID_HOME,
            StartupMode::LastSession => ID_LAST,
            StartupMode::CustomFolder => ID_CUSTOM,
        };
        SetFocus(GetDlgItem(hwnd, selected));
        let mut message: MSG = zeroed();
        let mut quit_code = None;
        let mut result = Ok(());
        while !state.done.get() {
            let status = GetMessageW(&mut message, null_mut(), 0, 0);
            if status == -1 {
                result = Err(io::Error::last_os_error());
                break;
            }
            if status == 0 {
                quit_code = Some(message.wParam as i32);
                break;
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        drop(modal);
        if let Some(code) = quit_code {
            PostQuitMessage(code);
        }
        result
    }
}
