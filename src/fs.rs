use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_LESS_THAN, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
    GetLogicalDrives, MoveFileExW,
};

pub struct Entry {
    pub name: OsString,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hidden: bool,
}

pub fn list_dir(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let reader =
        fs::read_dir(path).map_err(|error| at(tr!("一覧取得", "List directory"), path, error))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|error| at(tr!("一覧取得", "List directory"), path, error))?;
        // An unreadable attribute must not make an otherwise accessible name disappear.
        let metadata = entry.metadata().ok();
        let attributes = metadata.as_ref().map_or(0, MetadataExt::file_attributes);
        let hidden = attributes & FILE_ATTRIBUTE_HIDDEN != 0;
        if hidden && !show_hidden {
            continue;
        }
        let is_dir = metadata.as_ref().map_or_else(
            || entry.file_type().is_ok_and(|kind| kind.is_dir()),
            |metadata| metadata.is_dir(),
        );
        entries.push(Entry {
            name: entry.file_name(),
            path: entry.path(),
            is_dir,
            size: metadata.as_ref().map_or(0, Metadata::len),
            modified: metadata.and_then(|metadata| metadata.modified().ok()),
            hidden,
        });
    }
    Ok(entries)
}

pub fn drives() -> Vec<PathBuf> {
    // This API returns a value only and does not retain any borrowed pointers.
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| PathBuf::from(format!("{}:\\", char::from(b'A' + index))))
        .collect()
}

pub fn rename(path: &Path, name: &str) -> io::Result<()> {
    validate_name(name)?;
    let source = leaf_path(path)?;
    let parent = source.parent().ok_or_else(|| {
        invalid(tr!(
            "ルートは名前を変更できません",
            "A root cannot be renamed"
        ))
    })?;
    let old_name = source
        .file_name()
        .ok_or_else(|| invalid(tr!("名前がありません", "The item has no name")))?;
    fs::symlink_metadata(&source).map_err(|error| at(tr!("名前変更", "Rename"), &source, error))?;
    ensure_name_available(parent, OsStr::new(name), Some(old_name))?;
    let target = parent.join(name);
    let source_wide = wide_path(&source)?;
    let target_wide = wide_path(&target)?;
    // Unlike std::fs::rename, flags=0 never requests replacement of an existing file.
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), 0) };
    if result == 0 {
        return Err(at(
            &tr_format!("名前変更 → {}", "Rename → {}", target.display()),
            &source,
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

pub fn create_folder(parent: &Path, name: &str) -> io::Result<()> {
    validate_name(name)?;
    let parent = checked_directory(parent)?;
    ensure_name_available(&parent, OsStr::new(name), None)?;
    let target = parent.join(name);
    fs::create_dir(&target)
        .map_err(|error| at(tr!("フォルダー作成", "Create folder"), &target, error))
}

pub fn delete_permanently(paths: &[PathBuf]) -> io::Result<()> {
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        let path = leaf_path(path)?;
        fs::symlink_metadata(&path)
            .map_err(|error| at(tr!("削除準備", "Prepare deletion"), &path, error))?;
        reject_overlapping(sources.iter().map(PathBuf::as_path), &path)?;
        sources.push(path);
    }
    for path in sources {
        remove_tree(&path)?;
    }
    Ok(())
}

fn remove_tree(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_no_directory_reparse(parent)?;
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| at(tr!("削除", "Delete"), path, error))?;
    let attributes = metadata.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // Remove the link/junction itself, never enumerate the linked directory.
        return if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
        .map_err(|error| at(tr!("リンク削除", "Delete link"), path, error));
    }
    if metadata.is_dir() {
        // The standard Windows implementation does not follow child symlinks and
        // uses its platform-specific protection against deletion-time link races.
        fs::remove_dir_all(path)
            .map_err(|error| at(tr!("フォルダー削除", "Delete folder"), path, error))
    } else {
        fs::remove_file(path).map_err(|error| at(tr!("ファイル削除", "Delete file"), path, error))
    }
}

fn ensure_no_directory_reparse(path: &Path) -> io::Result<()> {
    let absolute = std::path::absolute(path)
        .map_err(|error| at(tr!("パス解決", "Resolve path"), path, error))?;
    let mut prefix = PathBuf::new();
    for component in absolute.components() {
        prefix.push(component.as_os_str());
        // A drive prefix alone (C:) is not the rooted path C:\.
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&prefix)
            .map_err(|error| at(tr!("パス確認", "Check path"), &prefix, error))?;
        let attributes = metadata.file_attributes();
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(at(
                tr!("パス確認", "Check path"),
                &prefix,
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    tr!(
                        "ディレクトリの再解析ポイントはたどれません",
                        "Directory reparse points cannot be followed"
                    ),
                ),
            ));
        }
    }
    Ok(())
}

