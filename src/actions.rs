use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND as NativeHwnd, LPARAM,
    LRESULT, POINT as NativePoint, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IDataObject,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop, IDropSource,
    IDropSource_Impl, OleInitialize, OleUninitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CMF_CANRENAME, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL,
    CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO,
    CMINVOKECOMMANDINFOEX, COPYENGINE_E_CANCELLED, COPYENGINE_E_USER_CANCELLED, DefSubclassProc,
    FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOFX_ADDUNDORECORD, FOFX_SHOWELEVATIONPROMPT,
    FileOperation, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, IFileOperation,
    ILFindLastID, ILIsParent, IShellFolder, IShellItem, IShellItemArray, IThumbnailCache,
    LocalThumbnailCache, RemoveWindowSubclass, SEE_MASK_NOASYNC, SEE_MASK_UNICODE, SHBindToObject,
    SHBindToParent, SHCreateItemFromParsingName, SHCreateShellItemArrayFromIDLists,
    SHParseDisplayName, SetWindowSubclass, WTS_EXTRACT, WTS_SCALETOREQUESTEDSIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DeleteMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW,
    HMENU, IsWindow, MENUITEMINFOW, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MIIM_ID, MIIM_SUBMENU,
    SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP,
    WM_MEASUREITEM, WM_MENUCHAR, WM_NCDESTROY,
};
use windows::core::{BOOL, HRESULT, Interface, PCSTR, PCWSTR, PSTR, implement};
use windows_sys::Win32::Foundation::{HWND, POINT};

const FIRST_SHELL_COMMAND: u32 = 1;
const LAST_SHELL_COMMAND: u32 = 0x7fff;
const OPEN_IN_TAB: u32 = 0x8000;
const PIN_TO_QUICK_ACCESS: u32 = 0x8001;
const OPEN_TERMINAL: u32 = 0x8002;

/// Shift adds CMF_EXTENDEDVERBS when the menu is opened.
pub const CONTEXT_MENU_FLAGS: u32 = CMF_NORMAL | CMF_EXPLORE;
/// Keep Explorer's progress, conflict, cancellation, elevation and undo behavior.
pub const TRANSFER_FLAGS: FILEOPERATION_FLAGS =
    FILEOPERATION_FLAGS(FOF_ALLOWUNDO.0 | FOFX_ADDUNDORECORD.0 | FOFX_SHOWELEVATIONPROMPT.0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    Canceled,
    Invoked,
    Open,
    OpenWindow,
    Rename,
    OpenInTab,
    PinToQuickAccess,
    OpenTerminal,
}

fn native_error(operation: &str, error: windows::core::Error) -> io::Error {
    let code = error.code();
    let kind = if matches!(code.0 as u32, 0x800704c7 | 0x80004004 | 0x800704d3)
        || code == COPYENGINE_E_USER_CANCELLED
        || code == COPYENGINE_E_CANCELLED
    {
        io::ErrorKind::Interrupted
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(
        kind,
        format!("{operation}: {error} (HRESULT 0x{:08X})", code.0 as u32),
    )
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            tr_format!(
                "パスを解決 ({}): {error}",
                "Resolve path ({}): {error}",
                path.display()
            ),
        )
    })?;
    let mut wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if wide.is_empty() || wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "パスが空か、NUL 文字を含んでいます。",
                "The path is empty or contains a NUL character."
            ),
        ));
    }
    wide.push(0);
    Ok(wide)
}

struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn parse(path: &Path) -> io::Result<Self> {
        let wide = wide_path(path)?;
        let mut pidl = null_mut();
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) }.map_err(
            |error| {
                native_error(
                    &tr_format!(
                        "シェル項目を取得 ({})",
                        "Get shell item ({})",
                        path.display()
                    ),
                    error,
                )
            },
        )?;
        Ok(Self(pidl))
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn item_array(paths: &[PathBuf]) -> io::Result<IShellItemArray> {
    let pidls = paths
        .iter()
        .map(|path| Pidl::parse(path))
        .collect::<io::Result<Vec<_>>>()?;
    let pointers: Vec<*const ITEMIDLIST> = pidls.iter().map(|pidl| pidl.0.cast_const()).collect();
    unsafe { SHCreateShellItemArrayFromIDLists(&pointers) }.map_err(|error| {
        native_error(
            tr!("シェル項目一覧を作成", "Create shell item array"),
            error,
        )
    })
}

