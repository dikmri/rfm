use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND as NativeHwnd, PROPERTYKEY};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::{COMDLG_FILTERSPEC, ITEMIDLIST};
use windows::Win32::UI::Shell::PropertiesSystem::PSGetPropertyKeyFromName;
use windows::Win32::UI::Shell::{
    BHID_SFObject, FOS_ALLOWMULTISELECT, FOS_DONTADDTORECENT, FOS_FILEMUSTEXIST,
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    IShellDispatch, IShellFolder, IShellItem, IShellItem2, SEE_MASK_CLASSNAME, SEE_MASK_NOASYNC,
    SHCONTF_FOLDERS, SHCreateItemFromParsingName, SHCreateItemWithParent, SHELLEXECUTEINFOW,
    SICHINT_CANONICAL, SICHINT_TEST_FILESYSPATH_IF_NOT_EQUAL, SIGDN, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY, Shell, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HRESULT, Interface, PCWSTR, w};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

pub struct Pin {
    pub path: PathBuf,
    pub name: OsString,
    pub pinned: Option<bool>,
    pub image: i32,
}

fn shell_error(operation: &str, error: windows::core::Error) -> io::Error {
    io::Error::other(format!("{operation}: {error}"))
}

// Background shell workers own their apartments. Their shell interfaces must be
// released before this guard uninitializes OLE.
pub(crate) struct Apartment;

impl Apartment {
    pub(crate) fn sta() -> io::Result<Self> {
        unsafe { OleInitialize(None) }.map_err(|error| {
            shell_error(
                tr!(
                    "クイック アクセスの COM 初期化",
                    "Initialize Quick Access COM"
                ),
                error,
            )
        })?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn display_name(item: &IShellItem, kind: SIGDN) -> io::Result<OsString> {
    let text = unsafe { item.GetDisplayName(kind) }.map_err(|error| {
        shell_error(tr!("シェル項目の名前を取得", "Get shell item name"), error)
    })?;
    if text.is_null() {
        return Err(io::Error::other(tr!(
            "シェルから項目名が返されませんでした。",
            "The shell did not return an item name."
        )));
    }
    // GetDisplayName transfers a CoTaskMemAlloc string, including for non-UTF-8 names.
    let name = unsafe { OsString::from_wide(text.as_wide()) };
    unsafe { CoTaskMemFree(Some(text.0.cast())) };
    Ok(name)
}

fn filesystem_folder(item: &IShellItem) -> io::Result<bool> {
    let required = SFGAO_FILESYSTEM | SFGAO_FOLDER;
    let attributes = unsafe { item.GetAttributes(required) }.map_err(|error| {
        shell_error(
            tr!("シェル項目の属性を取得", "Get shell item attributes"),
            error,
        )
    })?;
    Ok(attributes.0 & required.0 == required.0)
}

fn path_item(path: &Path) -> io::Result<IShellItem> {
    let path = std::path::absolute(path)?;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "フォルダーのパスに NUL 文字が含まれています。",
                "The folder path contains a NUL character."
            ),
        ));
    }
    wide.push(0);
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
        .map_err(|error| {
            shell_error(
                tr!("フォルダーをシェルで開く", "Open folder in shell"),
                error,
            )
        })?;
    if !filesystem_folder(&item)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "ファイル システムのフォルダーを選択してください。",
                "Select a file system folder."
            ),
        ));
    }
    Ok(item)
}

struct Child {
    item: IShellItem,
}

struct QuickAccess {
    folder: IShellFolder,
    pinned_key: Option<PROPERTYKEY>,
}

