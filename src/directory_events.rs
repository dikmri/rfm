//! On-demand shell change subscriptions and cancellable filename-only search.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HANDLE as ShellHandle, HWND as ShellWindow};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    GPFIDL_DEFAULT, SHCNE_ATTRIBUTES, SHCNE_CREATE, SHCNE_DELETE, SHCNE_MKDIR, SHCNE_RENAMEFOLDER,
    SHCNE_RENAMEITEM, SHCNE_RMDIR, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNRF_InterruptLevel,
    SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock,
    SHChangeNotifyDeregister, SHChangeNotifyEntry, SHChangeNotifyRegister, SHGetPathFromIDListEx,
    SHParseDisplayName,
};
use windows::core::PCWSTR;
use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::Globalization::{
    CSTR_EQUAL, CompareStringOrdinal, FIND_FROMSTART, FindStringOrdinal,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_SHARE_READ, GetFileInformationByHandle,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WM_APP;

pub const MESSAGE: u32 = WM_APP + 70;

const EVENTS: u32 = SHCNE_ATTRIBUTES.0
    | SHCNE_CREATE.0
    | SHCNE_DELETE.0
    | SHCNE_MKDIR.0
    | SHCNE_RENAMEFOLDER.0
    | SHCNE_RENAMEITEM.0
    | SHCNE_RMDIR.0
    | SHCNE_UPDATEDIR.0
    | SHCNE_UPDATEITEM.0;

struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

/// A nonrecursive subscription. Create/drop on the COM-initialized owner UI
/// thread; keep it alive until the directory is no longer displayed. The owner
/// must consume MESSAGE with changed_paths even when a refresh is already queued.
pub struct Watcher {
    registration: u32,
    root: Vec<u16>,
    _pidl: Pidl,
}

impl Watcher {
    pub fn new(owner: HWND, path: &Path) -> io::Result<Self> {
        if owner.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!("通知先がありません", "No notification recipient"),
            ));
        }
        let path = std::path::absolute(path)?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                tr!(
                    "監視先がフォルダーではありません",
                    "The watched path is not a folder"
                ),
            ));
        }
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!(
                    "パスに NUL 文字が含まれています",
                    "The path contains a NUL character"
                ),
            ));
        }
        wide.push(0);
        let mut raw = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut raw, 0, None) }.map_err(
            |error| {
                io::Error::other(tr_format!(
                    "フォルダー通知の準備: {error}",
                    "Prepare folder notifications: {error}"
                ))
            },
        )?;
        let pidl = Pidl(raw);
        let entry = SHChangeNotifyEntry {
            pidl: pidl.0,
            fRecursive: false.into(),
        };
        let registration = unsafe {
            SHChangeNotifyRegister(
                ShellWindow(owner),
                SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_NewDelivery,
                EVENTS as i32,
                MESSAGE,
                1,
                &entry,
            )
        };
        if registration == 0 {
            return Err(io::Error::other(tr!(
                "フォルダーの変更通知を登録できませんでした",
                "Could not register folder change notifications"
            )));
        }
        Ok(Self {
            registration,
            root: wide,
            _pidl: pidl,
        })
    }

    pub fn affects(&self, changed: &[u16]) -> bool {
        change_affects(&self.root, changed)
    }
}

fn change_affects(root: &[u16], changed: &[u16]) -> bool {
    let root = root.strip_suffix(&[0]).unwrap_or(root);
    let changed = changed.strip_suffix(&[0]).unwrap_or(changed);
    let prefix = |parent: &[u16], child: &[u16]| {
        parent.len() <= child.len()
            && (parent.len() == child.len()
                || parent.last() == Some(&92)
                || child[parent.len()] == 92)
            && unsafe {
                CompareStringOrdinal(
                    parent.as_ptr(),
                    parent.len() as i32,
                    child.as_ptr(),
                    parent.len() as i32,
                    1,
                )
            } == CSTR_EQUAL
    };
    if prefix(changed, root) {
        return true;
    }
    prefix(root, changed)
        && !changed[root.len()..]
            .strip_prefix(&[92])
            .unwrap_or(&changed[root.len()..])
            .contains(&92)
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The retained PIDL is freed by its field destructor after deregistration.
        let _ = unsafe { SHChangeNotifyDeregister(self.registration) };
    }
}

struct NotificationLock(ShellHandle);

impl Drop for NotificationLock {
    fn drop(&mut self) {
        let _ = unsafe { SHChangeNotification_Unlock(self.0) };
    }
}