struct OleApartment;

impl OleApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { OleInitialize(None) }.map_err(|error| {
            native_error(tr!("OLE の STA を初期化", "Initialize OLE STA"), error)
        })?;
        Ok(Self)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

struct PopupMenu(HMENU);

impl Drop for PopupMenu {
    fn drop(&mut self) {
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

struct MenuMessages {
    menu2: Option<IContextMenu2>,
    menu3: Option<IContextMenu3>,
}

unsafe extern "system" fn menu_subclass(
    owner: NativeHwnd,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass: usize,
    data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(owner, Some(menu_subclass), subclass) };
    } else if matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        // WM_DRAWITEM/WM_MEASUREITEM also serve controls; only forward menu messages.
        let is_menu = !matches!(message, WM_DRAWITEM | WM_MEASUREITEM) || wparam.0 == 0;
        if is_menu {
            // Clone before calling an extension, which can synchronously reenter the owner.
            let (menu3, menu2) = {
                let state = unsafe { &*(data as *const MenuMessages) };
                (state.menu3.clone(), state.menu2.clone())
            };
            if let Some(menu) = menu3 {
                let mut result = LRESULT(0);
                let handled = unsafe {
                    (menu.vtable().HandleMenuMsg2)(
                        menu.as_raw(),
                        message,
                        wparam,
                        lparam,
                        &mut result,
                    )
                };
                if handled == S_OK {
                    return result;
                }
            } else if message != WM_MENUCHAR
                && let Some(menu) = menu2
            {
                let handled = unsafe {
                    (menu.vtable().HandleMenuMsg)(menu.as_raw(), message, wparam, lparam)
                };
                if handled == S_OK {
                    return LRESULT(if matches!(message, WM_DRAWITEM | WM_MEASUREITEM) {
                        1
                    } else {
                        0
                    });
                }
            }
        }
    }
    unsafe { DefSubclassProc(owner, message, wparam, lparam) }
}

struct MenuSubclass {
    owner: NativeHwnd,
    state: Box<MenuMessages>,
}

impl MenuSubclass {
    fn install(owner: NativeHwnd, menu: &IContextMenu) -> io::Result<Self> {
        let state = Box::new(MenuMessages {
            menu2: menu.cast().ok(),
            menu3: menu.cast().ok(),
        });
        let address = (&*state as *const MenuMessages) as usize;
        if !unsafe { SetWindowSubclass(owner, Some(menu_subclass), address, address) }.as_bool() {
            return Err(io::Error::other(tr!(
                "シェルメニューのウィンドウ処理を登録できませんでした。",
                "Could not register the shell menu window handler."
            )));
        }
        Ok(Self { owner, state })
    }
}

impl Drop for MenuSubclass {
    fn drop(&mut self) {
        let address = (&*self.state as *const MenuMessages) as usize;
        let _ = unsafe { RemoveWindowSubclass(self.owner, Some(menu_subclass), address) };
    }
}