impl QuickAccess {
    fn open() -> io::Result<Self> {
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(w!("shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}"), None)
        }
        .map_err(|error| shell_error(tr!("クイック アクセスを開く", "Open Quick Access"), error))?;
        let folder = unsafe { item.BindToHandler(None, &BHID_SFObject) }.map_err(|error| {
            shell_error(
                tr!(
                    "クイック アクセスのフォルダーを取得",
                    "Get Quick Access folder"
                ),
                error,
            )
        })?;
        let mut key = PROPERTYKEY::default();
        let pinned_key = unsafe { PSGetPropertyKeyFromName(w!("System.Home.IsPinned"), &mut key) }
            .ok()
            .map(|()| key);
        Ok(Self { folder, pinned_key })
    }

    fn children(&self) -> io::Result<Vec<Child>> {
        let mut enumerator = None;
        unsafe {
            self.folder.EnumObjects(
                NativeHwnd::default(),
                SHCONTF_FOLDERS.0 as u32,
                &mut enumerator,
            )
        }
        .ok()
        .map_err(|error| {
            shell_error(
                tr!("クイック アクセスを列挙", "Enumerate Quick Access"),
                error,
            )
        })?;
        // IShellFolder::EnumObjects may return S_FALSE and a null enumerator for
        // an empty namespace. This is distinct from an enumeration failure.
        let Some(enumerator) = enumerator else {
            return Ok(Vec::new());
        };
        let mut children = Vec::new();
        loop {
            let mut raw = [null_mut()];
            let mut fetched = 0;
            let result = unsafe { enumerator.Next(&mut raw, Some(&mut fetched)) };
            let pidl = Pidl(raw[0]);
            result.ok().map_err(|error| {
                shell_error(
                    tr!(
                        "クイック アクセスの項目を列挙",
                        "Enumerate Quick Access items"
                    ),
                    error,
                )
            })?;
            if fetched == 0 {
                break;
            }
            if pidl.0.is_null() {
                return Err(io::Error::other(tr!(
                    "クイック アクセスから無効な項目が返されました。",
                    "Quick Access returned an invalid item."
                )));
            }
            let item: IShellItem = unsafe { SHCreateItemWithParent(None, &self.folder, pidl.0) }
                .map_err(|error| {
                    shell_error(
                        tr!("クイック アクセスの項目を取得", "Get Quick Access item"),
                        error,
                    )
                })?;
            if filesystem_folder(&item)? {
                children.push(Child { item });
            }
        }
        Ok(children)
    }

    fn pinned(&self, child: &Child) -> Option<bool> {
        let key = self.pinned_key.as_ref()?;
        let item = child.item.cast::<IShellItem2>().ok()?;
        unsafe { item.GetBool(key) }
            .ok()
            .map(|pinned| pinned.as_bool())
    }

    fn find(&self, target: &IShellItem) -> io::Result<Option<Child>> {
        for child in self.children()? {
            let order = unsafe {
                child.item.Compare(
                    target,
                    (SICHINT_CANONICAL.0 | SICHINT_TEST_FILESYSPATH_IF_NOT_EQUAL.0) as u32,
                )
            }
            .map_err(|error| {
                shell_error(
                    tr!(
                        "クイック アクセスの項目を比較",
                        "Compare Quick Access items"
                    ),
                    error,
                )
            })?;
            if order == 0 {
                return Ok(Some(child));
            }
        }
        Ok(None)
    }
}

/// Enumerates Explorer's shared Quick Access, including pinned and frequent
/// filesystem folders, in the order supplied by the shell. Call from an
/// initialized background STA. Real folder icons are deliberately loaded
/// separately: a single shell icon lookup can block on filesystem or network I/O.
pub fn folders() -> io::Result<Vec<Pin>> {
    let quick_access = QuickAccess::open()?;
    let image = crate::shell::icon_index(Path::new("folder"), true);
    let mut folders = Vec::new();
    for child in quick_access.children()? {
        let path = PathBuf::from(display_name(&child.item, SIGDN_FILESYSPATH)?);
        let name = display_name(&child.item, SIGDN_NORMALDISPLAY)?;
        let pinned = quick_access.pinned(&child);
        folders.push(Pin {
            path,
            name,
            pinned,
            image,
        });
    }
    Ok(folders)
}

