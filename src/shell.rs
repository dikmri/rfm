use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HGLOBAL, HWND, SetLastError};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
use windows_sys::Win32::UI::Controls::HIMAGELIST;
use windows_sys::Win32::UI::Shell::{
    DROPFILES, DragQueryFileW, FO_DELETE, FOF_ALLOWUNDO, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC,
    SHELLEXECUTEINFOW, SHFILEINFOW, SHFILEOPSTRUCTW, SHFileOperationW, SHGFI_SMALLICON,
    SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, ShellExecuteExW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

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

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.is_empty() || text.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "パスが空か、NUL 文字を含んでいます。",
                "The path is empty or contains a NUL character."
            ),
        ));
    }
    text.push(0);
    Ok(text)
}

// Shell file lists use absolute paths and an additional NUL after the last entry.
fn path_list(paths: &[PathBuf]) -> io::Result<Vec<u16>> {
    let mut list = Vec::new();
    for path in paths {
        let absolute = std::path::absolute(path)?;
        list.extend(wide_path(&absolute)?);
    }
    if list.is_empty() {
        list.push(0);
    }
    list.push(0);
    Ok(list)
}

pub fn open(path: &Path) -> io::Result<()> {
    let path = wide_path(path)?;
    unsafe {
        SetLastError(0);
        let result = ShellExecuteW(
            null_mut(),
            null(),
            path.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        ) as isize;
        if result <= 32 {
            let error = last_error(tr!("ファイルを開く", "Open file"));
            return Err(io::Error::new(
                error.kind(),
                format!("{error} (ShellExecuteW {result})"),
            ));
        }
    }
    Ok(())
}

pub fn properties(owner: HWND, path: &Path) -> io::Result<()> {
    let path = wide_path(path)?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC,
        hwnd: owner,
        lpVerb: windows_sys::w!("properties"),
        lpFile: path.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(last_error(tr!("プロパティを表示", "Show properties")));
    }
    Ok(())
}

pub fn recycle(owner: HWND, paths: &[PathBuf]) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let list = path_list(paths)?;
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: owner,
        wFunc: FO_DELETE,
        pFrom: list.as_ptr(),
        fFlags: FOF_ALLOWUNDO as u16,
        ..Default::default()
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if operation.fAnyOperationsAborted != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            tr!(
                "ごみ箱への移動がキャンセルされました。",
                "Moving to the Recycle Bin was canceled."
            ),
        ));
    }
    if result != 0 {
        // SHFileOperation returns its own error codes, not GetLastError values.
        return Err(io::Error::other(tr_format!(
            "ごみ箱への移動に失敗しました (SHFileOperationW 0x{result:04X})。",
            "Could not move to the Recycle Bin (SHFileOperationW 0x{result:04X})."
        )));
    }
    Ok(())
}

struct Clipboard;

impl Clipboard {
    fn open(owner: HWND) -> io::Result<Self> {
        if unsafe { OpenClipboard(owner) } == 0 {
            return Err(last_error(tr!("クリップボードを開く", "Open clipboard")));
        }
        Ok(Self)
    }

    fn empty(&self) -> io::Result<()> {
        if unsafe { EmptyClipboard() } == 0 {
            return Err(last_error(tr!(
                "クリップボードを空にする",
                "Empty clipboard"
            )));
        }
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

struct GlobalBlock(HGLOBAL);

impl GlobalBlock {
    fn allocate(bytes: usize) -> io::Result<Self> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes) };
        if handle.is_null() {
            return Err(last_error(tr!(
                "クリップボード用メモリを確保",
                "Allocate clipboard memory"
            )));
        }
        Ok(Self(handle))
    }

    fn publish(mut self, format: u32) -> io::Result<()> {
        if unsafe { SetClipboardData(format, self.0) }.is_null() {
            return Err(last_error(tr!(
                "クリップボードに書き込む",
                "Write to clipboard"
            )));
        }
        // A successful SetClipboardData transfers ownership to Windows.
        self.0 = null_mut();
        Ok(())
    }
}

impl Drop for GlobalBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GlobalFree(self.0) };
        }
    }
}

struct GlobalLockGuard {
    handle: HGLOBAL,
    pointer: *mut u8,
}

impl GlobalLockGuard {
    fn new(handle: HGLOBAL) -> io::Result<Self> {
        let pointer = unsafe { GlobalLock(handle) }.cast::<u8>();
        if pointer.is_null() {
            return Err(last_error(tr!(
                "クリップボード用メモリをロック",
                "Lock clipboard memory"
            )));
        }
        Ok(Self { handle, pointer })
    }
}

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe { GlobalUnlock(self.handle) };
    }
}

fn drop_effect_format() -> io::Result<u32> {
    let format = unsafe { RegisterClipboardFormatW(windows_sys::w!("Preferred DropEffect")) };
    if format == 0 {
        return Err(last_error(tr!(
            "ファイル操作用クリップボード形式を登録",
            "Register file operation clipboard format"
        )));
    }
    Ok(format)
}