fn native_context_menu(
    owner: NativeHwnd,
    folder: &Path,
    selected: &[PathBuf],
) -> io::Result<IContextMenu> {
    if selected.is_empty() {
        let pidl = Pidl::parse(folder)?;
        let shell_folder: IShellFolder =
            unsafe { SHBindToObject(None, pidl.0, None) }.map_err(|error| {
                native_error(
                    tr!("フォルダーをシェルに接続", "Bind folder to shell"),
                    error,
                )
            })?;
        return unsafe { shell_folder.CreateViewObject(owner) }.map_err(|error| {
            native_error(tr!("フォルダーのメニューを取得", "Get folder menu"), error)
        });
    }

    let parent_path = selected[0].parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "選択項目の親フォルダーがありません。",
                "The selected item has no parent folder."
            ),
        )
    })?;
    let parent_pidl = Pidl::parse(parent_path)?;
    let pidls = selected
        .iter()
        .map(|path| Pidl::parse(path))
        .collect::<io::Result<Vec<_>>>()?;
    if pidls
        .iter()
        .all(|pidl| unsafe { ILIsParent(parent_pidl.0, pidl.0, true) }.as_bool())
    {
        let parent: IShellFolder =
            unsafe { SHBindToParent(pidls[0].0, None) }.map_err(|error| {
                native_error(
                    tr!(
                        "選択項目の親フォルダーを取得",
                        "Get selected items' parent folder"
                    ),
                    error,
                )
            })?;
        let children: Vec<*const ITEMIDLIST> = pidls
            .iter()
            .map(|pidl| unsafe { ILFindLastID(pidl.0) }.cast_const())
            .collect();
        return unsafe { parent.GetUIObjectOf(owner, &children, None) }.map_err(|error| {
            native_error(
                tr!("選択項目のメニューを取得", "Get selected items' menu"),
                error,
            )
        });
    }

    // GetUIObjectOf requires immediate-child PIDLs, each with one SHITEMID.
    // Passing multi-component absolute PIDLs to the Desktop violates that contract.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        tr!(
            "異なる親フォルダーにある項目の共有メニューは表示できません。各フォルダーの項目を個別に選択してください。",
            "A shared menu cannot be shown for items in different parent folders. Select items from one folder at a time."
        ),
    ))
}

fn command_verb(menu: &IContextMenu, offset: u32) -> Option<String> {
    let mut verb = [0u16; 256];
    unsafe {
        menu.GetCommandString(
            offset as usize,
            GCS_VERBW,
            None,
            PSTR(verb.as_mut_ptr().cast()),
            verb.len() as u32,
        )
    }
    .ok()?;
    let length = verb.iter().position(|&ch| ch == 0).unwrap_or(verb.len());
    Some(String::from_utf16_lossy(&verb[..length]))
}

fn remove_view_commands(menu: &IContextMenu, popup: HMENU, command_count: u32) -> io::Result<bool> {
    let count = unsafe { GetMenuItemCount(Some(popup)) };
    if count < 0 {
        return Err(native_error(
            tr!("メニュー項目数を取得", "Get menu item count"),
            windows::core::Error::from_thread(),
        ));
    }
    let mut changed = false;
    for position in (0..count as u32).rev() {
        let mut item = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_SUBMENU,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(popup, position, true, &mut item) }
            .map_err(|error| native_error(tr!("メニュー項目を取得", "Get menu item"), error))?;
        // Popup headers may use their HMENU as wID, not a shell command.
        let view_command = item.hSubMenu.is_invalid()
            && item
                .wID
                .checked_sub(FIRST_SHELL_COMMAND)
                .filter(|&offset| {
                    offset < command_count && offset <= LAST_SHELL_COMMAND - FIRST_SHELL_COMMAND
                })
                .and_then(|offset| command_verb(menu, offset))
                .is_some_and(|verb| {
                    [
                        "view",
                        "arrange",
                        "sort",
                        "sortby",
                        "group",
                        "groupby",
                        "refresh",
                        "viewlargeicons",
                        "viewsmallicons",
                        "viewlist",
                        "viewdetails",
                        "viewtiles",
                        "viewthumbs",
                        "viewcontent",
                        "autoarrange",
                        "aligntogrid",
                    ]
                    .iter()
                    .any(|native| verb.eq_ignore_ascii_case(native))
                });
        if view_command {
            unsafe { DeleteMenu(popup, position, MF_BYPOSITION) }.map_err(|error| {
                native_error(
                    tr!(
                        "適用できない表示メニューを除外",
                        "Remove inapplicable view command"
                    ),
                    error,
                )
            })?;
            changed = true;
        } else if !item.hSubMenu.is_invalid() {
            let child_changed = remove_view_commands(menu, item.hSubMenu, command_count)?;
            changed |= child_changed;
            if child_changed && unsafe { GetMenuItemCount(Some(item.hSubMenu)) } == 0 {
                unsafe { DeleteMenu(popup, position, MF_BYPOSITION) }.map_err(|error| {
                    native_error(
                        tr!("空の表示メニューを除外", "Remove empty view menu"),
                        error,
                    )
                })?;
            }
        }
    }
    Ok(changed)
}