/// Pins real filesystem folders in Windows Quick Access. The calling GUI thread
/// must already have initialized COM/OLE as STA. Existing pins are not toggled.
/// The shell can publish its namespace change after this function returns.
pub fn pin(owner: HWND, paths: &[PathBuf]) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let quick_access = QuickAccess::open()?;
    for path in paths {
        let target = path_item(path)?;
        if let Some(child) = quick_access.find(&target)?
            && quick_access.pinned(&child) == Some(true)
        {
            continue;
        }
        let path = display_name(&target, SIGDN_FILESYSPATH)?;
        let mut path: Vec<u16> = path.encode_wide().collect();
        path.push(0);
        let mut command = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_CLASSNAME | SEE_MASK_NOASYNC,
            hwnd: NativeHwnd(owner),
            lpVerb: w!("pintohome"),
            lpFile: PCWSTR(path.as_ptr()),
            lpClass: w!("Folder"),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut command) }.map_err(|error| {
            shell_error(
                tr!("クイック アクセスにピン留め", "Pin to Quick Access"),
                error,
            )
        })?;
    }
    Ok(())
}

/// Removes only this Quick Access pin, never the directory. The calling GUI
/// thread must already have initialized COM/OLE as STA. An absent pin is a no-op.
/// Like pin(), the shell can finish updating its namespace after this returns.
pub fn unpin(_owner: HWND, path: &Path) -> io::Result<()> {
    let target = path_item(path)?;
    let quick_access = QuickAccess::open()?;
    let Some(child) = quick_access.find(&target)? else {
        return Ok(());
    };
    if quick_access.pinned(&child) == Some(false) {
        return Ok(());
    }
    let path = display_name(&child.item, SIGDN_FILESYSPATH)?;
    let path: Vec<u16> = path.encode_wide().collect();

    // Use the real namespace FolderItem, not a filesystem FolderItem or a
    // context menu assembled without Explorer's site. InvokeVerb initializes
    // the shell's command correctly; neither Verbs nor QueryContextMenu needs
    // to enumerate unrelated context-menu extensions.
    let shell: IShellDispatch = unsafe { CoCreateInstance(&Shell, None, CLSCTX_INPROC_SERVER) }
        .map_err(|error| {
            shell_error(
                tr!(
                    "クイック アクセスの操作を初期化",
                    "Initialize Quick Access operation"
                ),
                error,
            )
        })?;
    let namespace = unsafe {
        shell.NameSpace(&VARIANT::from(
            "shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}",
        ))
    }
    .map_err(|error| {
        shell_error(
            tr!(
                "クイック アクセスの操作対象を取得",
                "Get Quick Access operation target"
            ),
            error,
        )
    })?;
    let items = unsafe { namespace.Items() }.map_err(|error| {
        shell_error(
            tr!(
                "クイック アクセスの操作対象を列挙",
                "Enumerate Quick Access operation targets"
            ),
            error,
        )
    })?;
    let count = unsafe { items.Count() }.map_err(|error| {
        shell_error(
            tr!(
                "クイック アクセスの項目数を取得",
                "Get Quick Access item count"
            ),
            error,
        )
    })?;
    for index in 0..count {
        let item = unsafe { items.Item(&VARIANT::from(index)) }.map_err(|error| {
            shell_error(
                tr!(
                    "クイック アクセスの操作項目を取得",
                    "Get Quick Access operation item"
                ),
                error,
            )
        })?;
        let candidate = unsafe { item.Path() }.map_err(|error| {
            shell_error(
                tr!(
                    "クイック アクセスの操作項目のパスを取得",
                    "Get Quick Access operation item path"
                ),
                error,
            )
        })?;
        let same = unsafe {
            CompareStringOrdinal(
                path.as_ptr(),
                path.len() as i32,
                candidate.as_ptr(),
                candidate.len() as i32,
                1,
            )
        } == CSTR_EQUAL;
        if same {
            return unsafe { item.InvokeVerb(&VARIANT::from("unpinfromhome")) }.map_err(|error| {
                shell_error(
                    tr!(
                        "クイック アクセスのピン留めを外す",
                        "Unpin from Quick Access"
                    ),
                    error,
                )
            });
        }
    }
    // The namespace may have changed between the native and Automation
    // enumerations. Do not report success for a command that was never sent.
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        tr!(
            "クイック アクセスの項目が変更されました。更新してから再試行してください。",
            "Quick Access items have changed. Refresh and try again."
        ),
    ))
}

enum Picker {
    Folder,
    Folders,
    Executable,
}