pub fn set_clipboard(owner: HWND, paths: &[PathBuf], cut: bool) -> io::Result<()> {
    if owner.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "クリップボードの所有者ウィンドウが必要です。",
                "A clipboard owner window is required."
            ),
        ));
    }
    if paths.is_empty() {
        return clear_clipboard(owner);
    }
    let list = path_list(paths)?;
    let bytes = list
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|bytes| bytes.checked_add(size_of::<DROPFILES>()))
        .ok_or_else(|| {
            io::Error::other(tr!(
                "クリップボードのデータが大きすぎます。",
                "The clipboard data is too large."
            ))
        })?;
    let files = GlobalBlock::allocate(bytes)?;
    {
        let lock = GlobalLockGuard::new(files.0)?;
        let header = DROPFILES {
            pFiles: size_of::<DROPFILES>() as u32,
            fWide: 1,
            ..Default::default()
        };
        unsafe {
            lock.pointer.cast::<DROPFILES>().write_unaligned(header);
            std::ptr::copy_nonoverlapping(
                list.as_ptr().cast::<u8>(),
                lock.pointer.add(size_of::<DROPFILES>()),
                list.len() * size_of::<u16>(),
            );
        }
    }
    let effect_format = drop_effect_format()?;
    let effect = GlobalBlock::allocate(size_of::<u32>())?;
    {
        let lock = GlobalLockGuard::new(effect.0)?;
        unsafe {
            lock.pointer.cast::<u32>().write_unaligned(if cut {
                DROPEFFECT_MOVE
            } else {
                DROPEFFECT_COPY
            });
        }
    }
    let clipboard = Clipboard::open(owner)?;
    clipboard.empty()?;
    files.publish(CF_HDROP as u32)?;
    if let Err(error) = effect.publish(effect_format) {
        // Never leave a cut operation advertised as a copy after partial failure.
        let _ = clipboard.empty();
        return Err(error);
    }
    Ok(())
}

pub fn get_clipboard(owner: HWND) -> io::Result<(Vec<PathBuf>, bool)> {
    let effect_format = drop_effect_format()?;
    let _clipboard = Clipboard::open(owner)?;
    if unsafe { IsClipboardFormatAvailable(CF_HDROP as u32) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            tr!(
                "クリップボードにファイルがありません。",
                "The clipboard contains no files."
            ),
        ));
    }
    let files = unsafe { GetClipboardData(CF_HDROP as u32) };
    if files.is_null() {
        return Err(last_error(tr!(
            "クリップボードからファイルを取得",
            "Get files from clipboard"
        )));
    }
    // DragQueryFileW also converts an ANSI DROPFILES list to Unicode.
    // The HDROP belongs to the clipboard: neither DragFinish nor GlobalFree applies.
    let count = unsafe { DragQueryFileW(files, u32::MAX, null_mut(), 0) };
    let mut paths = Vec::new();
    for index in 0..count {
        let length = unsafe { DragQueryFileW(files, index, null_mut(), 0) };
        if length == 0 || length == u32::MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!(
                    "クリップボードのパスが不正です。",
                    "The clipboard path is invalid."
                ),
            ));
        }
        let mut text = vec![0u16; length as usize + 1];
        let copied = unsafe { DragQueryFileW(files, index, text.as_mut_ptr(), length + 1) };
        if copied != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!(
                    "クリップボードのパスを読み取れません。",
                    "Could not read the clipboard path."
                ),
            ));
        }
        paths.push(PathBuf::from(OsString::from_wide(&text[..length as usize])));
    }
    let mut cut = false;
    if unsafe { IsClipboardFormatAvailable(effect_format) } != 0 {
        let effect = unsafe { GetClipboardData(effect_format) };
        if effect.is_null() {
            return Err(last_error(tr!(
                "クリップボードから操作種別を取得",
                "Get operation type from clipboard"
            )));
        }
        if unsafe { GlobalSize(effect) } < size_of::<u32>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!(
                    "クリップボードの操作種別が不正です。",
                    "The clipboard operation type is invalid."
                ),
            ));
        }
        let lock = GlobalLockGuard::new(effect)?;
        cut = unsafe { lock.pointer.cast::<u32>().read_unaligned() } & DROPEFFECT_MOVE != 0;
    }
    Ok((paths, cut))
}

pub fn clear_clipboard(owner: HWND) -> io::Result<()> {
    Clipboard::open(owner)?.empty()
}

pub fn system_image_list() -> HIMAGELIST {
    let mut info = SHFILEINFOW::default();
    // This is a borrowed system-owned image list; callers must not destroy it.
    unsafe {
        SHGetFileInfoW(
            windows_sys::w!("file"),
            FILE_ATTRIBUTE_NORMAL,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        ) as HIMAGELIST
    }
}

pub fn icon_index(path: &Path, directory: bool) -> i32 {
    let Ok(path) = wide_path(path) else {
        return 0;
    };
    let mut info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            path.as_ptr(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if result == 0 { 0 } else { info.iIcon }
}

/// Preserves known-folder and desktop.ini artwork instead of guessing by file
/// attributes. Call on a background STA: shell extensions may perform slow I/O.
pub fn folder_icon_index(path: &Path) -> i32 {
    let Ok(wide) = wide_path(path) else {
        return icon_index(path, true);
    };
    let mut info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
        )
    };
    if result == 0 {
        icon_index(path, true)
    } else {
        info.iIcon
    }
}

pub fn system_large_image_list() -> HIMAGELIST {
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            windows_sys::w!("file"),
            FILE_ATTRIBUTE_NORMAL,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
        ) as HIMAGELIST
    }
}

pub fn file_type(path: &Path, directory: bool) -> Vec<u16> {
    let Ok(path) = wide_path(path) else {
        return vec![0];
    };
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            path.as_ptr(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            windows_sys::Win32::UI::Shell::SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        );
    }
    let length = info
        .szTypeName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szTypeName.len() - 1);
    info.szTypeName[..=length].to_vec()
}