/// `point` is in screen coordinates. The caller must have initialized the UI STA.
/// Host actions apply to the single selected directory, or `folder` for a background menu.
pub fn context_menu(
    owner: HWND,
    folder: &Path,
    selected: &[PathBuf],
    point: POINT,
) -> io::Result<MenuAction> {
    let owner = NativeHwnd(owner);
    if !unsafe { IsWindow(Some(owner)) }.as_bool() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "メニューの親ウィンドウがありません。",
                "The menu has no owner window."
            ),
        ));
    }
    let context = native_context_menu(owner, folder, selected)?;
    let popup =
        PopupMenu(unsafe { CreatePopupMenu() }.map_err(|error| {
            native_error(tr!("シェルメニューを作成", "Create shell menu"), error)
        })?);
    let _subclass = MenuSubclass::install(owner, &context)?;
    let extended = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let flags = CONTEXT_MENU_FLAGS
        | if extended { CMF_EXTENDEDVERBS } else { 0 }
        | if selected.len() == 1 {
            CMF_CANRENAME
        } else {
            0
        };
    let queried = unsafe {
        context.QueryContextMenu(popup.0, 0, FIRST_SHELL_COMMAND, LAST_SHELL_COMMAND, flags)
    };
    queried.ok().map_err(|error| {
        native_error(
            tr!("シェルメニューの項目を取得", "Get shell menu items"),
            error,
        )
    })?;
    let command_count = queried.0 as u32 & 0xffff;
    if selected.is_empty() {
        // There is no Explorer IShellView site; the host owns view/sort/group/refresh.
        remove_view_commands(&context, popup.0, command_count)?;
    }

    let target = if selected.is_empty() {
        Some(folder)
    } else if selected.len() == 1 {
        Some(selected[0].as_path())
    } else {
        None
    };
    let directory = target.is_some_and(Path::is_dir);
    if directory {
        unsafe {
            AppendMenuW(popup.0, MF_SEPARATOR, 0, PCWSTR::null()).map_err(|error| {
                native_error(tr!("メニューの区切りを追加", "Add menu separator"), error)
            })?;
            AppendMenuW(
                popup.0,
                MF_STRING,
                OPEN_IN_TAB as usize,
                PCWSTR(tr_w!("新しいタブで開く", "Open in New Tab")),
            )
            .map_err(|error| {
                native_error(tr!("タブのメニューを追加", "Add tab menu command"), error)
            })?;
            AppendMenuW(
                popup.0,
                MF_STRING,
                OPEN_TERMINAL as usize,
                PCWSTR(tr_w!(
                    "設定したターミナルで開く",
                    "Open in Configured Terminal"
                )),
            )
            .map_err(|error| {
                native_error(
                    tr!("ターミナルのメニューを追加", "Add terminal menu command"),
                    error,
                )
            })?;
        }
        // Selected folders already offer Windows' shared-list pin/unpin command.
        // The background's host action targets the current folder instead.
        if selected.is_empty() {
            unsafe {
                AppendMenuW(
                    popup.0,
                    MF_STRING,
                    PIN_TO_QUICK_ACCESS as usize,
                    PCWSTR(tr_w!("クイックアクセスに追加", "Add to Quick Access")),
                )
            }
            .map_err(|error| {
                native_error(
                    tr!(
                        "クイックアクセスのメニューを追加",
                        "Add Quick Access menu command"
                    ),
                    error,
                )
            })?;
        }
    }

    // Shell extensions can pump messages and close the owner while building a menu.
    if !unsafe { IsWindow(Some(owner)) }.as_bool() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            tr!(
                "メニューのウィンドウが閉じられました。",
                "The menu window was closed."
            ),
        ));
    }
    let command = unsafe {
        TrackPopupMenuEx(
            popup.0,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            point.x,
            point.y,
            owner,
            None,
        )
    }
    .0 as u32;
    match command {
        0 => return Ok(MenuAction::Canceled),
        OPEN_IN_TAB => return Ok(MenuAction::OpenInTab),
        PIN_TO_QUICK_ACCESS => return Ok(MenuAction::PinToQuickAccess),
        OPEN_TERMINAL => return Ok(MenuAction::OpenTerminal),
        _ => {}
    }
    if !unsafe { IsWindow(Some(owner)) }.as_bool() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            tr!(
                "メニューのウィンドウが閉じられました。",
                "The menu window was closed."
            ),
        ));
    }
    let offset = command
        .checked_sub(FIRST_SHELL_COMMAND)
        .filter(|&offset| command <= LAST_SHELL_COMMAND && offset < command_count)
        .ok_or_else(|| {
            io::Error::other(tr!(
                "シェルメニューが不正なコマンドを返しました。",
                "The shell menu returned an invalid command."
            ))
        })?;
    if selected.len() == 1
        && let Some(verb) = command_verb(&context, offset)
    {
        if verb.eq_ignore_ascii_case("rename") {
            return Ok(MenuAction::Rename);
        }
        if directory && (verb.eq_ignore_ascii_case("open") || verb.eq_ignore_ascii_case("explore"))
        {
            return Ok(MenuAction::Open);
        }
        if directory
            && (verb.eq_ignore_ascii_case("opennewwindow")
                || verb.eq_ignore_ascii_case("opennewprocess"))
        {
            return Ok(MenuAction::OpenWindow);
        }
    }
    let directory_wide = wide_path(folder)?;
    let mut mask = SEE_MASK_UNICODE | SEE_MASK_NOASYNC | CMIC_MASK_PTINVOKE;
    if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 {
        mask |= CMIC_MASK_CONTROL_DOWN;
    }
    if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 {
        mask |= CMIC_MASK_SHIFT_DOWN;
    }
    let invoke = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: mask,
        hwnd: owner,
        lpVerb: PCSTR(offset as usize as *const u8),
        lpVerbW: PCWSTR(offset as usize as *const u16),
        lpDirectoryW: PCWSTR(directory_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: NativePoint {
            x: point.x,
            y: point.y,
        },
        ..Default::default()
    };
    unsafe {
        context
            .InvokeCommand((&invoke as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())
    }
    .map_err(|error| {
        native_error(
            tr!(
                "シェルメニューのコマンドを実行",
                "Execute shell menu command"
            ),
            error,
        )
    })?;
    Ok(MenuAction::Invoked)
}