fn selection_dialog(owner: HWND, picker: Picker) -> io::Result<Option<IFileOpenDialog>> {
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| {
                shell_error(
                    tr!("選択ダイアログを作成", "Create selection dialog"),
                    error,
                )
            },
        )?;
    let options = unsafe { dialog.GetOptions() }.map_err(|error| {
        shell_error(
            tr!("選択ダイアログの設定を取得", "Get selection dialog options"),
            error,
        )
    })?;
    let options = (options | FOS_FORCEFILESYSTEM | FOS_DONTADDTORECENT) & !FOS_ALLOWMULTISELECT;
    let options = match picker {
        Picker::Folder => options | FOS_PICKFOLDERS,
        Picker::Folders => options | FOS_PICKFOLDERS | FOS_ALLOWMULTISELECT,
        Picker::Executable => {
            let filters = [COMDLG_FILTERSPEC {
                pszName: PCWSTR(tr_w!("実行ファイル (*.exe)", "Executables (*.exe)")),
                pszSpec: w!("*.exe"),
            }];
            unsafe { dialog.SetFileTypes(&filters) }.map_err(|error| {
                shell_error(
                    tr!("実行ファイルのフィルターを設定", "Set executable filter"),
                    error,
                )
            })?;
            unsafe {
                dialog.SetTitle(PCWSTR(tr_w!(
                    "ターミナルの実行ファイルを選択",
                    "Select Terminal Executable"
                )))
            }
            .map_err(|error| {
                shell_error(
                    tr!(
                        "選択ダイアログのタイトルを設定",
                        "Set selection dialog title"
                    ),
                    error,
                )
            })?;
            (options | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST) & !FOS_PICKFOLDERS
        }
    };
    unsafe { dialog.SetOptions(options) }.map_err(|error| {
        shell_error(
            tr!("選択ダイアログを設定", "Set selection dialog options"),
            error,
        )
    })?;
    if let Err(error) = unsafe { dialog.Show(Some(NativeHwnd(owner))) } {
        if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            return Ok(None);
        }
        return Err(shell_error(
            tr!("選択ダイアログを表示", "Show selection dialog"),
            error,
        ));
    }
    Ok(Some(dialog))
}

/// Shows the standard multi-select folder picker. The calling GUI thread must
/// already have initialized COM/OLE as STA. Cancellation is an empty selection.
pub fn pick_folders(owner: HWND) -> io::Result<Vec<PathBuf>> {
    let Some(dialog) = selection_dialog(owner, Picker::Folders)? else {
        return Ok(Vec::new());
    };
    let selected = unsafe { dialog.GetResults() }.map_err(|error| {
        shell_error(
            tr!("選択されたフォルダーを取得", "Get selected folder"),
            error,
        )
    })?;
    let count = unsafe { selected.GetCount() }.map_err(|error| {
        shell_error(
            tr!("選択されたフォルダー数を取得", "Get selected folder count"),
            error,
        )
    })?;
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let item = unsafe { selected.GetItemAt(index) }.map_err(|error| {
            shell_error(
                tr!("選択されたフォルダーを取得", "Get selected folder"),
                error,
            )
        })?;
        paths.push(PathBuf::from(display_name(&item, SIGDN_FILESYSPATH)?));
    }
    Ok(paths)
}

/// Shows the standard single-select folder picker on an initialized STA thread.
pub fn pick_folder(owner: HWND) -> io::Result<Option<PathBuf>> {
    let Some(dialog) = selection_dialog(owner, Picker::Folder)? else {
        return Ok(None);
    };
    let item = unsafe { dialog.GetResult() }.map_err(|error| {
        shell_error(
            tr!("選択されたフォルダーを取得", "Get selected folder"),
            error,
        )
    })?;
    Ok(Some(PathBuf::from(display_name(&item, SIGDN_FILESYSPATH)?)))
}

/// Shows the standard single-select executable picker on an initialized STA thread.
pub fn pick_executable(owner: HWND) -> io::Result<Option<PathBuf>> {
    let Some(dialog) = selection_dialog(owner, Picker::Executable)? else {
        return Ok(None);
    };
    let item = unsafe { dialog.GetResult() }.map_err(|error| {
        shell_error(
            tr!("選択された実行ファイルを取得", "Get selected executable"),
            error,
        )
    })?;
    Ok(Some(PathBuf::from(display_name(&item, SIGDN_FILESYSPATH)?)))
}
