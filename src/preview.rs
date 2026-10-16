use crate::{actions, chrome::Style};
use std::{
    cell::RefCell,
    io::{self, Read},
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender},
};
use windows_sys::Win32::{
    Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};
const READY: u32 = WM_APP + 91;
const CLASS: *const u16 = windows_sys::core::w!("rfm.preview");
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
struct Bitmap(usize);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0 as HGDIOBJ);
        }
    }
}
enum Content {
    Text(Vec<u16>),
    Image(Bitmap),
    Notice(Vec<u16>),
}
struct State {
    style: Style,
    edit: HWND,
    path: Option<PathBuf>,
    generation: u64,
    running: bool,
    content: Content,
    tx: Sender<(u64, Content)>,
    rx: Receiver<(u64, Content)>,
    dc: HDC,
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            DeleteDC(self.dc);
        }
    }
}
#[derive(Clone, Copy)]
pub struct Panel(HWND);
unsafe fn cell(hwnd: HWND) -> Option<&'static RefCell<State>> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>).as_ref() }
}
impl Panel {
    pub fn new(parent: HWND, style: Style) -> io::Result<Self> {
        unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: CLASS,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..zeroed()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                0,
                CLASS,
                tr_w!("プレビュー", "Preview"),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                parent,
                null_mut(),
                class.hInstance,
                &style as *const _ as _,
            );
            if hwnd.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(hwnd))
            }
        }
    }
    pub fn layout(self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            MoveWindow(self.0, x, y, w.max(0), h.max(0), 1);
        }
    }
    pub fn show(self, show: bool) {
        unsafe {
            ShowWindow(self.0, if show { SW_SHOW } else { SW_HIDE });
        }
    }
    pub fn set_style(self, style: Style) {
        unsafe {
            if let Some(cell) = cell(self.0) {
                let Ok(mut state) = cell.try_borrow_mut() else {
                    return;
                };
                state.style = style;
                let edit = state.edit;
                drop(state);
                SendMessageW(edit, WM_SETFONT, style.font as usize, 1);
                InvalidateRect(self.0, null(), 0);
            }
        }
    }
    pub fn set_path(self, path: Option<&Path>) {
        unsafe {
            if let Some(cell) = cell(self.0) {
                let Ok(mut state) = cell.try_borrow_mut() else {
                    return;
                };
                if state.path.as_deref() == path {
                    return;
                };
                state.path = path.map(Path::to_path_buf);
                state.generation += 1;
                state.content = Content::Notice(wide(if path.is_some() {
                    tr!("読み込み中…", "Loading…")
                } else {
                    tr!(
                        "プレビューするファイルを選択してください",
                        "Select a file to preview"
                    )
                }));
                let edit = state.edit;
                drop(state);
                ShowWindow(edit, SW_HIDE);
                SetTimer(self.0, 1, 140, None);
                InvalidateRect(self.0, null(), 0);
            }
        }
    }
    pub fn refresh(self) {
        unsafe {
            if let Some(cell) = cell(self.0) {
                let Ok(mut state) = cell.try_borrow_mut() else {
                    return;
                };
                state.generation += 1;
                drop(state);
                SetTimer(self.0, 1, 1, None);
            }
        }
    }
}
fn load(path: &Path) -> Content {
    let extension = path
        .extension()
        .map(|v| v.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if [
        "txt",
        "md",
        "rs",
        "toml",
        "json",
        "log",
        "csv",
        "xml",
        "html",
        "css",
        "js",
        "ts",
        "py",
        "ini",
        "yaml",
        "yml",
        "ps1",
        "bat",
        "c",
        "cpp",
        "h",
        "gitignore",
    ]
    .contains(&extension.as_str())
        || path.file_name().is_some_and(|v| v == ".gitignore")
    {
        let result = (|| -> io::Result<Vec<u16>> {
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(262145)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > 262144;
            bytes.truncate(262144);
            let utf8 = std::str::from_utf8(&bytes).or_else(|error| {
                if truncated && error.error_len().is_none() {
                    std::str::from_utf8(&bytes[..error.valid_up_to()])
                } else {
                    Err(error)
                }
            });
            let mut value = if bytes.starts_with(&[255, 254]) {
                bytes[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| u16::from_le_bytes(pair))
                    .collect::<Vec<_>>()
            } else if bytes.starts_with(&[254, 255]) {
                bytes[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| u16::from_be_bytes(pair))
                    .collect()
            } else if let Ok(text) = utf8 {
                text.trim_start_matches('\u{feff}').encode_utf16().collect()
            } else {
                unsafe {
                    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
                    let len = MultiByteToWideChar(
                        CP_ACP,
                        0,
                        bytes.as_ptr(),
                        bytes.len() as i32,
                        null_mut(),
                        0,
                    );
                    if len == 0 {
                        return Err(io::Error::last_os_error());
                    };
                    let mut value = vec![0; len as usize];
                    MultiByteToWideChar(
                        CP_ACP,
                        0,
                        bytes.as_ptr(),
                        bytes.len() as i32,
                        value.as_mut_ptr(),
                        len,
                    );
                    value
                }
            };
            let extra = value
                .iter()
                .enumerate()
                .filter(|(index, unit)| **unit == 10 && (*index == 0 || value[*index - 1] != 13))
                .count();
            let suffix = tr!(
                "\r\n\r\n（先頭256 KiBを表示しています）",
                "\r\n\r\n(Showing the first 256 KiB)"
            );
            value.reserve(
                extra
                    + 1
                    + if truncated {
                        suffix.encode_utf16().count()
                    } else {
                        0
                    },
            );
            let original = value.len();
            value.resize(original + extra, 0);
            // Grow backwards in place so unread UTF-16 units are never overwritten.
            let mut output = value.len();
            for index in (0..original).rev() {
                let unit = value[index];
                output -= 1;
                value[output] = if unit == 0 { 0x2400 } else { unit };
                if unit == 10 && (index == 0 || value[index - 1] != 13) {
                    output -= 1;
                    value[output] = 13;
                }
            }
            if truncated {
                value.extend(suffix.encode_utf16());
            }
            value.push(0);
            Ok(value)
        })();
        return match result {
            Ok(value) => Content::Text(value),
            Err(error) => Content::Notice(wide(&tr_format!(
                "プレビューを読み込めません\n{error}",
                "Could not load preview\n{error}"
            ))),
        };
    }
    match actions::thumbnail(path, 768) {
        Ok(bitmap) => Content::Image(Bitmap(bitmap)),
        Err(error) => Content::Notice(wide(&tr_format!(
            "プレビューを表示できません\n{error}",
            "Could not display preview\n{error}"
        ))),
    }
}
unsafe fn start(hwnd: HWND) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        if state.running {
            return;
        };
        let Some(path) = state.path.clone() else {
            state.content = Content::Notice(wide(tr!(
                "プレビューするファイルを選択してください",
                "Select a file to preview"
            )));
            return;
        };
        if path.is_dir() {
            state.content = Content::Notice(wide(tr!(
                "フォルダーの内容は一覧で確認できます",
                "View folder contents in the file list"
            )));
            drop(state);
            InvalidateRect(hwnd, null(), 0);
            return;
        }
        state.running = true;
        let generation = state.generation;
        let tx = state.tx.clone();
        drop(state);
        let target = hwnd as usize;
        std::thread::spawn(move || {
            let value = load(&path);
            if tx.send((generation, value)).is_ok() {
                PostMessageW(target as HWND, READY, 0, 0);
            }
        });
    }
}
unsafe fn draw(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let Some(cell) = cell(hwnd) else {
            EndPaint(hwnd, &ps);
            return;
        };
        let Ok(state) = cell.try_borrow() else {
            EndPaint(hwnd, &ps);
            return;
        };
        let mut rect: RECT = zeroed();
        GetClientRect(hwnd, &mut rect);
        SetDCBrushColor(dc, 0x00ff_ffff);
        FillRect(dc, &rect, GetStockObject(DC_BRUSH) as HBRUSH);
        let s = |n| n * state.style.dpi as i32 / 96;
        SetDCBrushColor(dc, 0x00df_dfdf);
        FillRect(
            dc,
            &RECT { right: 1, ..rect },
            GetStockObject(DC_BRUSH) as HBRUSH,
        );
        let old = SelectObject(dc, state.style.font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x0060_6060);
        let mut title = RECT {
            left: s(12),
            top: s(8),
            right: rect.right - s(8),
            bottom: s(32),
        };
        DrawTextW(
            dc,
            tr_w!("プレビュー", "Preview"),
            -1,
            &mut title,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
        let mut body = RECT {
            left: s(12),
            top: s(42),
            right: rect.right - s(12),
            bottom: rect.bottom - s(12),
        };
        match &state.content {
            Content::Notice(label) => {
                DrawTextW(
                    dc,
                    label.as_ptr(),
                    -1,
                    &mut body,
                    DT_WORDBREAK | DT_NOPREFIX,
                );
            }
            Content::Image(bitmap) => {
                let mut info: BITMAP = zeroed();
                if GetObjectW(
                    bitmap.0 as HGDIOBJ,
                    size_of::<BITMAP>() as i32,
                    &mut info as *mut _ as _,
                ) > 0
                    && info.bmWidth > 0
                    && info.bmHeight > 0
                    && body.right > body.left
                    && body.bottom > body.top
                {
                    let ratio = ((body.right - body.left) as f64 / info.bmWidth as f64)
                        .min((body.bottom - body.top) as f64 / info.bmHeight as f64)
                        .min(1.0);
                    let w = (info.bmWidth as f64 * ratio).round() as i32;
                    let h = (info.bmHeight as f64 * ratio).round() as i32;
                    let old_bitmap = SelectObject(state.dc, bitmap.0 as HGDIOBJ);
                    SetStretchBltMode(dc, HALFTONE);
                    StretchBlt(
                        dc,
                        body.left + (body.right - body.left - w) / 2,
                        body.top,
                        w,
                        h,
                        state.dc,
                        0,
                        0,
                        info.bmWidth,
                        info.bmHeight,
                        SRCCOPY,
                    );
                    SelectObject(state.dc, old_bitmap);
                }
            }
            Content::Text(_) => {}
        }
        SelectObject(dc, old);
        EndPaint(hwnd, &ps);
    }
}
unsafe extern "system" fn proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_NCCREATE => {
                let create = &*(l as *const CREATESTRUCTW);
                let style = *(create.lpCreateParams as *const Style);
                let (tx, rx) = mpsc::channel();
                let dc = CreateCompatibleDC(null_mut());
                if dc.is_null() {
                    return 0;
                }
                let state = State {
                    style,
                    edit: null_mut(),
                    path: None,
                    generation: 0,
                    running: false,
                    content: Content::Notice(wide(tr!(
                        "プレビューするファイルを選択してください",
                        "Select a file to preview"
                    ))),
                    tx,
                    rx,
                    dc,
                };
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_USERDATA,
                    Box::into_raw(Box::new(RefCell::new(state))) as isize,
                );
                return 1;
            }
            WM_CREATE => {
                let Some(cell) = cell(hwnd) else { return -1 };
                let style = cell.borrow().style;
                let edit = CreateWindowExW(
                    0,
                    windows_sys::core::w!("EDIT"),
                    null(),
                    WS_CHILD
                        | WS_VSCROLL
                        | WS_HSCROLL
                        | ES_MULTILINE as u32
                        | ES_READONLY as u32
                        | ES_AUTOHSCROLL as u32,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                SendMessageW(edit, WM_SETFONT, style.font as usize, 0);
                cell.borrow_mut().edit = edit;
                return 0;
            }
            WM_SIZE => {
                if let Some(cell) = cell(hwnd) {
                    let state = cell.borrow();
                    let s = |n| n * state.style.dpi as i32 / 96;
                    let mut r: RECT = zeroed();
                    GetClientRect(hwnd, &mut r);
                    MoveWindow(
                        state.edit,
                        s(10),
                        s(38),
                        (r.right - s(20)).max(0),
                        (r.bottom - s(48)).max(0),
                        1,
                    );
                }
                return 0;
            }
            WM_TIMER => {
                KillTimer(hwnd, 1);
                start(hwnd);
                return 0;
            }
            READY => {
                let Some(cell) = cell(hwnd) else { return 0 };
                let mut state = cell.borrow_mut();
                let mut stale = false;
                while let Ok((generation, value)) = state.rx.try_recv() {
                    state.running = false;
                    if generation != state.generation {
                        stale = true;
                        continue;
                    }
                    let edit = state.edit;
                    let text = if let Content::Text(value) = &value {
                        Some(value.as_ptr())
                    } else {
                        None
                    };
                    state.content = value;
                    drop(state);
                    if let Some(text) = text {
                        SetWindowTextW(edit, text);
                        ShowWindow(edit, SW_SHOW);
                    } else {
                        ShowWindow(edit, SW_HIDE);
                    }
                    state = cell.borrow_mut();
                }
                drop(state);
                if stale {
                    start(hwnd);
                }
                InvalidateRect(hwnd, null(), 0);
                return 0;
            }
            WM_PAINT => {
                draw(hwnd);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
                SetTextColor(w as HDC, 0x0020_2020);
                SetBkColor(w as HDC, 0x00ff_ffff);
                SetDCBrushColor(w as HDC, 0x00ff_ffff);
                return GetStockObject(DC_BRUSH) as isize;
            }
            WM_NCDESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<State>;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn text(bytes: &[u8]) -> String {
        let (temp, mut file) = loop {
            let path = std::env::temp_dir().join(format!(
                "rfm-preview-{}-{}.txt",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => break (Temp(path), file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temporary file: {error}"),
            }
        };
        file.write_all(bytes).unwrap();
        drop(file);
        let Content::Text(value) = load(&temp.0) else {
            panic!("text preview failed")
        };
        String::from_utf16(value.strip_suffix(&[0]).unwrap()).unwrap()
    }
    #[test]
    fn preserves_lines_in_lf_crlf_and_utf16_files() {
        assert_eq!(text(b"first\nsecond\r\nthird"), "first\r\nsecond\r\nthird");
        for little_endian in [true, false] {
            let mut bytes = if little_endian {
                vec![255, 254]
            } else {
                vec![254, 255]
            };
            for unit in "日本語\nsecond\r\nthird".encode_utf16() {
                bytes.extend(if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            assert_eq!(text(&bytes), "日本語\r\nsecond\r\nthird");
        }
    }
    #[test]
    fn truncating_utf8_keeps_complete_japanese_characters() {
        let original = "あ".repeat(87382);
        let rendered = text(original.as_bytes());
        let prefix = "あ".repeat(87381);
        assert!(rendered.starts_with(&prefix));
        assert!(rendered[prefix.len()..].starts_with("\r\n\r\n"));
        assert!(!rendered.contains('\u{fffd}'));
    }
}