/// Runs synchronously; a worker thread may call this without prior COM initialization.
/// Sources and cut clipboard state must only be discarded by the caller after success.
pub fn transfer(owner: HWND, sources: &[PathBuf], destination: &Path, cut: bool) -> io::Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    let _apartment = OleApartment::initialize()?;
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| {
                native_error(
                    tr!(
                        "Windows のファイル操作を作成",
                        "Create Windows file operation"
                    ),
                    error,
                )
            },
        )?;
    let items = item_array(sources)?;
    let destination_wide = wide_path(destination)?;
    let target: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(destination_wide.as_ptr()), None) }.map_err(
            |error| {
                native_error(
                    &tr_format!(
                        "転送先を取得 ({})",
                        "Get destination ({})",
                        destination.display()
                    ),
                    error,
                )
            },
        )?;
    unsafe {
        operation
            .SetOwnerWindow(NativeHwnd(owner))
            .map_err(|error| {
                native_error(
                    tr!(
                        "ファイル操作の親ウィンドウを設定",
                        "Set file operation owner window"
                    ),
                    error,
                )
            })?;
        operation
            .SetOperationFlags(TRANSFER_FLAGS)
            .map_err(|error| {
                native_error(
                    tr!(
                        "ファイル操作の確認・元に戻す設定",
                        "Set file operation confirmation and undo options"
                    ),
                    error,
                )
            })?;
        let queued = if cut {
            operation.MoveItems(&items, &target)
        } else {
            operation.CopyItems(&items, &target)
        };
        queued.map_err(|error| {
            native_error(
                if cut {
                    tr!("移動を登録", "Queue move")
                } else {
                    tr!("コピーを登録", "Queue copy")
                },
                error,
            )
        })?;
        let performed = operation.PerformOperations();
        let aborted = operation.GetAnyOperationsAborted().map_err(|error| {
            native_error(
                tr!(
                    "ファイル操作の中断状態を取得",
                    "Get file operation cancellation status"
                ),
                error,
            )
        })?;
        if aborted.as_bool() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                tr!(
                    "ファイル操作がキャンセルされたか、一部の項目を処理できませんでした。",
                    "The file operation was canceled or some items could not be processed."
                ),
            ));
        }
        performed.map_err(|error| {
            native_error(
                if cut {
                    tr!("ファイルを移動", "Move files")
                } else {
                    tr!("ファイルをコピー", "Copy files")
                },
                error,
            )
        })?;
    }
    Ok(())
}