/// Decode a new-delivery MESSAGE while its shared memory is locked. Renames
/// return both old and new paths; paths need not still exist. Non-filesystem
/// PIDLs have no usable native path and are omitted. Do not retain LPARAM or
/// WPARAM for later: call this immediately, then debounce the owned paths.
pub fn changed_paths(w: WPARAM, l: LPARAM) -> io::Result<Vec<PathBuf>> {
    let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
    let mut event = 0;
    let handle = unsafe {
        SHChangeNotification_Lock(
            ShellHandle(w as *mut _),
            l as u32,
            Some(&mut pidls),
            Some(&mut event),
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::other(tr!(
            "フォルダーの変更通知を読み取れませんでした",
            "Could not read folder change notifications"
        )));
    }
    let _lock = NotificationLock(handle);
    if pidls.is_null() || event as u32 & EVENTS == 0 {
        return Ok(Vec::new());
    }
    let count = if event as u32 & (SHCNE_RENAMEITEM.0 | SHCNE_RENAMEFOLDER.0) != 0 {
        2
    } else {
        1
    };
    let mut paths = Vec::with_capacity(count);
    // Extended-length Windows paths fit in 32,768 UTF-16 units including NUL.
    let mut wide = vec![0u16; 32_768];
    for index in 0..count {
        let pidl = unsafe { *pidls.add(index) };
        if pidl.is_null() {
            continue;
        }
        if unsafe { SHGetPathFromIDListEx(pidl, &mut wide, GPFIDL_DEFAULT) }.as_bool() {
            let length = wide
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(wide.len());
            if length != 0 {
                let path = PathBuf::from(OsString::from_wide(&wide[..length]));
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    Ok(paths)
}

pub struct SearchResult {
    pub entries: Vec<crate::fs::Entry>,
    /// Locations that could not be searched completely: failed descendant
    /// directory opens, directories with enumeration errors (once per directory),
    /// or entries whose type could not be established. Intentional hidden and
    /// reparse-point exclusions are not counted. This is not a missing-file count.
    pub skipped: usize,
}

/// Recursive, case-insensitive ordinal filename substring search, not AQS or
/// indexed/content search. The root itself is not a result. Metadata failures
/// retain matching names with the same defaults as fs::list_dir. Reparse-point
/// entries can match but their directory trees are never followed.
///
/// Root access errors are fatal; descendant access errors increment skipped.
/// Cancellation discards partial results and returns Interrupted. Cancellation
/// is checked around filesystem calls, but cannot interrupt a blocked OS call.
pub fn search(
    root: &Path,
    query: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
) -> io::Result<SearchResult> {
    check_cancel(cancel)?;
    let query: Vec<u16> = query.encode_utf16().collect();
    let query_len = i32::try_from(query.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!("検索文字列が長すぎます", "The search query is too long"),
        )
    })?;
    let root = std::path::absolute(root)?;
    let mut visited = HashSet::new();
    let Some(frame) = Frame::open(&root, show_hidden, &mut visited)? else {
        check_cancel(cancel)?;
        return Ok(SearchResult {
            entries: Vec::new(),
            skipped: 0,
        });
    };
    let mut stack = vec![frame];
    let mut result = SearchResult {
        entries: Vec::new(),
        skipped: 0,
    };
    let mut wide_name = Vec::new();
    while !stack.is_empty() {
        check_cancel(cancel)?;
        let next = stack.last_mut().unwrap().reader.next();
        let entry = match next {
            None => {
                stack.pop();
                continue;
            }
            Some(Ok(entry)) => entry,
            Some(Err(_)) => {
                let frame = stack.last_mut().unwrap();
                if !frame.incomplete {
                    frame.incomplete = true;
                    result.skipped += 1;
                }
                continue;
            }
        };
        check_cancel(cancel)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).ok();
        let attributes = metadata.as_ref().map(MetadataExt::file_attributes);
        let hidden = attributes.is_some_and(|attributes| attributes & FILE_ATTRIBUTE_HIDDEN != 0);
        if hidden && !show_hidden {
            continue;
        }
        let kind = if metadata.is_none() {
            entry.file_type().ok()
        } else {
            None
        };
        let is_dir = attributes.map_or_else(
            || kind.is_some_and(|kind| kind.is_dir()),
            |attributes| attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        );
        let name = entry.file_name();
        let matches = if query.is_empty() {
            true
        } else {
            wide_name.clear();
            wide_name.extend(name.encode_wide());
            let position = unsafe {
                FindStringOrdinal(
                    FIND_FROMSTART,
                    wide_name.as_ptr(),
                    wide_name.len() as i32,
                    query.as_ptr(),
                    query_len,
                    1,
                )
            };
            position >= 0
        };
        check_cancel(cancel)?;
        let child = if is_dir
            && !attributes.is_some_and(|attributes| attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        {
            match Frame::open(&path, show_hidden, &mut visited) {
                Ok(frame) => frame,
                Err(_) => {
                    result.skipped += 1;
                    None
                }
            }
        } else {
            if metadata.is_none() && kind.is_none() {
                // Retain the name, but count the possible uninspectable subtree.
                result.skipped += 1;
            }
            None
        };
        if matches {
            result.entries.push(crate::fs::Entry {
                name,
                path,
                is_dir,
                size: metadata.as_ref().map_or(0, fs::Metadata::len),
                modified: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok()),
                hidden,
            });
        }
        check_cancel(cancel)?;
        if let Some(frame) = child {
            stack.push(frame);
        }
    }
    check_cancel(cancel)?;
    Ok(result)
}