fn checked_directory(path: &Path) -> io::Result<PathBuf> {
    ensure_no_directory_reparse(path)?;
    let canonical = fs::canonicalize(path)
        .map_err(|error| at(tr!("フォルダー確認", "Check folder"), path, error))?;
    if !fs::metadata(&canonical)
        .map_err(|error| at(tr!("フォルダー確認", "Check folder"), path, error))?
        .is_dir()
    {
        return Err(at(
            tr!("フォルダー確認", "Check folder"),
            path,
            io::Error::new(
                io::ErrorKind::NotADirectory,
                tr!("フォルダーではありません", "Not a folder"),
            ),
        ));
    }
    Ok(canonical)
}

// Resolve the parent, not the leaf: deleting/renaming a link must affect the link.
fn leaf_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .map_err(|error| at(tr!("パス解決", "Resolve path"), path, error))?;
    let name = absolute.file_name().ok_or_else(|| {
        at(
            tr!("パス確認", "Check path"),
            path,
            invalid(tr!("ルートは操作できません", "A root cannot be modified")),
        )
    })?;
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid(tr!("親フォルダーがありません", "No parent folder")))?;
    Ok(checked_directory(parent)?.join(name))
}

fn ensure_name_available(parent: &Path, name: &OsStr, except: Option<&OsStr>) -> io::Result<()> {
    let name_wide: Vec<u16> = name.encode_wide().collect();
    for entry in
        fs::read_dir(parent).map_err(|error| at(tr!("名前の確認", "Check name"), parent, error))?
    {
        let entry = entry.map_err(|error| at(tr!("名前の確認", "Check name"), parent, error))?;
        let existing = entry.file_name();
        if except.is_some_and(|except| existing == except) {
            continue;
        }
        let existing_wide: Vec<u16> = existing.encode_wide().collect();
        if ordinal(&name_wide, &existing_wide) == Ordering::Equal {
            return Err(conflict(&parent.join(name)));
        }
    }
    Ok(())
}

fn ordinal(left: &[u16], right: &[u16]) -> Ordering {
    // Windows path components are limited well below i32::MAX UTF-16 units.
    let result = unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        )
    };
    match result {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        _ => Ordering::Greater,
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left: Vec<u16> = left.as_os_str().encode_wide().collect();
    let right: Vec<u16> = right.as_os_str().encode_wide().collect();
    ordinal(&left, &right) == Ordering::Equal
}

fn path_within(path: &Path, ancestor: &Path) -> bool {
    let mut components = path.components();
    ancestor.components().all(|part| {
        components.next().is_some_and(|other| {
            same_path(Path::new(part.as_os_str()), Path::new(other.as_os_str()))
        })
    })
}

fn reject_overlapping<'a>(
    mut paths: impl Iterator<Item = &'a Path>,
    next: &Path,
) -> io::Result<()> {
    if paths.any(|path| path_within(path, next) || path_within(next, path)) {
        return Err(at(
            tr!("操作準備", "Prepare operation"),
            next,
            invalid(tr!(
                "同じ項目、または親子関係の項目が同時に指定されています",
                "The same item or items with a parent-child relationship were specified together"
            )),
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name.encode_utf16().count() > 255
        || name
            .chars()
            .any(|character| character < ' ' || "<>:\"/\\|?*".contains(character))
    {
        return Err(invalid(tr_format!(
            "無効なファイル名: {name:?}",
            "Invalid file name: {name:?}"
        )));
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if device {
        return Err(invalid(tr_format!(
            "Windows の予約名は使用できません: {name:?}",
            "Windows reserved names cannot be used: {name:?}"
        )));
    }
    Ok(())
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(at(
            tr!("パス確認", "Check path"),
            path,
            invalid(tr!(
                "パスに NUL 文字が含まれています",
                "The path contains a NUL character"
            )),
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn conflict(path: &Path) -> io::Error {
    at(
        tr!("名前の競合", "Name conflict"),
        path,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            tr!(
                "同じ名前の項目が既に存在します (上書きしません)",
                "An item with the same name already exists (will not overwrite)"
            ),
        ),
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn at(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation}: {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "rfm-fs-{}-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed),
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
    fn validates_windows_names() {
        for name in [
            "",
            ".",
            "..",
            "x.",
            "x ",
            "a/b",
            "a\\b",
            "a:b",
            "a\0b",
            "a\nb",
            "CON",
            "nul.txt",
            "AUX .txt",
            "Lpt9.log",
            "com¹.txt",
            "CONOUT$",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        for name in ["通常の名前", ".hidden", "COM10", "console.txt", "a b"] {
            assert!(validate_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn rejects_renaming_over_an_existing_name() {
        let temp = Temp::new();
        fs::write(temp.0.join("first"), b"first").unwrap();
        fs::write(temp.0.join("SECOND"), b"second").unwrap();
        assert_eq!(
            rename(&temp.0.join("first"), "second").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(temp.0.join("first")).unwrap(), b"first");
        assert_eq!(fs::read(temp.0.join("SECOND")).unwrap(), b"second");
    }
}