#[implement(IDropSource)]
struct DragSource;

impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() || (keys.0 & MK_RBUTTON.0) != 0 {
            DRAGDROP_S_CANCEL
        } else if (keys.0 & MK_LBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Call from the OLE-initialized UI thread while handling the list's left-button drag.
/// Returns true only after a successful move; cancellation and copying return false.
pub fn drag(owner: HWND, paths: &[PathBuf]) -> io::Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    if !unsafe { IsWindow(Some(NativeHwnd(owner))) }.as_bool() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "ドラッグ元のウィンドウがありません。",
                "The drag source window is unavailable."
            ),
        ));
    }
    let items = item_array(paths)?;
    let data: IDataObject =
        unsafe { items.BindToHandler(None, &BHID_DataObject) }.map_err(|error| {
            native_error(
                tr!(
                    "ドラッグするファイルのデータを取得",
                    "Get dragged file data"
                ),
                error,
            )
        })?;
    let source: IDropSource = DragSource.into();
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe {
        DoDragDrop(
            &data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
            &mut effect,
        )
    };
    if result == DRAGDROP_S_CANCEL {
        return Ok(false);
    }
    result
        .ok()
        .map_err(|error| native_error(tr!("ファイルをドラッグ", "Drag files"), error))?;
    Ok(result == DRAGDROP_S_DROP && (effect.0 & DROPEFFECT_MOVE.0) != 0)
}

/// Extracts an on-demand Shell thumbnail on the calling worker's STA.
/// The caller owns the returned HBITMAP and must release it with DeleteObject.
pub fn thumbnail(path: &Path, size: u32) -> io::Result<usize> {
    if size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "サムネイルのサイズは 1 以上にしてください。",
                "The thumbnail size must be at least 1."
            ),
        ));
    }
    let _apartment = OleApartment::initialize()?;
    let wide = wide_path(path)?;
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
        .map_err(|error| {
            native_error(
                &tr_format!(
                    "サムネイルの項目を取得 ({})",
                    "Get thumbnail item ({})",
                    path.display()
                ),
                error,
            )
        })?;
    let cache: IThumbnailCache =
        unsafe { CoCreateInstance(&LocalThumbnailCache, None, CLSCTX_INPROC_SERVER) }.map_err(
            |error| {
                native_error(
                    tr!(
                        "Windows のサムネイルキャッシュを取得",
                        "Get Windows thumbnail cache"
                    ),
                    error,
                )
            },
        )?;
    let mut bitmap = None;
    unsafe {
        cache.GetThumbnail(
            &item,
            size,
            WTS_EXTRACT | WTS_SCALETOREQUESTEDSIZE,
            Some(&mut bitmap),
            None,
            None,
        )
    }
    .map_err(|error| {
        native_error(
            &tr_format!(
                "サムネイルを抽出 ({})",
                "Extract thumbnail ({})",
                path.display()
            ),
            error,
        )
    })?;
    let bitmap = bitmap.ok_or_else(|| {
        io::Error::other(tr!(
            "シェルはサムネイルを返しませんでした。",
            "The shell did not return a thumbnail."
        ))
    })?;
    let handle = unsafe { bitmap.Detach() }.map_err(|error| {
        native_error(
            tr!("サムネイルの所有権を取得", "Take thumbnail ownership"),
            error,
        )
    })?;
    if handle.is_invalid() {
        return Err(io::Error::other(tr!(
            "シェルは無効なサムネイルを返しました。",
            "The shell returned an invalid thumbnail."
        )));
    }
    Ok(handle.0 as usize)
}