struct Frame {
    reader: ReadDir,
    // Retain an OPEN_REPARSE_POINT handle denying write/delete sharing while
    // its reader and descendants are active, preventing replacement or retagging.
    _directory: File,
    incomplete: bool,
}

impl Frame {
    fn open(
        path: &Path,
        show_hidden: bool,
        visited: &mut HashSet<(u32, u64)>,
    ) -> io::Result<Option<Self>> {
        let directory = OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(directory.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                tr!(
                    "検索先がフォルダーではありません",
                    "The search location is not a folder"
                ),
            ));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !show_hidden && info.dwFileAttributes & FILE_ATTRIBUTE_HIDDEN != 0
        {
            return Ok(None);
        }
        let identity = (
            info.dwVolumeSerialNumber,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        );
        // Some network filesystems do not supply file IDs. No-follow directory
        // handles still prevent traversing a reparse loop; zero is not an ID.
        if identity.1 != 0 && !visited.insert(identity) {
            return Ok(None);
        }
        let reader = fs::read_dir(path)?;
        Ok(Some(Self {
            reader,
            _directory: directory,
            incomplete: false,
        }))
    }
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            tr!("検索をキャンセルしました", "Search canceled"),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::SystemTime;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileAttributesW};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "rfm-search-{}-{}-{}",
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

    #[test]
    fn finds_nested_files_and_directories_case_insensitively() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("MATCH-folder")).unwrap();
        fs::create_dir(temp.0.join("nested")).unwrap();
        fs::write(temp.0.join("nested").join("a-MaTcH.txt"), b"abc").unwrap();
        fs::write(temp.0.join("unrelated.txt"), b"").unwrap();
        let result = search(&temp.0, "match", true, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.skipped, 0);
        assert_eq!(result.entries.len(), 2);
        assert!(
            result
                .entries
                .iter()
                .any(|entry| entry.name == "MATCH-folder" && entry.is_dir)
        );
        assert!(
            result
                .entries
                .iter()
                .any(|entry| entry.name == "a-MaTcH.txt" && !entry.is_dir && entry.size == 3)
        );
    }

    #[test]
    fn cancellation_precedes_root_access() {
        let temp = Temp::new();
        let error = search(&temp.0.join("missing"), "", true, &AtomicBool::new(true))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn missing_root_is_an_error() {
        let temp = Temp::new();
        assert!(search(&temp.0.join("missing"), "", true, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn excludes_hidden_directory_trees_unless_requested() {
        let temp = Temp::new();
        let hidden = temp.0.join("hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("needle.txt"), b"").unwrap();
        let wide: Vec<u16> = hidden.as_os_str().encode_wide().chain(Some(0)).collect();
        let old = unsafe { GetFileAttributesW(wide.as_ptr()) };
        assert_ne!(old, u32::MAX);
        assert_ne!(
            unsafe { SetFileAttributesW(wide.as_ptr(), old | FILE_ATTRIBUTE_HIDDEN) },
            0
        );
        let visible = search(&temp.0, "needle", false, &AtomicBool::new(false)).unwrap();
        assert!(visible.entries.is_empty());
        assert_eq!(visible.skipped, 0);
        let all = search(&temp.0, "needle", true, &AtomicBool::new(false)).unwrap();
        assert_eq!(all.entries.len(), 1);
    }

    #[test]
    fn empty_query_matches_all_descendants_not_the_root() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("folder")).unwrap();
        fs::write(temp.0.join("folder").join("file"), b"").unwrap();
        let result = search(&temp.0, "", true, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert!(result.entries.iter().all(|entry| entry.path != temp.0));
    }

    #[test]
    fn does_not_descend_into_a_junction_back_to_the_root() {
        let temp = Temp::new();
        fs::write(temp.0.join("needle.txt"), b"").unwrap();
        let junction = temp.0.join("loop");
        // Directory junctions, unlike symbolic links, need no developer mode
        // or SeCreateSymbolicLinkPrivilege on Windows.
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&temp.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = search(&temp.0, "", true, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.skipped, 0);
        assert_eq!(result.entries.len(), 2);
        assert!(
            result
                .entries
                .iter()
                .any(|entry| entry.name == "loop" && entry.is_dir)
        );
        assert_eq!(
            result
                .entries
                .iter()
                .filter(|entry| entry.name == "needle.txt")
                .count(),
            1
        );
        fs::remove_dir(junction).unwrap();
    }
    #[test]
    fn notifications_match_windows_case_and_component_boundaries() {
        for (root, changed, expected) in [
            (r"D:\WORK", r"d:\work\file.txt", true),
            (r"D:\Ärbeit", r"d:\ärbeit\new", true),
            (r"D:\work\child", r"d:\WORK", true),
            (r"D:\work", r"d:\work", true),
            (r"D:\work", r"d:\workbench\file.txt", false),
            (r"D:\work", r"d:\work\child\file.txt", false),
            (r"D:\", r"d:\file.txt", true),
            (r"D:\", r"d:\child\file.txt", false),
        ] {
            let root: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            let changed: Vec<u16> = changed.encode_utf16().chain(Some(0)).collect();
            assert_eq!(change_affects(&root, &changed), expected);
        }
    }
}
