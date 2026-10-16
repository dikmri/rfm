use crate::{
    actions, breadcrumb, chrome, directory_events, fs, icons, preferences, preview, quick_access,
    settings, shell, terminal,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::OsString,
    io,
    mem::{size_of, zeroed},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{
        Com::*,
        LibraryLoader::GetModuleHandleW,
        Ole::{OleInitialize, OleUninitialize},
        SystemServices::*,
    },
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const APP_ICON_ID: usize = 1;
const READY: u32 = WM_APP + 1;
const ID_BACK: usize = 101;
const ID_FORWARD: usize = 102;
const ID_UP: usize = 103;
const ID_REFRESH: usize = 104;
const ID_ADDRESS: usize = 105;
const ID_SEARCH: usize = 106;
const ID_TABS: usize = 107;
const ID_LIST: usize = 108;
const ID_SIDE: usize = 109;
const ID_NEW_TAB: usize = 201;
const ID_CLOSE_TAB: usize = 202;
const ID_OPEN: usize = 203;
const ID_COPY: usize = 204;
const ID_CUT: usize = 205;
const ID_PASTE: usize = 206;
const ID_RENAME: usize = 207;
const ID_DELETE: usize = 208;
const ID_PERMANENT_DELETE: usize = 209;
const ID_FOLDER: usize = 210;
const ID_PROPERTIES: usize = 211;
const ID_HIDDEN: usize = 212;
const ID_SELECT_ALL: usize = 213;
const ID_HELP: usize = 214;
const ID_EXIT: usize = 215;
const ID_OPEN_TAB: usize = 216;
const ID_COPY_PATH: usize = 217;
const ID_SORT: usize = 218;
const ID_MORE: usize = 219;
const ID_SORT_NAME: usize = 220;
const ID_SORT_DATE: usize = 221;
const ID_SORT_TYPE: usize = 222;
const ID_SORT_SIZE: usize = 223;
const ID_SORT_REVERSE: usize = 224;
const ID_PIN: usize = 230;
const ID_PICK_PIN: usize = 231;
const ID_UNPIN: usize = 232;
const ID_TOOLBAR: usize = 233;
const ID_EXTENSIONS: usize = 234;
const ID_CHECKS: usize = 235;
const ID_VIEW_DETAILS: usize = 236;
const ID_VIEW_LIST: usize = 237;
const ID_VIEW_SMALL: usize = 238;
const ID_VIEW_ICONS: usize = 239;
const ID_PREVIEW: usize = 240;
const ID_NEW_WINDOW: usize = 241;
const ID_SHARE: usize = 242;
const ID_SETTINGS: usize = 243;
const ID_OPEN_TERMINAL: usize = 244;
const ID_HISTORY: usize = 245;
const REFRESH_TIMER: usize = 1;
const PIN_REFRESH_TIMER: usize = 2;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    // Owner-data callbacks can run synchronously inside SendMessage. Keep an
    // independent immutable snapshot instead of re-borrowing the mutable app.
    static DISPLAY: RefCell<Option<Display>> = const { RefCell::new(None) };
    static EVENTS: RefCell<Option<Sender<Completion>>> = const { RefCell::new(None) };
    static SIDEBAR: RefCell<Option<SidebarDisplay>> = const { RefCell::new(None) };
    static LABEL_EDIT: RefCell<Option<RenameTarget>> = const { RefCell::new(None) };
}

struct Row {
    entry: fs::Entry,
    name: Vec<u16>,
    search: String,
    kind: Rc<Vec<u16>>,
    icon: i32,
}

#[derive(Clone)]
struct Display {
    rows: Rc<Vec<Row>>,
    view: Rc<Vec<usize>>,
    show_extensions: bool,
    item_checks: bool,
    tab_id: u64,
    rename_allowed: bool,
}

struct SidebarDisplay {
    places: Rc<Vec<Place>>,
    style: chrome::Style,
    icon_theme: settings::IconTheme,
}

struct Visit {
    path: PathBuf,
    selected: Vec<PathBuf>,
    scroll: i32,
}

impl Visit {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            selected: Vec::new(),
            scroll: 0,
        }
    }
}

fn child_at(parent: &Path, descendant: &Path) -> Option<PathBuf> {
    match descendant.strip_prefix(parent).ok()?.components().next()? {
        std::path::Component::Normal(name) => Some(parent.join(name)),
        _ => None,
    }
}

struct Tab {
    id: u64,
    path: PathBuf,
    history: Vec<Visit>,
    history_at: usize,
    rows: Rc<Vec<Row>>,
    view: Rc<Vec<usize>>,
    filter: String,
    selected: Vec<PathBuf>,
    scroll: i32,
    sort: usize,
    descending: bool,
    order: Vec<usize>,
    request: u64,
    inflight: bool,
    loading: bool,
    error: Option<String>,
    recursive: bool,
    skipped: usize,
    cancel: Arc<AtomicBool>,
    watcher: Option<directory_events::Watcher>,
    watch_error: Option<String>,
    dirty: bool,
    view_mode: u32,
}

impl Tab {
    fn new(id: u64, path: PathBuf) -> Self {
        Self {
            id,
            history: vec![Visit::new(path.clone())],
            path,
            history_at: 0,
            rows: Rc::new(Vec::new()),
            view: Rc::new(Vec::new()),
            filter: String::new(),
            selected: Vec::new(),
            scroll: 0,
            sort: 0,
            descending: false,
            order: Vec::new(),
            request: 0,
            inflight: false,
            loading: false,
            error: None,
            recursive: false,
            skipped: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            watcher: None,
            watch_error: None,
            dirty: false,
            view_mode: LV_VIEW_DETAILS,
        }
    }

    fn remember_visit(&mut self) {
        let visit = &mut self.history[self.history_at];
        visit.selected = std::mem::take(&mut self.selected);
        visit.scroll = self.scroll;
    }

    fn visit(&mut self, path: PathBuf) {
        if path == self.path {
            return;
        }
        if let Some(child) = child_at(&self.path, &path) {
            self.selected.clear();
            self.selected.push(child);
        }
        self.remember_visit();
        let previous = std::mem::replace(&mut self.path, path);
        self.history.truncate(self.history_at + 1);
        self.history.push(Visit::new(self.path.clone()));
        self.history_at = self.history.len() - 1;
        self.selected = child_at(&self.path, &previous).into_iter().collect();
        self.scroll = 0;
    }

    fn travel(&mut self, forward: bool) -> bool {
        let index = if forward {
            if self.history_at + 1 >= self.history.len() {
                return false;
            }
            self.history_at + 1
        } else {
            if self.history_at == 0 {
                return false;
            }
            self.history_at - 1
        };
        self.travel_to(index)
    }

    fn travel_to(&mut self, index: usize) -> bool {
        if index >= self.history.len() || index == self.history_at {
            return false;
        }
        self.remember_visit();
        self.history_at = index;
        let visit = &mut self.history[index];
        self.path = visit.path.clone();
        self.selected = std::mem::take(&mut visit.selected);
        self.scroll = visit.scroll;
        true
    }

    fn sort_entries(&mut self) {
        let mut order: Vec<usize> = (0..self.rows.len()).collect();
        order.sort_unstable_by(|&a, &b| {
            let a = &self.rows[a];
            let b = &self.rows[b];
            b.entry.is_dir.cmp(&a.entry.is_dir).then_with(|| {
                let order = match self.sort {
                    1 => a.entry.size.cmp(&b.entry.size),
                    2 => a.kind.cmp(&b.kind),
                    3 => a.entry.modified.cmp(&b.entry.modified),
                    _ => unsafe { StrCmpLogicalW(a.name.as_ptr(), b.name.as_ptr()) }.cmp(&0),
                }
                .then_with(|| a.entry.name.cmp(&b.entry.name));
                if self.descending {
                    order.reverse()
                } else {
                    order
                }
            })
        });
        self.order = order;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        if self.recursive {
            self.view = Rc::new(self.order.clone());
        } else {
            let query = self.filter.to_lowercase();
            self.view = Rc::new(
                self.order
                    .iter()
                    .copied()
                    .filter(|&i| self.rows[i].search.contains(&query))
                    .collect(),
            );
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct RenameTarget {
    tab_id: u64,
    path: PathBuf,
    suffix: Option<String>,
}

impl RenameTarget {
    fn proposed_name(&self, value: Option<String>) -> Option<String> {
        let mut name = value.filter(|name| !name.is_empty())?;
        if let Some(suffix) = &self.suffix {
            name.push('.');
            name.push_str(suffix);
        }
        (std::ffi::OsStr::new(&name) != self.path.file_name().unwrap_or_default()).then_some(name)
    }
}

enum Completion {
    Listing {
        id: u64,
        request: u64,
        result: io::Result<(Vec<fs::Entry>, usize)>,
    },
    Operation {
        result: io::Result<()>,
        clipboard_sequence: Option<u32>,
        renamed: Option<(u64, PathBuf, PathBuf)>,
    },
    QuickAccess {
        generation: u64,
        result: io::Result<Vec<quick_access::Pin>>,
    },
    PcPlaces {
        generation: u64,
        places: Vec<Place>,
    },
    QuickAccessFinished,
    PlaceIcon {
        generation: u64,
        path: PathBuf,
        image: i32,
    },
    PlaceIconsFinished {
        result: io::Result<()>,
        generation: u64,
    },
    Changed(io::Result<Vec<PathBuf>>),
    LabelEditFinished {
        target: RenameTarget,
        value: Option<String>,
    },
}

enum Operation {
    Copy(Vec<PathBuf>, PathBuf),
    Move(Vec<PathBuf>, PathBuf),
    Rename {
        tab_id: u64,
        path: PathBuf,
        name: String,
    },
    Folder(PathBuf, String),
    Delete(Vec<PathBuf>),
}

#[derive(Clone, PartialEq, Eq)]
struct Place {
    label: Vec<u16>,
    glyph: u16,
    // Structure/labels stay immutable for reentrant owner-draw callbacks. Only
    // this UI-thread-owned cell changes when the native icon arrives.
    image: Cell<Option<i32>>,
    path: Option<PathBuf>,
    quick: bool,
}

fn sidebar_position(
    previous: &[Place],
    places: &[Place],
    selected: usize,
    top: usize,
) -> (Option<usize>, usize) {
    let same_place = |old: &Place, new: &Place| {
        old.quick == new.quick
            && old.path == new.path
            && (old.path.is_some() || old.label == new.label)
    };
    let selected = previous
        .get(selected)
        .and_then(|old| places.iter().position(|new| same_place(old, new)));
    // If the top row disappeared, anchor to the next surviving row, or the
    // nearest preceding row. Never restore by index after inserted/removed pins.
    let top = previous
        .iter()
        .skip(top)
        .chain(previous.iter().take(top).rev())
        .find_map(|old| places.iter().position(|new| same_place(old, new)))
        .unwrap_or(top.min(places.len().saturating_sub(1)));
    (selected, top)
}

struct IconRequest {
    generation: u64,
    paths: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
}

struct App {
    hwnd: HWND,
    tabs_hwnd: HWND,
    list: HWND,
    side: HWND,
    sidebar_host: HWND,
    address: HWND,
    address_bar: HWND,
    search: HWND,
    status: HWND,
    buttons: Vec<HWND>,
    history_button: HWND,
    font: HFONT,
    icon_font: HFONT,
    window_icon: HICON,
    dpi: u32,
    icon_theme: settings::IconTheme,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    shortcuts: Rc<Vec<Place>>,
    show_hidden: bool,
    busy: bool,
    tx: Sender<Completion>,
    rx: Receiver<Completion>,
    icons: HashMap<(bool, OsString), i32>,
    pins: Vec<quick_access::Pin>,
    pc_places: Vec<Place>,
    pin_error: Option<String>,
    show_toolbar: bool,
    show_extensions: bool,
    item_checks: bool,
    sidebar_width: i32,
    resizing_side: bool,
    pins_pending: bool,
    pins_refresh_requested: bool,
    pins_generation: u64,
    pins_cancel: Arc<AtomicBool>,
    pc_refresh_requested: bool,
    pin_icons_pending: bool,
    pin_icons_requested: Option<IconRequest>,
    pin_icon_paths: HashSet<PathBuf>,
    preview: Option<preview::Panel>,
    preview_visible: bool,
    kinds: HashMap<(bool, OsString), Rc<Vec<u16>>>,
}

fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

fn text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0; len + 1];
        let count = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..count.max(0) as usize])
    }
}

fn set_text(hwnd: HWND, value: impl AsRef<std::ffi::OsStr>) {
    unsafe {
        SetWindowTextW(hwnd, wide(value).as_ptr());
    }
}

fn error(owner: HWND, message: impl std::fmt::Display) {
    unsafe {
        MessageBoxW(
            owner,
            wide(message.to_string()).as_ptr(),
            tr_w!("rfm — エラー", "rfm — Error"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn confirm(owner: HWND, message: &str) -> bool {
    unsafe {
        MessageBoxW(
            owner,
            wide(message).as_ptr(),
            tr_w!("rfm — 確認", "rfm — Confirm"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    unsafe {
        let mut ptr = null_mut();
        if SHGetKnownFolderPath(id, 0, null_mut(), &mut ptr) < 0 || ptr.is_null() {
            return None;
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(ptr, len)));
        CoTaskMemFree(ptr.cast());
        Some(path)
    }
}

fn drive_icon(path: &Path) -> Option<i32> {
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };
    unsafe {
        let stock =
            match windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide(path).as_ptr()) {
                DRIVE_CDROM => SIID_DRIVECD,
                DRIVE_RAMDISK => SIID_DRIVERAM,
                DRIVE_REMOTE => SIID_DRIVENET,
                DRIVE_REMOVABLE => SIID_DRIVEREMOVE,
                _ => SIID_DRIVEFIXED,
            };
        let mut info = SHSTOCKICONINFO {
            cbSize: size_of::<SHSTOCKICONINFO>() as u32,
            ..zeroed()
        };
        (SHGetStockIconInfo(stock, SHGSI_SYSICONINDEX | SHGSI_SMALLICON, &mut info) >= 0)
            .then_some(info.iSysImageIndex)
    }
}

fn drive_label(path: &Path) -> String {
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE};
    let drive = path.display().to_string();
    let mut label = [0u16; 261];
    unsafe {
        let root = wide(path);
        let kind = windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr());
        if matches!(kind, DRIVE_FIXED | DRIVE_REMOTE)
            && windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            ) != 0
        {
            let length = label.iter().position(|&c| c == 0).unwrap_or(label.len());
            if length > 0 {
                return format!(
                    "{} ({})",
                    String::from_utf16_lossy(&label[..length]),
                    drive.trim_end_matches('\\')
                );
            }
        }
    }
    tr_format!("ドライブ ({})", "Drive ({})", drive.trim_end_matches('\\'))
}

// This snapshot is refreshed at startup/F5, not whenever a Quick Access icon
// arrives. All shell and volume lookups run off the window thread.
fn pc_places() -> Vec<Place> {
    let mut places = Vec::with_capacity(32);
    let folder_image = shell::icon_index(Path::new("folder"), true);
    for (label, glyph, id) in [
        (tr!("デスクトップ", "Desktop"), 0xE7F4, &FOLDERID_Desktop),
        (
            tr!("ダウンロード", "Downloads"),
            0xE896,
            &FOLDERID_Downloads,
        ),
        (
            tr!("ドキュメント", "Documents"),
            0xE8A5,
            &FOLDERID_Documents,
        ),
        (tr!("ピクチャ", "Pictures"), 0xEB9F, &FOLDERID_Pictures),
        (tr!("ミュージック", "Music"), 0xE8D6, &FOLDERID_Music),
        (tr!("ビデオ", "Videos"), 0xE714, &FOLDERID_Videos),
    ] {
        if let Some(path) = known_folder(id) {
            places.push(Place {
                label: wide(label),
                glyph,
                image: Cell::new(Some(folder_image)),
                path: Some(path),
                quick: false,
            });
        }
    }
    for path in fs::drives() {
        places.push(Place {
            label: wide(drive_label(&path)),
            glyph: 0,
            image: Cell::new(drive_icon(&path)),
            path: Some(path),
            quick: false,
        });
    }
    places
}

fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

struct CellText {
    value: [u16; 512],
    length: usize,
}
impl CellText {
    fn new() -> Self {
        Self {
            value: [0; 512],
            length: 0,
        }
    }
    fn push(&mut self, value: u16) {
        if self.length < self.value.len() - 1 {
            self.value[self.length] = value;
            self.length += 1;
        }
    }
    fn text(&mut self, value: &str) {
        for unit in value.encode_utf16() {
            self.push(unit);
        }
    }
    fn number(&mut self, mut number: u64, minimum: usize, grouping: bool) {
        let mut digits = [0u16; 20];
        let mut count = 0;
        loop {
            digits[count] = b'0' as u16 + (number % 10) as u16;
            count += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        for _ in count..minimum {
            self.push(b'0' as u16);
        }
        for i in (0..count).rev() {
            self.push(digits[i]);
            if grouping && i > 0 && i % 3 == 0 {
                self.push(b',' as u16);
            }
        }
    }
    fn date(&mut self, value: Option<SystemTime>) {
        let Some(seconds) = value
            .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
            .map(|v| v.as_secs())
        else {
            return;
        };
        let ticks = seconds
            .saturating_add(11_644_473_600)
            .saturating_mul(10_000_000);
        let utc = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        unsafe {
            let mut local: FILETIME = zeroed();
            let mut time: SYSTEMTIME = zeroed();
            if windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime(&utc, &mut local)
                == 0
                || windows_sys::Win32::System::Time::FileTimeToSystemTime(&local, &mut time) == 0
            {
                return;
            }
            self.number(time.wYear as u64, 4, false);
            self.text("/");
            self.number(time.wMonth as u64, 2, false);
            self.text("/");
            self.number(time.wDay as u64, 2, false);
            self.text(" ");
            self.number(time.wHour as u64, 2, false);
            self.text(":");
            self.number(time.wMinute as u64, 2, false);
        }
    }
    fn slice(&self) -> &[u16] {
        &self.value[..=self.length]
    }
}

impl App {
    unsafe fn new(
        hwnd: HWND,
        paths: Vec<PathBuf>,
        icon_theme: settings::IconTheme,
    ) -> io::Result<Self> {
        unsafe {
            let (tx, rx) = mpsc::channel();
            EVENTS.with(|events| *events.borrow_mut() = Some(tx.clone()));
            let dpi = GetDpiForWindow(hwnd).max(96);
            icons::prepare(hwnd, icon_theme, dpi)?;
            let font = Self::make_font(dpi);
            let icon_font = Self::make_icon_font(dpi);
            let style = chrome::Style {
                font,
                icons: icon_font,
                dpi,
            };
            let child = |class, label: &str, style, id| {
                let h = CreateWindowExW(
                    0,
                    class,
                    wide(label).as_ptr(),
                    WS_CHILD | WS_VISIBLE | style,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    id as HMENU,
                    GetModuleHandleW(null()),
                    null(),
                );
                SendMessageW(h, WM_SETFONT, font as usize, 0);
                h
            };
            let mut buttons = Vec::new();
            for (id, glyph, label, tooltip) in [
                (ID_BACK, 0xE72B, "", tr!("戻る (Alt+←)", "Back (Alt+←)")),
                (
                    ID_FORWARD,
                    0xE72A,
                    "",
                    tr!("進む (Alt+→)", "Forward (Alt+→)"),
                ),
                (ID_UP, 0xE74A, "", tr!("親フォルダー (Alt+↑)", "Up (Alt+↑)")),
                (
                    ID_REFRESH,
                    0xE72C,
                    "",
                    tr!("最新の情報に更新 (F5)", "Refresh (F5)"),
                ),
                (
                    ID_FOLDER,
                    0xE710,
                    tr!("新規フォルダー", "New folder"),
                    tr!(
                        "新しいフォルダー (Ctrl+Shift+N)",
                        "New folder (Ctrl+Shift+N)"
                    ),
                ),
                (ID_CUT, 0xE8C6, "", tr!("切り取り (Ctrl+X)", "Cut (Ctrl+X)")),
                (ID_COPY, 0xE8C8, "", tr!("コピー (Ctrl+C)", "Copy (Ctrl+C)")),
                (
                    ID_PASTE,
                    0xE77F,
                    "",
                    tr!("貼り付け (Ctrl+V)", "Paste (Ctrl+V)"),
                ),
                (ID_RENAME, 0xE8AC, "", tr!("名前の変更 (F2)", "Rename (F2)")),
                (
                    ID_DELETE,
                    0xE74D,
                    "",
                    tr!("ごみ箱に移動 (Delete)", "Move to Recycle Bin (Delete)"),
                ),
                (
                    ID_SORT,
                    0xE8CB,
                    tr!("並べ替え", "Sort"),
                    tr!("並べ替え", "Sort"),
                ),
                (ID_MORE, 0xE712, "", tr!("その他の操作", "More actions")),
            ] {
                buttons.push(chrome::create_button(
                    hwnd, id, glyph, label, tooltip, style,
                ));
            }
            let history_button = chrome::create_button(
                hwnd,
                ID_HISTORY,
                0xE70D,
                "",
                tr!("最近表示した場所", "Recent locations"),
                style,
            );
            let tabs_hwnd = chrome::create_tabs(hwnd, ID_TABS, style);
            let address_bar = breadcrumb::create(hwnd, 225, ID_ADDRESS, style);
            breadcrumb::attach_refresh(address_bar, buttons[3])?;
            let address = breadcrumb::edit(address_bar);
            let search = child(
                windows_sys::core::w!("EDIT"),
                "",
                WS_TABSTOP | ES_AUTOHSCROLL as u32,
                ID_SEARCH,
            );
            SendMessageW(
                search,
                EM_SETCUEBANNER,
                0,
                tr_w!("このフォルダーを検索", "Search this folder") as isize,
            );
            let side = child(
                windows_sys::core::w!("LISTBOX"),
                "",
                WS_TABSTOP
                    | WS_VSCROLL
                    | LBS_NOTIFY as u32
                    | LBS_NOINTEGRALHEIGHT as u32
                    | LBS_OWNERDRAWFIXED as u32
                    | LBS_HASSTRINGS as u32,
                ID_SIDE,
            );
            let sidebar_host = chrome::create_sidebar_host(hwnd);
            if side.is_null() || sidebar_host.is_null() {
                return Err(io::Error::last_os_error());
            }
            if SetParent(side, sidebar_host).is_null() {
                return Err(io::Error::last_os_error());
            }
            chrome::style_sidebar(side, style);
            let list = child(
                WC_LISTVIEWW,
                "",
                WS_TABSTOP
                    | LVS_REPORT
                    | LVS_OWNERDATA
                    | LVS_SHOWSELALWAYS
                    | LVS_SHAREIMAGELISTS
                    | LVS_EDITLABELS,
                ID_LIST,
            );
            SetWindowTheme(list, windows_sys::core::w!("Explorer"), null());
            SendMessageW(list, LVM_SETBKCOLOR, 0, 0x00ff_ffff);
            SendMessageW(list, LVM_SETTEXTBKCOLOR, 0, 0x00ff_ffff);
            SendMessageW(list, LVM_SETTEXTCOLOR, 0, 0x0020_2020);
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                0,
                (LVS_EX_FULLROWSELECT
                    | LVS_EX_DOUBLEBUFFER
                    | LVS_EX_LABELTIP
                    | LVS_EX_HEADERDRAGDROP) as isize,
            );
            SendMessageW(
                list,
                LVM_SETCALLBACKMASK,
                (LVIS_CUT | LVIS_STATEIMAGEMASK) as usize,
                0,
            );
            if SetWindowSubclass(list, Some(list_subclass), 1, 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            SendMessageW(
                list,
                LVM_SETIMAGELIST,
                LVSIL_SMALL as usize,
                shell::system_image_list() as isize,
            );
            for (i, (label, width)) in [
                (tr!("名前", "Name"), 260),
                (tr!("更新日時", "Date modified"), 160),
                (tr!("種類", "Type"), 120),
                (tr!("サイズ", "Size"), 84),
            ]
            .iter()
            .enumerate()
            {
                let mut title = wide(label);
                let column = LVCOLUMNW {
                    mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                    fmt: if i == 3 { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                    cx: width * dpi as i32 / 96,
                    pszText: title.as_mut_ptr(),
                    ..zeroed()
                };
                SendMessageW(list, LVM_INSERTCOLUMNW, i, &column as *const _ as isize);
            }
            let status = child(
                windows_sys::core::w!("STATIC"),
                "",
                SS_LEFT | SS_CENTERIMAGE | SS_ENDELLIPSIS,
                110,
            );
            let window_icon = LoadImageW(
                GetModuleHandleW(null()),
                APP_ICON_ID as _,
                IMAGE_ICON,
                GetSystemMetricsForDpi(SM_CXSMICON, dpi),
                GetSystemMetricsForDpi(SM_CYSMICON, dpi),
                LR_DEFAULTCOLOR,
            ) as HICON;
            if window_icon.is_null() {
                return Err(io::Error::last_os_error());
            }
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, window_icon as isize);
            let mut app = Self {
                hwnd,
                tabs_hwnd,
                list,
                side,
                sidebar_host,
                address,
                address_bar,
                search,
                status,
                buttons,
                history_button,
                font,
                icon_font,
                window_icon,
                dpi,
                icon_theme,
                tabs: Vec::new(),
                active: 0,
                next_id: 1,
                shortcuts: Rc::new(Vec::new()),
                show_hidden: false,
                busy: false,
                tx,
                rx,
                icons: HashMap::new(),
                pins: Vec::new(),
                pc_places: Vec::new(),
                pins_pending: false,
                pins_refresh_requested: false,
                pins_generation: 0,
                pins_cancel: Arc::new(AtomicBool::new(false)),
                pc_refresh_requested: false,
                pin_icons_pending: false,
                pin_icons_requested: None,
                pin_icon_paths: HashSet::new(),
                pin_error: None,
                show_toolbar: false,
                show_extensions: true,
                item_checks: false,
                sidebar_width: 152,
                resizing_side: false,
                preview: None,
                preview_visible: false,
                kinds: HashMap::new(),
            };
            app.make_menu();
            app.refresh_places();
            app.reload_pins(true);
            for path in paths {
                app.add_tab(path);
            }
            Ok(app)
        }
    }

    unsafe fn make_font(dpi: u32) -> HFONT {
        unsafe {
            let mut metrics: NONCLIENTMETRICSW = zeroed();
            metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
            if SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS,
                metrics.cbSize,
                &mut metrics as *mut _ as _,
                0,
                dpi,
            ) != 0
            {
                return CreateFontIndirectW(&metrics.lfMessageFont);
            }
            CreateFontW(
                -(13 * dpi as i32 / 96),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                windows_sys::core::w!("Segoe UI"),
            )
        }
    }

    unsafe fn make_icon_font(dpi: u32) -> HFONT {
        unsafe {
            CreateFontW(
                -(16 * dpi as i32 / 96),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                windows_sys::core::w!("Segoe MDL2 Assets"),
            )
        }
    }

    fn style(&self) -> chrome::Style {
        chrome::Style {
            font: self.font,
            icons: self.icon_font,
            dpi: self.dpi,
        }
    }

    fn make_menu(&self) {
        type MenuItems<'a> = &'a [(usize, &'static str)];
        unsafe {
            let bar = CreateMenu();
            let groups: &[(&str, usize, MenuItems<'_>)] = &[
                (
                    tr!("ファイル(&F)", "&File"),
                    0,
                    &[
                        (ID_NEW_TAB, tr!("新しいタブ\tCtrl+T", "New tab\tCtrl+T")),
                        (
                            ID_NEW_WINDOW,
                            tr!("新しいウィンドウ\tCtrl+N", "New window\tCtrl+N"),
                        ),
                        (
                            ID_OPEN_TERMINAL,
                            tr!("ターミナルで開く", "Open in terminal"),
                        ),
                        (
                            ID_CLOSE_TAB,
                            tr!("タブを閉じる\tCtrl+W", "Close tab\tCtrl+W"),
                        ),
                        (0, ""),
                        (ID_PIN, tr!("クイックアクセスに追加", "Pin to Quick access")),
                        (
                            ID_PICK_PIN,
                            tr!("追加するフォルダーを選択…", "Choose folders to pin…"),
                        ),
                        (
                            ID_UNPIN,
                            tr!("現在のフォルダーのピン留めを解除", "Unpin current folder"),
                        ),
                        (0, ""),
                        (
                            ID_PROPERTIES,
                            tr!("プロパティ\tAlt+Enter", "Properties\tAlt+Enter"),
                        ),
                        (ID_SETTINGS, tr!("設定…", "Settings…")),
                        (ID_HELP, tr!("操作方法\tF1", "Help\tF1")),
                        (ID_EXIT, tr!("終了", "Exit")),
                    ],
                ),
                (
                    tr!("ホーム(&H)", "&Home"),
                    0,
                    &[
                        (
                            ID_FOLDER,
                            tr!("新しいフォルダー\tCtrl+Shift+N", "New folder\tCtrl+Shift+N"),
                        ),
                        (0, ""),
                        (ID_CUT, tr!("切り取り\tCtrl+X", "Cut\tCtrl+X")),
                        (ID_COPY, tr!("コピー\tCtrl+C", "Copy\tCtrl+C")),
                        (ID_PASTE, tr!("貼り付け\tCtrl+V", "Paste\tCtrl+V")),
                        (
                            ID_COPY_PATH,
                            tr!("パスのコピー\tCtrl+Shift+C", "Copy path\tCtrl+Shift+C"),
                        ),
                        (0, ""),
                        (ID_RENAME, tr!("名前の変更\tF2", "Rename\tF2")),
                        (
                            ID_DELETE,
                            tr!("ごみ箱へ移動\tDelete", "Move to Recycle Bin\tDelete"),
                        ),
                        (
                            ID_PERMANENT_DELETE,
                            tr!(
                                "完全に削除\tShift+Delete",
                                "Delete permanently\tShift+Delete"
                            ),
                        ),
                        (
                            ID_SELECT_ALL,
                            tr!("すべて選択\tCtrl+A", "Select all\tCtrl+A"),
                        ),
                    ],
                ),
                (tr!("共有(&S)", "&Share"), ID_SHARE, &[]),
                (
                    tr!("表示(&V)", "&View"),
                    0,
                    &[
                        (ID_VIEW_ICONS, tr!("中アイコン", "Medium icons")),
                        (ID_VIEW_SMALL, tr!("小アイコン", "Small icons")),
                        (ID_VIEW_LIST, tr!("一覧", "List")),
                        (ID_VIEW_DETAILS, tr!("詳細", "Details")),
                        (0, ""),
                        (ID_SORT_NAME, tr!("名前で並べ替え", "Sort by name")),
                        (
                            ID_SORT_DATE,
                            tr!("更新日時で並べ替え", "Sort by date modified"),
                        ),
                        (ID_SORT_TYPE, tr!("種類で並べ替え", "Sort by type")),
                        (ID_SORT_SIZE, tr!("サイズで並べ替え", "Sort by size")),
                        (ID_SORT_REVERSE, tr!("降順", "Descending")),
                        (0, ""),
                        (ID_HIDDEN, tr!("隠しファイル", "Hidden files")),
                        (
                            ID_EXTENSIONS,
                            tr!("ファイル名拡張子", "File name extensions"),
                        ),
                        (ID_CHECKS, tr!("項目チェックボックス", "Item check boxes")),
                        (ID_TOOLBAR, tr!("コンパクトな操作バー", "Compact toolbar")),
                        (
                            ID_PREVIEW,
                            tr!("プレビュー ウィンドウ\tAlt+P", "Preview pane\tAlt+P"),
                        ),
                        (ID_REFRESH, tr!("最新の情報に更新\tF5", "Refresh\tF5")),
                    ],
                ),
            ];
            for &(title, command, items) in groups {
                if command != 0 {
                    AppendMenuW(bar, MF_STRING, command, wide(title).as_ptr());
                    continue;
                }
                let menu = CreatePopupMenu();
                for &(id, label) in items {
                    AppendMenuW(
                        menu,
                        if id == 0 { MF_SEPARATOR } else { MF_STRING },
                        id,
                        wide(label).as_ptr(),
                    );
                }
                AppendMenuW(bar, MF_POPUP, menu as usize, wide(title).as_ptr());
            }
            SetMenu(self.hwnd, bar);
            CheckMenuItem(bar, ID_EXTENSIONS as u32, MF_CHECKED);
            CheckMenuItem(bar, ID_VIEW_DETAILS as u32, MF_CHECKED);
        }
    }

    fn publish_places(&self) {
        SIDEBAR.with(|display| {
            *display.borrow_mut() = Some(SidebarDisplay {
                places: self.shortcuts.clone(),
                style: self.style(),
                icon_theme: self.icon_theme,
            })
        });
    }

    fn refresh_places(&mut self) {
        let mut shortcuts = Vec::with_capacity(self.pins.len() + 2 + self.pc_places.len());
        shortcuts.push(Place {
            label: wide(tr!("クイック アクセス", "Quick access")),
            glyph: 0,
            path: None,
            image: Cell::new(None),
            quick: false,
        });
        for pin in &self.pins {
            shortcuts.push(Place {
                label: wide(&pin.name),
                glyph: 0,
                path: Some(pin.path.clone()),
                image: Cell::new(Some(pin.image)),
                quick: true,
            });
        }
        shortcuts.push(Place {
            label: wide("PC"),
            glyph: 0,
            path: None,
            image: Cell::new(None),
            quick: false,
        });
        shortcuts.extend(self.pc_places.iter().cloned());
        if *self.shortcuts == shortcuts {
            return;
        }

        unsafe {
            let selected = SendMessageW(self.side, LB_GETCURSEL, 0, 0) as usize;
            let top = SendMessageW(self.side, LB_GETTOPINDEX, 0, 0).max(0) as usize;
            let (selected, restored_top) =
                sidebar_position(&self.shortcuts, &shortcuts, selected, top);
            SendMessageW(self.side, WM_SETREDRAW, 0, 0);
            SendMessageW(self.side, LB_RESETCONTENT, 0, 0);
            for place in &shortcuts {
                SendMessageW(self.side, LB_ADDSTRING, 0, place.label.as_ptr() as isize);
            }
            self.shortcuts = Rc::new(shortcuts);
            self.publish_places();
            SendMessageW(self.side, LB_SETCURSEL, selected.unwrap_or(usize::MAX), 0);
            if selected.is_none() && !self.tabs.is_empty() {
                self.sync_sidebar();
            }
            SendMessageW(self.side, LB_SETTOPINDEX, restored_top, 0);
            SendMessageW(self.side, WM_SETREDRAW, 1, 0);
            InvalidateRect(self.side, null(), 0);
        }
    }

    fn reload_pins(&mut self, refresh_pc: bool) {
        self.pins_generation = self.pins_generation.wrapping_add(1);
        self.pins_cancel.store(true, Ordering::Relaxed);
        self.pins_cancel = Arc::new(AtomicBool::new(false));
        self.pin_icons_requested = None;
        self.pin_icon_paths.clear();
        self.pc_refresh_requested |= refresh_pc;
        if self.pins_pending {
            self.pins_refresh_requested = true;
            return;
        }
        self.start_pins();
    }

    fn start_pins(&mut self) {
        self.pins_pending = true;
        let generation = self.pins_generation;
        let cancel = self.pins_cancel.clone();
        let refresh_pc = self.pc_refresh_requested || self.pc_places.is_empty();
        let tx = self.tx.clone();
        let hwnd = self.hwnd as usize;
        std::thread::spawn(move || {
            let publish = |completion| {
                if tx.send(completion).is_ok() {
                    unsafe { PostMessageW(hwnd as HWND, READY, 0, 0) };
                }
            };
            match quick_access::Apartment::sta() {
                Ok(_apartment) => {
                    let result = quick_access::folders();
                    if !cancel.load(Ordering::Relaxed) {
                        publish(Completion::QuickAccess { generation, result });
                        if refresh_pc && !cancel.load(Ordering::Relaxed) {
                            publish(Completion::PcPlaces {
                                generation,
                                places: pc_places(),
                            });
                        }
                    }
                }
                Err(error) => publish(Completion::QuickAccess {
                    generation,
                    result: Err(error),
                }),
            }
            publish(Completion::QuickAccessFinished);
        });
    }

    fn request_place_icons(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<_> = paths
            .into_iter()
            .filter(|path| self.pin_icon_paths.insert(path.clone()))
            .collect();
        if paths.is_empty() {
            return;
        }
        let request = IconRequest {
            generation: self.pins_generation,
            paths,
            cancel: self.pins_cancel.clone(),
        };
        if self.pin_icons_pending {
            if let Some(pending) = &mut self.pin_icons_requested {
                pending.paths.extend(request.paths);
            } else {
                self.pin_icons_requested = Some(request);
            }
        } else {
            self.start_place_icons(request);
        }
    }

    fn start_place_icons(&mut self, request: IconRequest) {
        self.pin_icons_pending = true;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as usize;
        std::thread::spawn(move || {
            let publish = |completion| {
                if tx.send(completion).is_ok() {
                    unsafe { PostMessageW(hwnd as HWND, READY, 0, 0) };
                }
            };
            let generation = request.generation;
            let result = (|| {
                let _apartment = quick_access::Apartment::sta()?;
                for path in request.paths {
                    if request.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let image = shell::folder_icon_index(&path);
                    // SHGetFileInfo itself cannot be cancelled. Never publish an
                    // old generation after that call eventually returns.
                    if request.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    publish(Completion::PlaceIcon {
                        generation,
                        path,
                        image,
                    });
                }
                Ok(())
            })();
            publish(Completion::PlaceIconsFinished { generation, result });
        });
    }

    fn apply_place_icon(&mut self, path: &Path, image: i32) {
        for pin in &mut self.pins {
            if pin.path.as_path() == path {
                pin.image = image;
            }
        }
        for place in &self.pc_places {
            if place.path.as_deref() == Some(path) {
                place.image.set(Some(image));
            }
        }
        unsafe {
            let mut client: RECT = zeroed();
            GetClientRect(self.side, &mut client);
            for (index, place) in self.shortcuts.iter().enumerate() {
                if place.path.as_deref() != Some(path)
                    || place.image.replace(Some(image)) == Some(image)
                {
                    continue;
                }
                let mut row: RECT = zeroed();
                if SendMessageW(
                    self.side,
                    LB_GETITEMRECT,
                    index,
                    &mut row as *mut RECT as isize,
                ) != LB_ERR as isize
                    && row.bottom > client.top
                    && row.top < client.bottom
                {
                    InvalidateRect(self.side, &row, 0);
                }
            }
        }
    }

    fn header_height(&self) -> i32 {
        if self.show_toolbar { 80 } else { 54 }
    }

    fn arm_watcher(&mut self, index: usize) {
        let result = directory_events::Watcher::new(self.hwnd, &self.tabs[index].path);
        let tab = &mut self.tabs[index];
        match result {
            Ok(watcher) => {
                tab.watcher = Some(watcher);
                tab.watch_error = None;
            }
            Err(err) => {
                tab.watcher = None;
                tab.watch_error = Some(tr_format!(
                    "自動更新を開始できません: {err}",
                    "Cannot start automatic refresh: {err}"
                ));
            }
        }
    }

    fn refresh_dirty(&mut self) {
        if self.busy {
            return;
        }
        let editing = LABEL_EDIT.with(|edit| edit.borrow().as_ref().map(|edit| edit.tab_id));
        if self.current().dirty && editing != Some(self.current().id) && !self.current().loading {
            self.save_selection();
        }
        for index in 0..self.tabs.len() {
            if self.tabs[index].dirty
                && !self.tabs[index].inflight
                && editing != Some(self.tabs[index].id)
            {
                self.tabs[index].dirty = false;
                self.reload(index);
            }
        }
    }

    fn check_menu(&self, id: usize, checked: bool) {
        unsafe {
            CheckMenuItem(
                GetMenu(self.hwnd),
                id as u32,
                MF_BYCOMMAND | if checked { MF_CHECKED } else { MF_UNCHECKED },
            );
        }
    }

    fn pins_changed(&mut self) {
        self.reload_pins(false);
        unsafe {
            SetTimer(self.hwnd, PIN_REFRESH_TIMER, 500, None);
        }
    }

    fn pin_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if let Err(err) = quick_access::pin(self.hwnd, &paths) {
            error(self.hwnd, err);
        }
        self.pins_changed();
    }

    fn sidebar_context(&mut self, mut point: POINT) {
        let keyboard = point.x == -1 && point.y == -1;
        let index = unsafe {
            if keyboard {
                let index = SendMessageW(self.side, LB_GETCURSEL, 0, 0);
                let mut rect: RECT = zeroed();
                GetWindowRect(self.side, &mut rect);
                point = POINT {
                    x: rect.left + self.scale(24),
                    y: rect.top + self.scale(24),
                };
                index as usize
            } else {
                let mut local = point;
                ScreenToClient(self.side, &mut local);
                let item = SendMessageW(
                    self.side,
                    LB_ITEMFROMPOINT,
                    0,
                    ((local.y as u16 as u32) << 16 | local.x as u16 as u32) as isize,
                ) as u32;
                if item >> 16 != 0 {
                    usize::MAX
                } else {
                    (item & 0xffff) as usize
                }
            }
        };
        let path = self
            .shortcuts
            .get(index)
            .and_then(|place| place.path.clone());
        let removable = self.shortcuts.get(index).is_some_and(|place| place.quick)
            && path.as_ref().is_some_and(|path| {
                self.pins
                    .iter()
                    .find(|pin| &pin.path == path)
                    .is_some_and(|pin| pin.pinned != Some(false))
            });
        unsafe {
            let menu = CreatePopupMenu();
            for (id, label, enabled) in [
                (ID_OPEN, tr!("開く", "Open"), path.is_some()),
                (
                    ID_OPEN_TAB,
                    tr!("新しいタブで開く", "Open in new tab"),
                    path.is_some(),
                ),
                (
                    ID_OPEN_TERMINAL,
                    tr!("ターミナルで開く", "Open in terminal"),
                    path.is_some(),
                ),
                (
                    ID_PIN,
                    tr!("クイックアクセスに追加", "Pin to Quick access"),
                    path.is_some(),
                ),
                (
                    ID_UNPIN,
                    tr!(
                        "クイックアクセスのピン留めを解除",
                        "Unpin from Quick access"
                    ),
                    removable,
                ),
                (0, "", true),
                (
                    ID_PICK_PIN,
                    tr!("追加するフォルダーを選択…", "Choose folders to pin…"),
                    true,
                ),
            ] {
                AppendMenuW(
                    menu,
                    if id == 0 {
                        MF_SEPARATOR
                    } else {
                        MF_STRING | if enabled { 0 } else { MF_GRAYED }
                    },
                    id,
                    wide(label).as_ptr(),
                );
            }
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            ) as usize;
            DestroyMenu(menu);
            match (command, path) {
                (ID_OPEN, Some(path)) => self.navigate(path),
                (ID_OPEN_TAB, Some(path)) => self.add_tab(path),
                (ID_OPEN_TERMINAL, Some(path)) => self.open_terminal(&path),
                (ID_PIN, Some(path)) => self.pin_paths(vec![path]),
                (ID_UNPIN, Some(path)) => {
                    if let Err(err) = quick_access::unpin(self.hwnd, &path) {
                        error(self.hwnd, err);
                    }
                    self.pins_changed();
                }
                (ID_PICK_PIN, _) => self.command(ID_PICK_PIN),
                _ => {}
            }
        }
    }

    fn toolbar_menu(&mut self) {
        unsafe {
            let menu = CreatePopupMenu();
            let sort_items = [
                (ID_SORT_NAME, tr!("名前", "Name")),
                (ID_SORT_DATE, tr!("更新日時", "Date modified")),
                (ID_SORT_TYPE, tr!("種類", "Type")),
                (ID_SORT_SIZE, tr!("サイズ", "Size")),
                (0, ""),
                (ID_SORT_REVERSE, tr!("降順", "Descending")),
            ];
            let checked_sort =
                [ID_SORT_NAME, ID_SORT_SIZE, ID_SORT_TYPE, ID_SORT_DATE][self.current().sort];
            for &(id, label) in &sort_items {
                let checked =
                    id == checked_sort || (id == ID_SORT_REVERSE && self.current().descending);
                let flags = if id == 0 {
                    MF_SEPARATOR
                } else {
                    MF_STRING | if checked { MF_CHECKED } else { 0 }
                };
                AppendMenuW(menu, flags, id, wide(label).as_ptr());
            }
            let mut rect: RECT = zeroed();
            GetWindowRect(self.buttons[10], &mut rect);
            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                rect.left,
                rect.bottom + self.scale(4),
                0,
                self.hwnd,
                null(),
            ) as usize;
            DestroyMenu(menu);
            if id != 0 {
                self.command(id);
            }
        }
    }

    fn scale(&self, n: i32) -> i32 {
        n * self.dpi as i32 / 96
    }

    fn layout(&self) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let (w, h) = (rect.right, rect.bottom);
            let s = |v| self.scale(v);
            MoveWindow(self.tabs_hwnd, s(3), 0, (w - s(6)).max(0), s(24), 1);
            for (i, button) in self.buttons[..2].iter().enumerate() {
                MoveWindow(*button, s(4 + i as i32 * 30), s(26), s(24), s(24), 1);
            }
            MoveWindow(self.history_button, s(64), s(26), s(12), s(24), 1);
            MoveWindow(self.buttons[2], s(80), s(26), s(24), s(24), 1);
            let search = self.search_rect(w);
            MoveWindow(
                self.search,
                search.left + s(25),
                search.top + s(2),
                (search.right - search.left - s(30)).max(0),
                s(17),
                1,
            );
            MoveWindow(
                self.address_bar,
                s(108),
                s(27),
                (search.left - s(116)).max(s(72)),
                s(22),
                1,
            );
            for &button in &self.buttons[4..] {
                ShowWindow(button, if self.show_toolbar { SW_SHOW } else { SW_HIDE });
            }
            MoveWindow(self.buttons[4], s(6), s(55), s(122), s(23), 1);
            for (i, button) in self.buttons[5..10].iter().enumerate() {
                MoveWindow(*button, s(140 + i as i32 * 30), s(55), s(26), s(23), 1);
            }
            MoveWindow(self.buttons[10], s(300), s(55), s(100), s(23), 1);
            MoveWindow(self.buttons[11], s(410), s(55), s(26), s(23), 1);
            let top = s(self.header_height());
            MoveWindow(
                self.sidebar_host,
                s(2),
                top + s(2),
                s(self.sidebar_width - 5),
                (h - top - s(24)).max(0),
                1,
            );
            let preview_width = if self.preview_visible {
                s(260).min(w / 3)
            } else {
                0
            };
            MoveWindow(
                self.list,
                s(self.sidebar_width + 4),
                top,
                (w - s(self.sidebar_width + 6) - preview_width).max(0),
                (h - top - s(22)).max(0),
                1,
            );
            if let Some(panel) = self.preview {
                panel.layout(
                    w - preview_width,
                    top,
                    preview_width,
                    (h - top - s(22)).max(0),
                );
            }
            MoveWindow(self.status, s(8), h - s(21), (w - s(16)).max(0), s(20), 1);
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    fn search_rect(&self, width: i32) -> RECT {
        let search_width = (width / 6).clamp(self.scale(108), self.scale(220));
        RECT {
            left: width - self.scale(7) - search_width,
            top: self.scale(27),
            right: width - self.scale(7),
            bottom: self.scale(49),
        }
    }

    fn paint(&self, hdc: HDC) {
        unsafe {
            let mut rect: RECT = zeroed();
            GetClientRect(self.hwnd, &mut rect);
            let (w, h) = (rect.right, rect.bottom);
            let s = |v| self.scale(v);
            let brush = GetStockObject(DC_BRUSH) as HBRUSH;
            let fill = |rect: RECT, color| {
                SetDCBrushColor(hdc, color);
                FillRect(hdc, &rect, brush);
            };
            fill(rect, 0x00ff_ffff);
            fill(
                RECT {
                    bottom: s(24),
                    ..rect
                },
                0x00f3_f3f3,
            );
            fill(
                RECT {
                    top: h - s(22),
                    ..rect
                },
                0x00f5_f5f5,
            );
            for y in [s(self.header_height()) - 1, h - s(22)] {
                fill(
                    RECT {
                        left: 0,
                        top: y,
                        right: w,
                        bottom: y + 1,
                    },
                    0x00df_dfdf,
                );
            }
            fill(
                RECT {
                    left: s(self.sidebar_width),
                    top: s(self.header_height()),
                    right: s(self.sidebar_width) + 1,
                    bottom: h - s(22),
                },
                0x00df_dfdf,
            );
            let search = self.search_rect(w);
            let old_pen = SelectObject(hdc, GetStockObject(DC_PEN));
            let old_brush = SelectObject(hdc, brush);
            SetDCPenColor(hdc, 0x00d0_d0d0);
            SetDCBrushColor(hdc, 0x00ff_ffff);
            Rectangle(hdc, search.left, search.top, search.right, search.bottom);
            let icon_rect = RECT {
                left: search.left + s(5),
                top: search.top + s(3),
                right: search.left + s(21),
                bottom: search.top + s(19),
            };
            icons::draw(hdc, icons::Icon::Search, icon_rect, true, false, false);
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
        }
    }

    fn change_dpi(&mut self, dpi: u32) {
        unsafe {
            let old_dpi = self.dpi;
            self.dpi = dpi;
            if let Err(err) = icons::prepare(self.hwnd, self.icon_theme, dpi) {
                error(self.hwnd, err);
            }
            let (old_font, old_icons) = (self.font, self.icon_font);
            self.font = Self::make_font(dpi);
            self.icon_font = Self::make_icon_font(dpi);
            SIDEBAR.with(|display| {
                if let Some(display) = display.borrow_mut().as_mut() {
                    display.style = self.style();
                }
            });
            for &button in &self.buttons {
                chrome::set_style(button, self.style());
            }
            chrome::set_style(self.history_button, self.style());
            chrome::set_style(self.tabs_hwnd, self.style());
            chrome::set_style(self.side, self.style());
            breadcrumb::set_style(self.address_bar, self.style());
            if let Some(panel) = self.preview {
                panel.set_style(self.style());
            }
            for hwnd in [self.search, self.list, self.status] {
                SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
            }
            for column in 0..4 {
                let width = SendMessageW(self.list, LVM_GETCOLUMNWIDTH, column, 0);
                SendMessageW(
                    self.list,
                    LVM_SETCOLUMNWIDTH,
                    column,
                    width * dpi as isize / old_dpi as isize,
                );
            }
            DeleteObject(old_font);
            DeleteObject(old_icons);
            self.layout();
            RedrawWindow(
                self.hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }

    fn apply_icon_theme(&mut self, theme: settings::IconTheme) -> io::Result<()> {
        icons::prepare(self.hwnd, theme, self.dpi)?;
        self.icon_theme = theme;
        SIDEBAR.with(|display| {
            if let Some(display) = display.borrow_mut().as_mut() {
                display.icon_theme = theme;
            }
        });
        unsafe {
            RedrawWindow(
                self.hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
        Ok(())
    }

    fn new_window(&self, path: &Path) {
        if let Err(err) = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .arg(path)
                .spawn()
                .map(|_| ())
        }) {
            error(self.hwnd, err);
        }
    }

    fn open_terminal(&self, folder: &Path) {
        if let Err(err) = settings::load().and_then(|saved| terminal::open(&saved.terminal, folder))
        {
            error(self.hwnd, err);
        }
    }

    fn open_selected_terminal(&self) {
        unsafe {
            if SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) > 1 {
                error(
                    self.hwnd,
                    tr!(
                        "ターミナルで開く項目を1つだけ選択してください。",
                        "Select only one item to open in a terminal."
                    ),
                );
                return;
            }
            let tab = self.current();
            let index = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_SELECTED as isize,
            );
            let entry = tab
                .view
                .get(index as usize)
                .map(|&row| &tab.rows[row].entry);
            let folder = match entry {
                Some(entry) if entry.is_dir => entry.path.as_path(),
                Some(entry) => entry.path.parent().unwrap_or(&tab.path),
                None => &tab.path,
            };
            self.open_terminal(folder);
        }
    }

    fn current(&self) -> &Tab {
        &self.tabs[self.active]
    }
    fn current_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn cancel_rename(&self) {
        unsafe {
            SendMessageW(self.list, LVM_CANCELEDITLABEL, 0, 0);
        }
    }

    fn begin_rename(&self) {
        if self.busy || self.current().loading {
            return;
        }
        unsafe {
            if SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0) != 1 {
                return;
            }
            let index = SendMessageW(
                self.list,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_SELECTED as isize,
            );
            if index >= 0 {
                SetFocus(self.list);
                SendMessageW(self.list, LVM_EDITLABELW, index as usize, 0);
            }
        }
    }

    fn save_selection(&mut self) {
        if self.current().loading && self.current().rows.is_empty() {
            return;
        }
        let selected = self.selected_paths();
        let scroll = unsafe { SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0) as i32 };
        let tab = self.current_mut();
        tab.selected = selected;
        tab.scroll = scroll;
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let tab = self.current();
        let mut paths = Vec::new();
        let mut index: isize = -1;
        unsafe {
            loop {
                index = SendMessageW(
                    self.list,
                    LVM_GETNEXTITEM,
                    index as usize,
                    LVNI_SELECTED as isize,
                );
                if index < 0 {
                    break;
                }
                if let Some(&row) = tab.view.get(index as usize) {
                    paths.push(tab.rows[row].entry.path.clone());
                }
            }
        }
        paths
    }

    fn add_tab(&mut self, path: PathBuf) {
        if !self.tabs.is_empty() {
            self.cancel_rename();
            self.save_selection();
        }
        self.tabs.push(Tab::new(self.next_id, path));
        self.next_id += 1;
        self.active = self.tabs.len() - 1;
        self.arm_watcher(self.active);
        self.update_tab_bar();
        self.show_current();
        self.reload(self.active);
    }

    fn update_tab_bar(&self) {
        let labels: Vec<_> = self
            .tabs
            .iter()
            .map(|tab| wide(tab.path.file_name().unwrap_or(tab.path.as_os_str())))
            .collect();
        chrome::set_tabs(self.tabs_hwnd, &labels, self.active);
    }

    fn show_current(&self) {
        let tab = self.current();
        breadcrumb::set_path(self.address_bar, &tab.path);
        if text(self.search) != tab.filter {
            set_text(self.search, &tab.filter);
        }
        let name = tab
            .path
            .file_name()
            .unwrap_or(tab.path.as_os_str())
            .to_string_lossy();
        unsafe {
            SendMessageW(
                self.search,
                EM_SETCUEBANNER,
                0,
                wide(tr_format!("{name} の検索", "Search {name}")).as_ptr() as isize,
            );
        }
        self.sync_sidebar();
        set_text(self.hwnd, format!("{name} — rfm"));
        self.render_list();
    }

    fn sync_sidebar(&self) {
        let selected = self
            .shortcuts
            .iter()
            .enumerate()
            .filter_map(|(i, place)| {
                place
                    .path
                    .as_ref()
                    .filter(|path| self.current().path.starts_with(path))
                    .map(|path| (i, path.components().count()))
            })
            .max_by_key(|(_, depth)| *depth)
            .map(|(i, _)| i);
        unsafe {
            let selected = selected.unwrap_or(usize::MAX);
            if SendMessageW(self.side, LB_GETCURSEL, 0, 0) as usize != selected {
                SendMessageW(self.side, LB_SETCURSEL, selected, 0);
            }
        }
    }

    fn sort_indicator(&self) {
        unsafe {
            let header = SendMessageW(self.list, LVM_GETHEADER, 0, 0) as HWND;
            let selected = [0, 3, 2, 1][self.current().sort];
            for column in 0..4 {
                let mut item = HDITEMW {
                    mask: HDI_FORMAT,
                    ..zeroed()
                };
                SendMessageW(header, HDM_GETITEMW, column, &mut item as *mut _ as isize);
                item.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
                if column == selected {
                    item.fmt |= if self.current().descending {
                        HDF_SORTDOWN
                    } else {
                        HDF_SORTUP
                    };
                }
                SendMessageW(header, HDM_SETITEMW, column, &item as *const _ as isize);
            }
        }
    }

    fn render_list(&self) {
        let tab = self.current();
        DISPLAY.with(|display| {
            *display.borrow_mut() = Some(Display {
                rows: tab.rows.clone(),
                view: tab.view.clone(),
                show_extensions: self.show_extensions,
                item_checks: self.item_checks,
                tab_id: tab.id,
                rename_allowed: !self.busy && !tab.loading,
            })
        });
        unsafe {
            SendMessageW(self.list, LVM_SETVIEW, tab.view_mode as usize, 0);
            let view_command = match tab.view_mode {
                LV_VIEW_ICON => ID_VIEW_ICONS,
                LV_VIEW_SMALLICON => ID_VIEW_SMALL,
                LV_VIEW_LIST => ID_VIEW_LIST,
                _ => ID_VIEW_DETAILS,
            };
            CheckMenuRadioItem(
                GetMenu(self.hwnd),
                ID_VIEW_DETAILS as u32,
                ID_VIEW_ICONS as u32,
                view_command as u32,
                MF_BYCOMMAND,
            );
            let clear = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: 0,
                ..zeroed()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &clear as *const _ as isize,
            );
            SendMessageW(self.list, LVM_SETITEMCOUNT, tab.view.len(), 0);
            let selected: std::collections::HashSet<_> = tab.selected.iter().collect();
            let mut focused = None;
            for (i, &row) in tab.view.iter().enumerate() {
                if selected.contains(&tab.rows[row].entry.path) {
                    let item = LVITEMW {
                        stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                        state: LVIS_SELECTED | if focused.is_none() { LVIS_FOCUSED } else { 0 },
                        ..zeroed()
                    };
                    SendMessageW(self.list, LVM_SETITEMSTATE, i, &item as *const _ as isize);
                    if focused.is_none() {
                        focused = Some(i);
                    }
                }
            }
            if !tab.view.is_empty() {
                let top = tab.scroll.max(0).min(tab.view.len() as i32 - 1) as usize;
                SendMessageW(self.list, LVM_ENSUREVISIBLE, top, 0);
                if tab.view_mode == LV_VIEW_DETAILS {
                    let mut rect = RECT {
                        left: LVIR_BOUNDS as i32,
                        ..zeroed()
                    };
                    if SendMessageW(
                        self.list,
                        LVM_GETITEMRECT,
                        top,
                        &mut rect as *mut _ as isize,
                    ) != 0
                    {
                        let current = SendMessageW(self.list, LVM_GETTOPINDEX, 0, 0);
                        SendMessageW(
                            self.list,
                            LVM_SCROLL,
                            0,
                            (top as isize - current) * (rect.bottom - rect.top) as isize,
                        );
                    }
                }
                if let Some(index) = focused {
                    SendMessageW(self.list, LVM_ENSUREVISIBLE, index, 0);
                }
            }
            InvalidateRect(self.list, null(), 0);
            EnableWindow(self.buttons[0], (tab.history_at > 0) as i32);
            EnableWindow(
                self.buttons[1],
                (tab.history_at + 1 < tab.history.len()) as i32,
            );
            EnableWindow(self.buttons[2], tab.path.parent().is_some() as i32);
        }
        self.sort_indicator();
        self.update_status();
    }

    fn switch_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        self.cancel_rename();
        self.save_selection();
        self.active = index;
        self.update_tab_bar();
        self.show_current();
        unsafe {
            SetFocus(self.list);
        }
    }

    fn close_tab(&mut self) {
        self.close_tab_at(self.active);
    }

    fn close_tab_at(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
            return;
        }
        self.cancel_rename();
        self.save_selection();
        self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self.update_tab_bar();
        self.show_current();
    }

    fn navigate(&mut self, path: PathBuf) {
        self.cancel_rename();
        self.save_selection();
        let path = if path.is_absolute() {
            path
        } else {
            self.current().path.join(path)
        };
        self.current_mut().visit(path);
        self.load_location();
    }

    fn history(&mut self, forward: bool) {
        self.cancel_rename();
        self.save_selection();
        if self.current_mut().travel(forward) {
            self.load_location();
        }
    }

    fn history_menu(&mut self) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                error(self.hwnd, io::Error::last_os_error());
                return;
            }
            let tab = self.current();
            let end = (tab.history_at + 11).min(tab.history.len());
            let start = end.saturating_sub(20);
            for (offset, visit) in tab.history[start..end].iter().enumerate().rev() {
                let label = visit.path.to_string_lossy().replace('&', "&&");
                AppendMenuW(
                    menu,
                    MF_STRING
                        | if start + offset == tab.history_at {
                            MF_CHECKED
                        } else {
                            0
                        },
                    offset + 1,
                    wide(label).as_ptr(),
                );
            }
            let mut bounds: RECT = zeroed();
            GetWindowRect(self.history_button, &mut bounds);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                bounds.left,
                bounds.bottom,
                0,
                self.hwnd,
                null(),
            ) as usize;
            DestroyMenu(menu);
            if command != 0 {
                self.cancel_rename();
                self.save_selection();
                if self.current_mut().travel_to(start + command - 1) {
                    self.load_location();
                }
            }
        }
    }

    fn load_location(&mut self) {
        let tab = self.current_mut();
        tab.recursive = false;
        tab.cancel.store(true, Ordering::Relaxed);
        tab.watcher = None;
        tab.filter.clear();
        tab.rows = Rc::new(Vec::new());
        tab.view = Rc::new(Vec::new());
        tab.order.clear();
        self.arm_watcher(self.active);
        self.update_tab_bar();
        self.reload(self.active);
        self.show_current();
    }

    fn reload(&mut self, index: usize) {
        let tab = &mut self.tabs[index];
        tab.cancel.store(true, Ordering::Relaxed);
        tab.request += 1;
        tab.loading = true;
        tab.error = None;
        if !tab.inflight {
            self.start_listing(index);
        }
        self.update_status();
    }

    fn start_listing(&mut self, index: usize) {
        let tab = &mut self.tabs[index];
        tab.inflight = true;
        let id = tab.id;
        let request = tab.request;
        let path = tab.path.clone();
        let recursive = tab.recursive;
        let query = tab.filter.clone();
        tab.cancel = Arc::new(AtomicBool::new(false));
        let cancel = tab.cancel.clone();
        let hidden = self.show_hidden;
        let tx = self.tx.clone();
        let hwnd = self.hwnd as usize;
        std::thread::spawn(move || {
            let result = if recursive {
                directory_events::search(&path, &query, hidden, &cancel)
                    .map(|result| (result.entries, result.skipped))
            } else {
                fs::list_dir(&path, hidden).map(|entries| (entries, 0))
            };
            if tx
                .send(Completion::Listing {
                    id,
                    request,
                    result,
                })
                .is_ok()
            {
                unsafe {
                    PostMessageW(hwnd as HWND, READY, 0, 0);
                }
            }
        });
    }

    fn completions(&mut self) {
        while let Ok(completion) = self.rx.try_recv() {
            match completion {
                Completion::Listing {
                    id,
                    request,
                    result,
                } => {
                    let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                        continue;
                    };
                    self.tabs[index].inflight = false;
                    if self.tabs[index].request != request {
                        self.start_listing(index);
                        continue;
                    }
                    self.tabs[index].loading = false;
                    match result {
                        Ok((entries, skipped)) => {
                            self.tabs[index].skipped = skipped;
                            let rows = entries
                                .into_iter()
                                .map(|entry| {
                                    let extension = entry
                                        .path
                                        .extension()
                                        .map(|v| v.to_ascii_lowercase())
                                        .unwrap_or_default();
                                    let key = (
                                        entry.is_dir,
                                        if entry.is_dir {
                                            OsString::new()
                                        } else {
                                            extension.clone()
                                        },
                                    );
                                    let icon =
                                        *self.icons.entry(key.clone()).or_insert_with(|| {
                                            shell::icon_index(&entry.path, entry.is_dir)
                                        });
                                    let kind = self
                                        .kinds
                                        .entry(key)
                                        .or_insert_with(|| {
                                            Rc::new(shell::file_type(&entry.path, entry.is_dir))
                                        })
                                        .clone();
                                    Row {
                                        name: wide(&entry.name),
                                        search: entry.name.to_string_lossy().to_lowercase(),
                                        kind,
                                        entry,
                                        icon,
                                    }
                                })
                                .collect();
                            self.tabs[index].rows = Rc::new(rows);
                            self.tabs[index].sort_entries();
                        }
                        Err(err) => {
                            self.tabs[index].error = Some(err.to_string());
                        }
                    }
                    if index == self.active {
                        self.show_current();
                    }
                    if self.tabs[index].dirty {
                        self.refresh_dirty();
                    }
                }
                Completion::Operation {
                    result,
                    clipboard_sequence,
                    renamed,
                } => {
                    self.busy = false;
                    if result.is_ok()
                        && let Some((id, from, to)) = renamed
                        && let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
                    {
                        if index == self.active
                            && self.current().path == from.parent().unwrap_or(Path::new(""))
                        {
                            self.save_selection();
                        }
                        for path in &mut self.tabs[index].selected {
                            if path == &from {
                                *path = to.clone();
                            }
                        }
                        for visit in &mut self.tabs[index].history {
                            for path in &mut visit.selected {
                                if path == &from {
                                    *path = to.clone();
                                }
                            }
                        }
                    }
                    if result.is_ok()
                        && clipboard_sequence.is_some_and(|sequence| unsafe {
                            windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber()
                                == sequence
                        })
                    {
                        let _ = shell::clear_clipboard(self.hwnd);
                    }
                    for i in 0..self.tabs.len() {
                        self.reload(i);
                        self.tabs[i].dirty = false;
                    }
                    if let Err(err) = result
                        && err.kind() != io::ErrorKind::Interrupted
                    {
                        error(
                            self.hwnd,
                            tr_format!(
                                "操作を完了できませんでした。\n{err}\n\n途中まで完了している場合があります。フォルダーを確認してください。",
                                "The operation could not be completed.\n{err}\n\nSome items may have been processed. Check the folder."
                            ),
                        );
                    }
                }
                Completion::LabelEditFinished { target, value } => {
                    if let Some(name) = target.proposed_name(value) {
                        if self.busy {
                            error(
                                self.hwnd,
                                tr!(
                                    "別のファイル操作が実行中です。完了後に名前を変更してください。",
                                    "Another file operation is in progress. Wait for it to finish before renaming."
                                ),
                            );
                        } else {
                            self.operate(
                                Operation::Rename {
                                    tab_id: target.tab_id,
                                    path: target.path,
                                    name,
                                },
                                false,
                            );
                        }
                    }
                    self.refresh_dirty();
                }
                Completion::QuickAccess { generation, result } => {
                    if generation != self.pins_generation {
                        continue;
                    }
                    match result {
                        Ok(mut pins) => {
                            // Keep already resolved artwork while this generation
                            // rechecks the real folder icons in the background.
                            for pin in &mut pins {
                                if let Some(image) = self.shortcuts.iter().find_map(|place| {
                                    (place.path.as_deref() == Some(pin.path.as_path()))
                                        .then_some(place.image.get())
                                        .flatten()
                                }) {
                                    pin.image = image;
                                }
                            }
                            self.pins = pins;
                            self.pin_error = None;
                            self.refresh_places();
                            // On startup/F5, resolve the six standard sidebar
                            // icons before unrelated pins can block their worker.
                            if !self.pc_refresh_requested {
                                let paths = self.pins.iter().map(|pin| pin.path.clone()).collect();
                                self.request_place_icons(paths);
                            }
                        }
                        Err(error) => {
                            self.pin_error = Some(error.to_string());
                        }
                    }
                    if !self.tabs.is_empty() {
                        self.update_status();
                    }
                }
                Completion::PcPlaces {
                    generation,
                    mut places,
                } => {
                    if generation != self.pins_generation {
                        continue;
                    }
                    for place in &mut places {
                        if place.glyph != 0
                            && let Some(image) = self.shortcuts.iter().find_map(|old| {
                                (old.path == place.path)
                                    .then_some(old.image.get())
                                    .flatten()
                            })
                        {
                            place.image.set(Some(image));
                        }
                    }
                    let paths = places
                        .iter()
                        .filter(|place| place.glyph != 0)
                        .filter_map(|place| place.path.clone())
                        .chain(self.pins.iter().map(|pin| pin.path.clone()))
                        .collect();
                    self.pc_places = places;
                    self.pc_refresh_requested = false;
                    self.refresh_places();
                    self.request_place_icons(paths);
                }
                Completion::QuickAccessFinished => {
                    self.pins_pending = false;
                    if self.pins_refresh_requested {
                        self.pins_refresh_requested = false;
                        self.start_pins();
                    }
                }
                Completion::PlaceIcon {
                    generation,
                    path,
                    image,
                } => {
                    if generation != self.pins_generation
                        || !self
                            .shortcuts
                            .iter()
                            .any(|place| place.path.as_deref() == Some(path.as_path()))
                    {
                        continue;
                    }
                    self.apply_place_icon(&path, image);
                }
                Completion::PlaceIconsFinished { generation, result } => {
                    self.pin_icons_pending = false;
                    if generation == self.pins_generation
                        && let Err(error) = result
                    {
                        self.pin_error = Some(error.to_string());
                        if !self.tabs.is_empty() {
                            self.update_status();
                        }
                    }
                    if let Some(request) = self.pin_icons_requested.take()
                        && request.generation == self.pins_generation
                        && !request.cancel.load(Ordering::Relaxed)
                    {
                        self.start_place_icons(request);
                    }
                }
                Completion::Changed(result) => match result {
                    Ok(paths) => {
                        let mut dirty = false;
                        for path in paths {
                            let path = wide(path);
                            for tab in &mut self.tabs {
                                if tab
                                    .watcher
                                    .as_ref()
                                    .is_some_and(|watcher| watcher.affects(&path))
                                {
                                    tab.dirty = true;
                                    dirty = true;
                                }
                            }
                        }
                        if dirty {
                            unsafe {
                                SetTimer(self.hwnd, REFRESH_TIMER, 200, None);
                            }
                        }
                    }
                    Err(err) => {
                        self.current_mut().watch_error = Some(err.to_string());
                        self.update_status();
                    }
                },
            }
        }
    }

    fn update_status(&self) {
        let tab = self.current();
        DISPLAY.with(|display| {
            if let Some(display) = display.borrow_mut().as_mut() {
                display.rename_allowed = !self.busy && !tab.loading;
            }
        });
        let mut status = if self.busy {
            tr!(
                "ファイル操作中… 終了までお待ちください",
                "File operation in progress… Please wait"
            )
            .to_string()
        } else if tab.loading {
            tr!("読み込み中…", "Loading…").to_string()
        } else if let Some(err) = &tab.error {
            tr_format!(
                "読み込めません: {err}  — F5 で再試行",
                "Cannot load: {err}  — Press F5 to retry"
            )
        } else {
            let mut count = 0;
            let mut bytes = 0u64;
            let mut index = -1isize;
            unsafe {
                loop {
                    index = SendMessageW(
                        self.list,
                        LVM_GETNEXTITEM,
                        index as usize,
                        LVNI_SELECTED as isize,
                    );
                    if index < 0 {
                        break;
                    }
                    if let Some(&row) = tab.view.get(index as usize) {
                        count += 1;
                        if !tab.rows[row].entry.is_dir {
                            bytes = bytes.saturating_add(tab.rows[row].entry.size);
                        }
                    }
                }
            }
            let total = if tab.filter.is_empty() {
                tr_format!("{} 個の項目", "{} items", tab.view.len())
            } else {
                tr_format!(
                    "{} / {} 個の項目",
                    "{} / {} items",
                    tab.view.len(),
                    tab.rows.len()
                )
            };
            if count == 0 {
                total
            } else {
                tr_format!(
                    "{total}    |    {count} 個を選択    {}",
                    "{total}    |    {count} selected    {}",
                    size_text(bytes)
                )
            }
        };
        if tab.skipped > 0 {
            status.push_str(&tr_format!(
                "    |    検索できなかった場所が {} 件あります",
                "    |    {} locations could not be searched",
                tab.skipped
            ));
        }
        if let Some(warning) = &tab.watch_error {
            status.push_str(&format!("    |    {warning}"));
        }
        if let Some(warning) = &self.pin_error {
            status.push_str(&tr_format!(
                "    |    クイックアクセス: {warning}",
                "    |    Quick access: {warning}"
            ));
        }
        set_text(self.status, status);
        if let Some(panel) = self.preview
            && self.preview_visible
        {
            let paths = self.selected_paths();
            panel.set_path(if paths.len() == 1 {
                Some(&paths[0])
            } else {
                None
            });
        }
        unsafe {
            let selected = SendMessageW(self.list, LVM_GETSELECTEDCOUNT, 0, 0);
            for index in [5, 6, 9] {
                EnableWindow(self.buttons[index], (selected > 0 && !self.busy) as i32);
            }
            EnableWindow(self.buttons[8], (selected == 1 && !self.busy) as i32);
            for index in [4, 7] {
                EnableWindow(self.buttons[index], (!self.busy) as i32);
            }
        }
    }

    fn filter(&mut self) {
        let query = text(self.search);
        if query == self.current().filter {
            return;
        }
        self.cancel_rename();
        self.save_selection();
        let tab = self.current_mut();
        tab.filter = query;
        let was_recursive = tab.recursive;
        tab.recursive = false;
        tab.cancel.store(true, Ordering::Relaxed);
        tab.scroll = 0;
        tab.rebuild();
        // Preserve selection by path, never by the old (now filtered) row index.
        self.render_list();
        if was_recursive {
            self.reload(self.active);
        }
    }

    fn operate(&mut self, operation: Operation, clear_clipboard: bool) {
        if self.busy {
            return;
        }
        self.save_selection();
        self.busy = true;
        self.update_status();
        let clipboard_sequence = clear_clipboard.then(|| unsafe {
            windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber()
        });
        let tx = self.tx.clone();
        let hwnd = self.hwnd as usize;
        std::thread::spawn(move || {
            let mut renamed = None;
            let result = match operation {
                Operation::Copy(paths, destination) => {
                    actions::transfer(hwnd as HWND, &paths, &destination, false)
                }
                Operation::Move(paths, destination) => {
                    actions::transfer(hwnd as HWND, &paths, &destination, true)
                }
                Operation::Rename { tab_id, path, name } => {
                    let result = fs::rename(&path, &name);
                    if result.is_ok() {
                        let to = path.with_file_name(&name);
                        renamed = Some((tab_id, path, to));
                    }
                    result
                }
                Operation::Folder(path, name) => fs::create_folder(&path, &name),
                Operation::Delete(paths) => fs::delete_permanently(&paths),
            };
            if tx
                .send(Completion::Operation {
                    result,
                    clipboard_sequence,
                    renamed,
                })
                .is_ok()
            {
                unsafe {
                    PostMessageW(hwnd as HWND, READY, 0, 0);
                }
            }
        });
    }

    fn open_selected(&mut self, new_tab: bool) {
        let paths = self.selected_paths();
        if paths.len() > 1
            && !confirm(
                self.hwnd,
                &tr_format!(
                    "選択した {} 項目を開きますか？",
                    "Open {} selected items?",
                    paths.len()
                ),
            )
        {
            return;
        }
        for path in paths {
            if path.is_dir() {
                if new_tab {
                    self.add_tab(path);
                } else {
                    self.navigate(path);
                }
            } else if let Err(err) = shell::open(&path) {
                error(self.hwnd, err);
            }
        }
    }

    fn command(&mut self, id: usize) {
        if self.tabs.is_empty() {
            return;
        }
        match id {
            ID_SETTINGS => {
                self.cancel_rename();
                if let Err(err) = preferences::show(self.hwnd, self.font, &self.current().path) {
                    error(self.hwnd, err);
                } else {
                    match settings::load() {
                        Ok(saved) if saved.appearance.icon_theme != self.icon_theme => {
                            if let Err(err) = self.apply_icon_theme(saved.appearance.icon_theme) {
                                error(self.hwnd, err);
                            }
                        }
                        Ok(_) => {}
                        Err(err) => error(self.hwnd, err),
                    }
                }
            }
            ID_OPEN_TERMINAL => {
                self.cancel_rename();
                self.open_selected_terminal();
            }
            ID_HISTORY => self.history_menu(),
            ID_BACK => self.history(false),
            ID_FORWARD => self.history(true),
            ID_UP => {
                if let Some(parent) = self.current().path.parent().map(Path::to_path_buf) {
                    self.navigate(parent);
                }
            }
            ID_REFRESH => {
                self.save_selection();
                self.reload(self.active);
                self.reload_pins(true);
                if let Some(panel) = self.preview {
                    panel.refresh();
                }
            }
            ID_NEW_TAB => self.add_tab(self.current().path.clone()),
            ID_NEW_WINDOW => self.new_window(&self.current().path),
            ID_CLOSE_TAB => self.close_tab(),
            ID_OPEN => self.open_selected(false),
            ID_OPEN_TAB => self.open_selected(true),
            ID_COPY | ID_CUT => {
                let paths = self.selected_paths();
                if !paths.is_empty()
                    && let Err(err) = shell::set_clipboard(self.hwnd, &paths, id == ID_CUT)
                {
                    error(self.hwnd, err);
                }
            }
            ID_PASTE if !self.busy => match shell::get_clipboard(self.hwnd) {
                Ok((paths, cut)) if !paths.is_empty() => {
                    let destination = self.current().path.clone();
                    self.operate(
                        if cut {
                            Operation::Move(paths, destination)
                        } else {
                            Operation::Copy(paths, destination)
                        },
                        cut,
                    );
                }
                Ok(_) => {}
                Err(err) => error(self.hwnd, err),
            },
            ID_RENAME => self.begin_rename(),
            ID_FOLDER if !self.busy => {
                if let Some(name) = prompt(
                    self.hwnd,
                    tr!("新しいフォルダー", "New folder"),
                    tr!("フォルダー名", "Folder name"),
                    tr!("新しいフォルダー", "New folder"),
                    self.font,
                ) {
                    self.operate(Operation::Folder(self.current().path.clone(), name), false);
                }
            }
            ID_DELETE | ID_PERMANENT_DELETE if !self.busy => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return;
                }
                if id == ID_PERMANENT_DELETE {
                    if confirm(
                        self.hwnd,
                        &tr_format!(
                            "選択した {} 項目を完全に削除しますか？\nこの操作は元に戻せません。",
                            "Permanently delete {} selected items?\nThis cannot be undone.",
                            paths.len()
                        ),
                    ) {
                        self.operate(Operation::Delete(paths), false);
                    }
                } else {
                    // SHFileOperation displays its own native confirmation and progress.
                    self.busy = true;
                    self.update_status();
                    let result = shell::recycle(self.hwnd, &paths);
                    self.busy = false;
                    self.reload(self.active);
                    if let Err(err) = result
                        && err.kind() != io::ErrorKind::Interrupted
                    {
                        error(self.hwnd, err);
                    }
                }
            }
            ID_PROPERTIES => {
                let paths = self.selected_paths();
                let path = paths.first().unwrap_or(&self.current().path);
                if let Err(err) = shell::properties(self.hwnd, path) {
                    error(self.hwnd, err);
                }
            }
            ID_SELECT_ALL => unsafe {
                let item = LVITEMW {
                    stateMask: LVIS_SELECTED,
                    state: LVIS_SELECTED,
                    ..zeroed()
                };
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    usize::MAX,
                    &item as *const _ as isize,
                );
                self.update_status();
            },
            ID_HIDDEN => {
                self.show_hidden = !self.show_hidden;
                for i in 0..self.tabs.len() {
                    self.reload(i);
                }
            }
            ID_SORT => self.toolbar_menu(),
            ID_MORE => unsafe {
                let mut point: POINT = zeroed();
                GetCursorPos(&mut point);
                self.context_menu(point.x, point.y);
            },
            ID_SHARE => unsafe {
                let menu = GetMenu(self.hwnd);
                let position = (0..GetMenuItemCount(menu))
                    .find(|&position| GetMenuItemID(menu, position) == ID_SHARE as u32);
                let mut rect: RECT = zeroed();
                if let Some(position) = position
                    && GetMenuItemRect(self.hwnd, menu, position as u32, &mut rect) != 0
                {
                    self.context_menu(rect.left, rect.bottom);
                } else {
                    error(
                        self.hwnd,
                        tr!(
                            "共有メニューの位置を取得できませんでした。",
                            "Could not locate the Share menu."
                        ),
                    );
                }
            },
            ID_PIN => {
                let mut paths: Vec<_> = self
                    .selected_paths()
                    .into_iter()
                    .filter(|path| path.is_dir())
                    .collect();
                if paths.is_empty() {
                    paths.push(self.current().path.clone());
                }
                self.pin_paths(paths);
            }
            ID_PICK_PIN => match quick_access::pick_folders(self.hwnd) {
                Ok(paths) => self.pin_paths(paths),
                Err(err) => error(self.hwnd, err),
            },
            ID_UNPIN => {
                if let Err(err) = quick_access::unpin(self.hwnd, &self.current().path) {
                    error(self.hwnd, err);
                }
                self.pins_changed();
            }
            ID_TOOLBAR => {
                self.show_toolbar = !self.show_toolbar;
                self.check_menu(ID_TOOLBAR, self.show_toolbar);
                self.layout();
            }
            ID_EXTENSIONS => {
                self.save_selection();
                self.show_extensions = !self.show_extensions;
                self.check_menu(ID_EXTENSIONS, self.show_extensions);
                self.render_list();
            }
            ID_CHECKS => {
                self.save_selection();
                self.item_checks = !self.item_checks;
                self.check_menu(ID_CHECKS, self.item_checks);
                unsafe {
                    SendMessageW(
                        self.list,
                        LVM_SETEXTENDEDLISTVIEWSTYLE,
                        LVS_EX_CHECKBOXES as usize,
                        if self.item_checks {
                            LVS_EX_CHECKBOXES as isize
                        } else {
                            0
                        },
                    );
                }
                self.render_list();
            }
            ID_VIEW_DETAILS | ID_VIEW_LIST | ID_VIEW_SMALL | ID_VIEW_ICONS => {
                self.save_selection();
                let view = match id {
                    ID_VIEW_ICONS => LV_VIEW_ICON,
                    ID_VIEW_SMALL => LV_VIEW_SMALLICON,
                    ID_VIEW_LIST => LV_VIEW_LIST,
                    _ => LV_VIEW_DETAILS,
                };
                if view == LV_VIEW_ICON {
                    unsafe {
                        SendMessageW(
                            self.list,
                            LVM_SETIMAGELIST,
                            LVSIL_NORMAL as usize,
                            shell::system_large_image_list() as isize,
                        );
                    }
                }
                self.current_mut().view_mode = view;
                self.render_list();
            }
            ID_PREVIEW => {
                if self.preview.is_none() {
                    match preview::Panel::new(self.hwnd, self.style()) {
                        Ok(panel) => self.preview = Some(panel),
                        Err(err) => {
                            error(self.hwnd, err);
                            return;
                        }
                    }
                }
                self.preview_visible = !self.preview_visible;
                self.check_menu(ID_PREVIEW, self.preview_visible);
                if let Some(panel) = self.preview {
                    panel.show(self.preview_visible);
                    if !self.preview_visible {
                        panel.set_path(None);
                    }
                }
                self.layout();
                self.update_status();
            }
            ID_SORT_NAME | ID_SORT_DATE | ID_SORT_TYPE | ID_SORT_SIZE | ID_SORT_REVERSE => {
                self.save_selection();
                let tab = self.current_mut();
                if id == ID_SORT_REVERSE {
                    tab.descending = !tab.descending;
                } else {
                    tab.sort = match id {
                        ID_SORT_SIZE => 1,
                        ID_SORT_TYPE => 2,
                        ID_SORT_DATE => 3,
                        _ => 0,
                    };
                }
                tab.sort_entries();
                self.render_list();
            }
            ID_COPY_PATH => {
                let paths = self.selected_paths();
                let value = if paths.is_empty() {
                    self.current().path.display().to_string()
                } else {
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\r\n")
                };
                if let Err(err) = copy_text(self.hwnd, &value) {
                    error(self.hwnd, err);
                }
            }
            ID_HELP => unsafe {
                MessageBoxW(
                    self.hwnd,
                    tr_w!(
                        "rfm — 軽量 Windows ファイラー\n\nCtrl+T / Ctrl+W   タブを追加 / 閉じる\nCtrl+N   新しいウィンドウ\nCtrl+Tab / Ctrl+Shift+Tab   タブを切り替え\nCtrl+L / Alt+D   アドレスバー\nCtrl+F   名前で絞り込み（Enterでサブフォルダー検索）\nAlt+← / Alt+→   戻る / 進む\nAlt+↑ / Backspace   親フォルダー\nEnter / ダブルクリック   開く\nCtrl+Enter   選択フォルダーを別タブで開く\nCtrl+C / X / V   コピー / 切り取り / 貼り付け\nCtrl+Shift+C   パスをコピー\nCtrl+Shift+N   新しいフォルダー\nF2   名前の変更\nDelete   ごみ箱へ移動\nShift+Delete   完全に削除（確認あり）\nAlt+Enter   プロパティ\nAlt+P   プレビューの表示切り替え\nCtrl+H   隠しファイルの表示切り替え\nF5   更新\n\nファイル / ホーム / 共有 / 表示メニューから操作できます。\n列の見出しで並べ替え。右クリックはWindows標準メニュー。\nクイックアクセスの追加・解除はファイルメニューと左ペインの右クリック。\nコピー・移動の競合、進捗、キャンセルはWindows標準画面。\n通常の検索入力は現在のフォルダー内。Enterで名前の再帰検索。\nAQS・ファイル内容検索・シェル仮想フォルダーには未対応です。",
                        "rfm — Lightweight Windows file manager\n\nCtrl+T / Ctrl+W   Add / close tab\nCtrl+N   New window\nCtrl+Tab / Ctrl+Shift+Tab   Switch tabs\nCtrl+L / Alt+D   Address bar\nCtrl+F   Filter by name (Enter searches subfolders)\nAlt+← / Alt+→   Back / forward\nAlt+↑ / Backspace   Parent folder\nEnter / double-click   Open\nCtrl+Enter   Open selected folders in new tabs\nCtrl+C / X / V   Copy / cut / paste\nCtrl+Shift+C   Copy path\nCtrl+Shift+N   New folder\nF2   Rename\nDelete   Move to Recycle Bin\nShift+Delete   Delete permanently (with confirmation)\nAlt+Enter   Properties\nAlt+P   Toggle preview pane\nCtrl+H   Toggle hidden files\nF5   Refresh\n\nCommands are also available in the File / Home / Share / View menus.\nClick column headings to sort. Right-click opens the Windows context menu.\nPin or unpin Quick access folders from File or the sidebar context menu.\nCopy/move conflicts, progress, and cancellation use Windows dialogs.\nTyping in search filters the current folder. Enter searches names recursively.\nAQS, file-content search, and shell virtual folders are not supported."
                    ),
                    tr_w!("rfm — 操作方法", "rfm — Help"),
                    MB_OK | MB_ICONINFORMATION,
                );
            },
            ID_EXIT => unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            },
            _ => {}
        }
    }

    fn address_enter(&mut self) {
        let input = text(self.address);
        let input = input.trim().trim_matches('"');
        if input.is_empty() {
            return;
        }
        let path = if input == "~" {
            home()
        } else if let Some(rest) = input
            .strip_prefix("~\\")
            .or_else(|| input.strip_prefix("~/"))
        {
            home().join(rest)
        } else {
            PathBuf::from(input)
        };
        self.navigate(path);
        breadcrumb::end_edit(self.address_bar);
        unsafe {
            SetFocus(self.list);
        }
    }

    fn keyboard(&mut self, msg: &MSG) -> bool {
        if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
            return false;
        }
        let key = msg.wParam as u16;
        let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) < 0 };
        let shift = unsafe { GetKeyState(VK_SHIFT as i32) < 0 };
        let alt = unsafe { GetKeyState(VK_MENU as i32) < 0 };
        let focus = unsafe { GetFocus() };
        let label_edit = unsafe { SendMessageW(self.list, LVM_GETEDITCONTROL, 0, 0) as HWND };
        if !label_edit.is_null() && focus == label_edit {
            return false;
        }
        let editing = focus == self.address || focus == self.search;
        if ctrl && (key == b'L' as u16 || key == b'F' as u16) || alt && key == b'D' as u16 {
            if key == b'F' as u16 {
                unsafe {
                    SetFocus(self.search);
                    SendMessageW(self.search, EM_SETSEL, 0, -1);
                }
            } else {
                breadcrumb::begin_edit(self.address_bar);
            }
            return true;
        }
        if ctrl && key == VK_TAB {
            let count = self.tabs.len();
            self.switch_tab((self.active + if shift { count - 1 } else { 1 }) % count);
            return true;
        }
        if ctrl && (b'1' as u16..=b'9' as u16).contains(&key) {
            let index = if key == b'9' as u16 {
                self.tabs.len() - 1
            } else {
                (key - b'1' as u16) as usize
            };
            self.switch_tab(index);
            return true;
        }
        if key == VK_RETURN && focus == self.address {
            self.address_enter();
            return true;
        }
        if key == VK_RETURN && focus == self.search {
            let query = text(self.search);
            if !query.trim().is_empty() {
                self.save_selection();
                let tab = self.current_mut();
                tab.filter = query;
                tab.recursive = true;
                tab.skipped = 0;
                self.reload(self.active);
            }
            unsafe {
                SetFocus(self.list);
            }
            return true;
        }
        if key == VK_ESCAPE
            && focus == self.list
            && (self.current().recursive || !self.current().filter.is_empty())
        {
            set_text(self.search, "");
            self.filter();
            return true;
        }
        if key == VK_ESCAPE && editing {
            if focus == self.search {
                set_text(self.search, "");
                self.filter();
            } else {
                breadcrumb::end_edit(self.address_bar);
            }
            unsafe {
                SetFocus(self.list);
            }
            return true;
        }
        let command = if ctrl && !alt {
            match key as u8 {
                b'T' => Some(ID_NEW_TAB),
                b'W' => Some(ID_CLOSE_TAB),
                b'H' if !editing => Some(ID_HIDDEN),
                b'N' if shift => Some(ID_FOLDER),
                b'N' => Some(ID_NEW_WINDOW),
                b'C' if !editing => Some(if shift { ID_COPY_PATH } else { ID_COPY }),
                b'X' if !editing => Some(ID_CUT),
                b'V' if !editing => Some(ID_PASTE),
                b'A' if !editing => Some(ID_SELECT_ALL),
                _ if key == VK_RETURN && !editing => Some(ID_OPEN_TAB),
                _ => None,
            }
        } else if alt {
            match key {
                key if key == b'P' as u16 => Some(ID_PREVIEW),
                VK_LEFT => Some(ID_BACK),
                VK_RIGHT => Some(ID_FORWARD),
                VK_UP => Some(ID_UP),
                VK_RETURN => Some(ID_PROPERTIES),
                _ => None,
            }
        } else {
            match key {
                VK_F1 => Some(ID_HELP),
                VK_F5 => Some(ID_REFRESH),
                VK_F2 if !editing => Some(ID_RENAME),
                VK_DELETE if !editing && focus == self.list => Some(if shift {
                    ID_PERMANENT_DELETE
                } else {
                    ID_DELETE
                }),
                VK_BACK if !editing && focus == self.list => Some(ID_UP),
                VK_RETURN if focus == self.list => Some(ID_OPEN),
                _ => None,
            }
        };
        if let Some(command) = command {
            self.command(command);
            true
        } else {
            false
        }
    }

    fn context_menu(&mut self, x: i32, y: i32) {
        let selected = self.selected_paths();
        match actions::context_menu(self.hwnd, &self.current().path, &selected, POINT { x, y }) {
            Ok(actions::MenuAction::Canceled) => return,
            Ok(actions::MenuAction::Open) => {
                self.open_selected(false);
                return;
            }
            Ok(actions::MenuAction::OpenWindow) => {
                self.new_window(selected.first().unwrap_or(&self.current().path));
                return;
            }
            Ok(actions::MenuAction::Rename) => {
                self.command(ID_RENAME);
                return;
            }
            Ok(actions::MenuAction::OpenInTab) => {
                if selected.is_empty() {
                    self.add_tab(self.current().path.clone());
                } else {
                    self.open_selected(true);
                }
                return;
            }
            Ok(actions::MenuAction::OpenTerminal) => {
                self.open_terminal(selected.first().unwrap_or(&self.current().path));
                return;
            }
            Ok(actions::MenuAction::PinToQuickAccess) => {
                self.command(ID_PIN);
                return;
            }
            Ok(actions::MenuAction::Invoked) => {}
            Err(err) => {
                error(self.hwnd, err);
                return;
            }
        }
        self.pins_changed();
        if self.current().watcher.is_none() {
            self.save_selection();
            self.reload(self.active);
        }
    }

    fn dropped(&mut self, drop: HDROP) {
        unsafe {
            let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
            let mut paths = Vec::new();
            for i in 0..count {
                let len = DragQueryFileW(drop, i, null_mut(), 0);
                let mut buf = vec![0; len as usize + 1];
                DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
                paths.push(PathBuf::from(OsString::from_wide(&buf[..len as usize])));
            }
            DragFinish(drop);
            if self.busy || paths.is_empty() {
                return;
            }
            if confirm(
                self.hwnd,
                &tr_format!(
                    "{} 項目を次のフォルダーにコピーしますか？\n{}",
                    "Copy {} items to this folder?\n{}",
                    paths.len(),
                    self.current().path.display()
                ),
            ) {
                self.operate(Operation::Copy(paths, self.current().path.clone()), false);
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.pins_cancel.store(true, Ordering::Relaxed);
        unsafe {
            icons::shutdown();
            DeleteObject(self.font);
            DeleteObject(self.icon_font);
            if !self.window_icon.is_null() {
                DestroyIcon(self.window_icon);
            }
        }
    }
}

fn copy_text(owner: HWND, value: &str) -> io::Result<()> {
    use windows_sys::Win32::System::{DataExchange::*, Memory::*};
    unsafe {
        if OpenClipboard(owner) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let value = wide(value);
            let memory = GlobalAlloc(GMEM_MOVEABLE, value.len() * 2);
            if memory.is_null() {
                return Err(io::Error::last_os_error());
            }
            let ptr = GlobalLock(memory);
            if ptr.is_null() {
                GlobalFree(memory);
                return Err(io::Error::last_os_error());
            }
            std::ptr::copy_nonoverlapping(value.as_ptr(), ptr.cast(), value.len());
            GlobalUnlock(memory);
            if EmptyClipboard() == 0 || SetClipboardData(13, memory).is_null() {
                GlobalFree(memory);
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

unsafe extern "system" fn list_subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        if message == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(list_subclass), id);
        } else if DISPLAY.with(|display| display.borrow().as_ref().is_some_and(|d| d.item_checks)) {
            let item = if matches!(message, WM_LBUTTONDOWN | WM_LBUTTONDBLCLK) {
                let mut hit = LVHITTESTINFO {
                    pt: POINT {
                        x: l as u16 as i16 as i32,
                        y: (l >> 16) as u16 as i16 as i32,
                    },
                    ..zeroed()
                };
                SendMessageW(hwnd, LVM_HITTEST, 0, &mut hit as *mut _ as isize);
                if hit.flags & LVHT_ONITEMSTATEICON != 0 {
                    hit.iItem
                } else {
                    -1
                }
            } else if message == WM_KEYDOWN && w == VK_SPACE as usize {
                SendMessageW(hwnd, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize) as i32
            } else {
                -1
            };
            if item >= 0 {
                let selected = SendMessageW(
                    hwnd,
                    LVM_GETITEMSTATE,
                    item as usize,
                    LVIS_SELECTED as isize,
                ) != 0;
                let state = LVITEMW {
                    stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                    state: LVIS_FOCUSED | if selected { 0 } else { LVIS_SELECTED },
                    ..zeroed()
                };
                SetFocus(hwnd);
                SendMessageW(
                    hwnd,
                    LVM_SETITEMSTATE,
                    item as usize,
                    &state as *const _ as isize,
                );
                InvalidateRect(hwnd, null(), 0);
                return 0;
            }
        }
        DefSubclassProc(hwnd, message, w, l)
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_CLOSE && APP.with(|app| app.try_borrow().is_err()) {
            return 0;
        }
        if message == directory_events::MESSAGE {
            let result = directory_events::changed_paths(w, l);
            EVENTS.with(|events| {
                if let Some(sender) = events.borrow().as_ref() {
                    let _ = sender.send(Completion::Changed(result));
                }
            });
            PostMessageW(hwnd, READY, 0, 0);
            return 0;
        }
        if message == WM_ERASEBKGND {
            return 1;
        }
        if message == WM_MEASUREITEM {
            let item = &mut *(l as *mut MEASUREITEMSTRUCT);
            if item.CtlID == ID_SIDE as u32 {
                item.itemHeight = 22 * GetDpiForWindow(hwnd).max(96) / 96;
                return 1;
            }
        }
        if message == WM_DRAWITEM {
            let item = &*(l as *const DRAWITEMSTRUCT);
            if chrome::draw_button(item) {
                return 1;
            }
            if item.CtlID == ID_SIDE as u32 {
                SIDEBAR.with(|display| {
                    let display = display.borrow();
                    let Some(display) = display.as_ref() else {
                        return;
                    };
                    let Some(place) = display.places.get(item.itemID as usize) else {
                        return;
                    };
                    let style = display.style;
                    chrome::draw_sidebar(
                        item,
                        &place.label,
                        if display.icon_theme == settings::IconTheme::Windows {
                            0
                        } else {
                            place.glyph
                        },
                        place.path.is_none(),
                        style,
                    );
                    if let Some(image) = place.image.get()
                        && (display.icon_theme == settings::IconTheme::Windows || place.glyph == 0)
                    {
                        let icon = RECT {
                            left: item.rcItem.left + 12 * style.dpi as i32 / 96,
                            right: item.rcItem.left + 30 * style.dpi as i32 / 96,
                            ..item.rcItem
                        };
                        chrome::draw_shell_icon(item.hDC, icon, image, style);
                    }
                });
                return 1;
            }
        }
        if message == WM_NOTIFY {
            let header = &*(l as *const NMHDR);
            if header.idFrom == ID_LIST && header.code == LVN_BEGINLABELEDITW {
                let info = &*(l as *const NMLVDISPINFOW);
                let target = DISPLAY.with(|display| {
                    let display = display.borrow();
                    let display = display.as_ref().filter(|display| display.rename_allowed)?;
                    let row = &display.rows[*display.view.get(info.item.iItem as usize)?];
                    let select = if row.entry.is_dir {
                        row.entry.name.as_os_str()
                    } else {
                        Path::new(&row.entry.name)
                            .file_stem()
                            .unwrap_or(&row.entry.name)
                    };
                    let edit = SendMessageW(header.hwndFrom, LVM_GETEDITCONTROL, 0, 0) as HWND;
                    if !edit.is_null() {
                        PostMessageW(edit, EM_SETSEL, 0, select.encode_wide().count() as isize);
                    }
                    Some(RenameTarget {
                        tab_id: display.tab_id,
                        path: row.entry.path.clone(),
                        suffix: if !display.show_extensions && !row.entry.is_dir {
                            row.entry
                                .path
                                .extension()
                                .map(|extension| extension.to_string_lossy().into_owned())
                        } else {
                            None
                        },
                    })
                });
                let allowed = target.is_some();
                LABEL_EDIT.with(|edit| *edit.borrow_mut() = target);
                return if allowed { 0 } else { 1 };
            }
            if header.idFrom == ID_LIST && header.code == LVN_ENDLABELEDITW {
                let info = &*(l as *const NMLVDISPINFOW);
                let target = LABEL_EDIT.with(|edit| edit.borrow_mut().take());
                if let Some(target) = target {
                    let value = if info.item.pszText.is_null() {
                        None
                    } else {
                        let mut length = 0;
                        while *info.item.pszText.add(length) != 0 {
                            length += 1;
                        }
                        Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                            info.item.pszText,
                            length,
                        )))
                    };
                    EVENTS.with(|events| {
                        if let Some(sender) = events.borrow().as_ref() {
                            let _ = sender.send(Completion::LabelEditFinished { target, value });
                        }
                    });
                    PostMessageW(hwnd, READY, 0, 0);
                }
                // Owner-data labels are published only after the filesystem rename succeeds.
                return 0;
            }
            if header.code == LVN_GETDISPINFOW {
                let info = &mut *(l as *mut NMLVDISPINFOW);
                DISPLAY.with(|display| {
                    let display = display.borrow();
                    let Some(display) = display.as_ref() else {
                        return;
                    };
                    let Some(&row) = display.view.get(info.item.iItem as usize) else {
                        return;
                    };
                    let row = &display.rows[row];
                    if info.item.mask & LVIF_TEXT != 0 {
                        let mut cell = CellText::new();
                        let value = match info.item.iSubItem {
                            1 => {
                                cell.date(row.entry.modified);
                                cell.slice()
                            }
                            2 => row.kind.as_slice(),
                            3 => {
                                if !row.entry.is_dir {
                                    cell.number(row.entry.size.div_ceil(1024), 1, true);
                                    cell.text(" KB");
                                }
                                cell.slice()
                            }
                            _ if !display.show_extensions && !row.entry.is_dir => {
                                let name = Path::new(&row.entry.name)
                                    .file_stem()
                                    .unwrap_or(&row.entry.name);
                                for unit in name.encode_wide() {
                                    cell.push(unit);
                                }
                                cell.slice()
                            }
                            _ => &row.name,
                        };
                        // Copy into the control's buffer; never expose a pointer whose
                        // lifetime might end when switching tabs.
                        if !info.item.pszText.is_null() && info.item.cchTextMax > 0 {
                            let len = (value.len() - 1).min(info.item.cchTextMax as usize - 1);
                            std::ptr::copy_nonoverlapping(value.as_ptr(), info.item.pszText, len);
                            *info.item.pszText.add(len) = 0;
                        }
                    }
                    if info.item.mask & LVIF_IMAGE != 0 {
                        info.item.iImage = row.icon;
                    }
                    if info.item.mask & LVIF_STATE != 0 {
                        if info.item.stateMask & LVIS_CUT != 0 {
                            info.item.state = (info.item.state & !LVIS_CUT)
                                | if row.entry.hidden { LVIS_CUT } else { 0 };
                        }
                        if info.item.stateMask & LVIS_STATEIMAGEMASK != 0 {
                            let selected = SendMessageW(
                                header.hwndFrom,
                                LVM_GETITEMSTATE,
                                info.item.iItem as usize,
                                LVIS_SELECTED as isize,
                            ) != 0;
                            info.item.state = (info.item.state & !LVIS_STATEIMAGEMASK)
                                | if display.item_checks {
                                    if selected { 2 << 12 } else { 1 << 12 }
                                } else {
                                    0
                                };
                        }
                    }
                });
                return 0;
            }
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        if message == WM_GETMINMAXINFO {
            let info = &mut *(l as *mut MINMAXINFO);
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            info.ptMinTrackSize = POINT {
                x: 520 * dpi / 96,
                y: 420 * dpi / 96,
            };
            return 0;
        }
        if message == READY {
            return 0;
        }
        let handled = APP.with(|cell| {
            let Ok(mut state) = cell.try_borrow_mut() else {
                return None;
            };
            let app = state.as_mut()?;
            match message {
                WM_TIMER if w == REFRESH_TIMER => {
                    KillTimer(hwnd, REFRESH_TIMER);
                    app.refresh_dirty();
                    Some(0)
                }
                WM_TIMER if w == PIN_REFRESH_TIMER => {
                    KillTimer(hwnd, PIN_REFRESH_TIMER);
                    app.reload_pins(false);
                    Some(0)
                }
                WM_LBUTTONDOWN => {
                    let x = l as u16 as i16 as i32;
                    let y = (l >> 16) as u16 as i16 as i32;
                    if (x - app.scale(app.sidebar_width)).abs() <= app.scale(4)
                        && y >= app.scale(app.header_height())
                    {
                        app.resizing_side = true;
                        SetCapture(hwnd);
                        Some(0)
                    } else {
                        None
                    }
                }
                WM_MOUSEMOVE if app.resizing_side => {
                    let mut rect: RECT = zeroed();
                    GetClientRect(hwnd, &mut rect);
                    let x = l as u16 as i16 as i32;
                    app.sidebar_width = (x * 96 / app.dpi as i32)
                        .clamp(100, (rect.right * 48 / app.dpi as i32).max(100));
                    app.layout();
                    Some(0)
                }
                WM_LBUTTONUP | WM_CAPTURECHANGED if app.resizing_side => {
                    app.resizing_side = false;
                    if message == WM_LBUTTONUP {
                        ReleaseCapture();
                    }
                    Some(0)
                }
                WM_SETCURSOR => {
                    let mut point: POINT = zeroed();
                    GetCursorPos(&mut point);
                    ScreenToClient(hwnd, &mut point);
                    if (point.x - app.scale(app.sidebar_width)).abs() <= app.scale(4)
                        && point.y >= app.scale(app.header_height())
                    {
                        SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
                        Some(1)
                    } else {
                        None
                    }
                }
                WM_PAINT => {
                    let mut ps: PAINTSTRUCT = zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    app.paint(dc);
                    EndPaint(hwnd, &ps);
                    Some(0)
                }
                WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
                    let dc = w as HDC;
                    let color = if l as HWND == app.side {
                        0x00f6_f6f6
                    } else if l as HWND == app.status {
                        0x00fa_fafa
                    } else {
                        0x00ff_ffff
                    };
                    SetTextColor(
                        dc,
                        if l as HWND == app.status {
                            0x0060_6060
                        } else {
                            0x0020_2020
                        },
                    );
                    SetBkColor(dc, color);
                    SetDCBrushColor(dc, color);
                    Some(GetStockObject(DC_BRUSH) as isize)
                }
                chrome::TAB_SELECT => {
                    app.switch_tab(w);
                    Some(0)
                }
                chrome::TAB_CLOSE => {
                    app.close_tab_at(w);
                    Some(0)
                }
                chrome::TAB_NEW => {
                    app.command(ID_NEW_TAB);
                    Some(0)
                }
                breadcrumb::NAVIGATE => {
                    if w as HWND == app.address_bar
                        && let Some(path) = breadcrumb::take_navigation(app.address_bar)
                    {
                        app.navigate(path);
                    }
                    Some(0)
                }
                WM_THEMECHANGED => {
                    if let Err(err) = app.apply_icon_theme(app.icon_theme) {
                        error(hwnd, err);
                    }
                    Some(0)
                }
                WM_SIZE => {
                    app.layout();
                    Some(0)
                }
                WM_DPICHANGED => {
                    app.change_dpi((w & 0xffff) as u32);
                    let rect = &*(l as *const RECT);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    Some(0)
                }
                WM_COMMAND => {
                    let id = w & 0xffff;
                    let notification = (w >> 16) as u32;
                    if id == ID_SEARCH && notification == EN_CHANGE {
                        app.filter();
                    } else if id == ID_SIDE && notification == LBN_SELCHANGE {
                        let index = SendMessageW(app.side, LB_GETCURSEL, 0, 0) as usize;
                        let mut target = app
                            .shortcuts
                            .get(index)
                            .and_then(|place| place.path.clone());
                        if target.is_none() && GetKeyState(VK_DOWN as i32) < 0 {
                            target = app
                                .shortcuts
                                .iter()
                                .skip(index + 1)
                                .find_map(|place| place.path.clone());
                        } else if target.is_none() && GetKeyState(VK_UP as i32) < 0 {
                            target = app
                                .shortcuts
                                .iter()
                                .take(index)
                                .rev()
                                .find_map(|place| place.path.clone());
                        }
                        if let Some(path) = target {
                            app.navigate(path);
                        } else {
                            app.sync_sidebar();
                        }
                    } else if notification == 0 {
                        app.command(id);
                    }
                    Some(0)
                }
                WM_NOTIFY => {
                    let header = &*(l as *const NMHDR);
                    if header.hwndFrom == app.list {
                        match header.code {
                            NM_DBLCLK => app.open_selected(false),
                            LVN_BEGINDRAG => {
                                let paths = app.selected_paths();
                                if !paths.is_empty() && !app.busy {
                                    app.busy = true;
                                    app.update_status();
                                    let result = actions::drag(hwnd, &paths);
                                    app.busy = false;
                                    if let Err(err) = result {
                                        error(hwnd, err);
                                    }
                                    for index in 0..app.tabs.len() {
                                        app.reload(index);
                                    }
                                }
                            }
                            LVN_COLUMNCLICK => {
                                app.save_selection();
                                let column = match (*(l as *const NMLISTVIEW)).iSubItem {
                                    1 => 3,
                                    2 => 2,
                                    3 => 1,
                                    _ => 0,
                                };
                                let tab = app.current_mut();
                                if tab.sort == column {
                                    tab.descending = !tab.descending;
                                } else {
                                    tab.sort = column;
                                    tab.descending = false;
                                }
                                tab.sort_entries();
                                app.show_current();
                            }
                            LVN_ITEMCHANGED | LVN_ODSTATECHANGED => app.update_status(),
                            _ => {}
                        }
                    }
                    Some(0)
                }
                WM_CONTEXTMENU if w as HWND == app.list => {
                    let mut x = (l as u16) as i16 as i32;
                    let mut y = ((l >> 16) as u16) as i16 as i32;
                    if x == -1 && y == -1 {
                        let mut rect: RECT = zeroed();
                        GetWindowRect(app.list, &mut rect);
                        x = rect.left + app.scale(24);
                        y = rect.top + app.scale(52);
                    }
                    app.context_menu(x, y);
                    Some(0)
                }
                WM_CONTEXTMENU if w as HWND == app.side => {
                    app.sidebar_context(POINT {
                        x: l as u16 as i16 as i32,
                        y: (l >> 16) as u16 as i16 as i32,
                    });
                    Some(0)
                }
                WM_DROPFILES => {
                    app.dropped(w as HDROP);
                    Some(0)
                }
                WM_APPCOMMAND => {
                    match ((l >> 16) & 0xfff) as u32 {
                        APPCOMMAND_BROWSER_BACKWARD => app.history(false),
                        APPCOMMAND_BROWSER_FORWARD => app.history(true),
                        _ => return None,
                    }
                    Some(1)
                }
                WM_CLOSE if app.busy => {
                    MessageBoxW(
                        hwnd,
                        tr_w!("ファイル操作中です。完了してから終了してください。", "A file operation is in progress. Wait for it to finish before exiting."),
                        wide("rfm").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                    Some(0)
                }
                WM_CLOSE => {
                    if let Err(err) = settings::save_session(hwnd, &app.current().path) {
                        let message = tr_format!(
                            "終了時の状態を保存できませんでした。\n{err}\n\n保存せずに終了しますか？",
                            "Could not save the session.\n{err}\n\nExit without saving?"
                        );
                        if MessageBoxW(
                            hwnd,
                            wide(message).as_ptr(),
                            wide("rfm").as_ptr(),
                            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                        ) != IDYES
                        {
                            return Some(0);
                        }
                    }
                    None
                }
                WM_ENDSESSION if w != 0 => {
                    let _ = settings::save_session(hwnd, &app.current().path);
                    Some(0)
                }
                _ => None,
            }
        });
        handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, w, l))
    }
}

struct Prompt {
    edit: HWND,
    done: bool,
    accepted: bool,
}

unsafe extern "system" fn prompt_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Prompt;
        if !ptr.is_null() {
            match message {
                WM_COMMAND if w & 0xffff == IDOK as usize || w & 0xffff == IDCANCEL as usize => {
                    (*ptr).accepted = w & 0xffff == IDOK as usize;
                    (*ptr).done = true;
                    return 0;
                }
                WM_CLOSE => {
                    (*ptr).done = true;
                    return 0;
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

fn prompt(owner: HWND, title: &str, label: &str, initial: &str, font: HFONT) -> Option<String> {
    unsafe {
        let mut state = Prompt {
            edit: null_mut(),
            done: false,
            accepted: false,
        };
        let mut rect: RECT = zeroed();
        GetWindowRect(owner, &mut rect);
        let dpi = GetDpiForWindow(owner).max(96) as i32;
        let s = |n| n * dpi / 96;
        let width = s(460);
        let height = s(190);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            wide("rfm.prompt").as_ptr(),
            wide(title).as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_POPUP,
            rect.left + (rect.right - rect.left - width) / 2,
            rect.top + (rect.bottom - rect.top - height) / 2,
            width,
            height,
            owner,
            null_mut(),
            GetModuleHandleW(null()),
            &mut state as *mut _ as _,
        );
        if hwnd.is_null() {
            error(owner, io::Error::last_os_error());
            return None;
        }
        let make = |class, value: &str, style, x, y, w, h, id| {
            let child = CreateWindowExW(
                0,
                class,
                wide(value).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                s(x),
                s(y),
                s(w),
                s(h),
                hwnd,
                id as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            SendMessageW(child, WM_SETFONT, font as usize, 0);
            child
        };
        make(
            windows_sys::core::w!("STATIC"),
            label,
            0,
            16,
            12,
            410,
            24,
            0,
        );
        state.edit = make(
            windows_sys::core::w!("EDIT"),
            initial,
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
            16,
            40,
            410,
            28,
            10,
        );
        make(
            windows_sys::core::w!("BUTTON"),
            "OK",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            246,
            86,
            80,
            30,
            IDOK,
        );
        make(
            windows_sys::core::w!("BUTTON"),
            tr!("キャンセル", "Cancel"),
            WS_TABSTOP,
            336,
            86,
            90,
            30,
            IDCANCEL,
        );
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(state.edit);
        SendMessageW(state.edit, EM_SETSEL, 0, -1);
        let mut msg: MSG = zeroed();
        let mut quit = false;
        while !state.done {
            if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
                quit = true;
                break;
            }
            if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize {
                state.done = true;
            } else if msg.message == WM_KEYDOWN && msg.wParam == VK_RETURN as usize {
                state.accepted = true;
                state.done = true;
            } else if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let result = state.accepted.then(|| text(state.edit));
        EnableWindow(owner, 1);
        DestroyWindow(hwnd);
        SetForegroundWindow(owner);
        if quit {
            PostQuitMessage(0);
        }
        result
    }
}

pub fn run() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let com = OleInitialize(null());
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES | ICC_STANDARD_CLASSES,
        });
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: LoadIconW(instance, APP_ICON_ID as _),
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            lpszClassName: windows_sys::core::w!("rfm.main"),
            ..zeroed()
        };
        if RegisterClassW(&class) == 0 {
            error(null_mut(), io::Error::last_os_error());
            return;
        }
        let prompt_class = WNDCLASSW {
            lpfnWndProc: Some(prompt_proc),
            lpszClassName: windows_sys::core::w!("rfm.prompt"),
            ..class
        };
        RegisterClassW(&prompt_class);
        let (saved, load_error) = match settings::load() {
            Ok(saved) => (saved, None),
            Err(err) => (settings::Settings::default(), Some(err)),
        };
        crate::i18n::initialize(saved.appearance.language);
        if let Some(err) = load_error {
            error(
                null_mut(),
                tr_format!(
                    "設定を読み込めませんでした。\n{err}",
                    "Could not load settings.\n{err}"
                ),
            );
        }
        let paths: Vec<PathBuf> = std::env::args_os()
            .skip(1)
            .map(|p| {
                let path = PathBuf::from(p);
                if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| home())
                        .join(path)
                }
            })
            .collect();
        let paths = if paths.is_empty() {
            vec![settings::startup_path(&saved, home())]
        } else {
            paths
        };
        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES | WS_EX_CONTROLPARENT,
            wide("rfm.main").as_ptr(),
            wide("rfm").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1200,
            760,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            error(null_mut(), io::Error::last_os_error());
            return;
        }
        if let Some(placement) = &saved.placement
            && let Err(err) = placement.restore_hidden(hwnd)
        {
            error(
                hwnd,
                tr_format!(
                    "ウィンドウの位置を復元できませんでした。\n{err}",
                    "Could not restore the window position.\n{err}"
                ),
            );
        }
        let app = match App::new(hwnd, paths, saved.appearance.icon_theme) {
            Ok(app) => app,
            Err(err) => {
                error(hwnd, err);
                DestroyWindow(hwnd);
                EVENTS.with(|events| events.borrow_mut().take());
                OleUninitialize();
                return;
            }
        };
        app.layout();
        APP.with(|state| *state.borrow_mut() = Some(app));
        ShowWindow(
            hwnd,
            saved
                .placement
                .as_ref()
                .map_or(SW_SHOWNORMAL, settings::Placement::show_command),
        );
        UpdateWindow(hwnd);
        APP.with(|state| {
            if let Some(app) = state.borrow().as_ref() {
                SetFocus(app.list);
            }
        });
        let mut msg: MSG = zeroed();
        loop {
            let result = GetMessageW(&mut msg, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            let consumed = APP.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .is_some_and(|app| app.keyboard(&msg))
            });
            let label_message = APP.with(|state| {
                state.borrow().as_ref().is_some_and(|app| {
                    let edit = SendMessageW(app.list, LVM_GETEDITCONTROL, 0, 0) as HWND;
                    !edit.is_null() && msg.hwnd == edit
                })
            });
            if !consumed && (label_message || IsDialogMessageW(hwnd, &msg) == 0) {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            APP.with(|state| {
                if let Some(app) = state.borrow_mut().as_mut() {
                    app.completions();
                }
            });
        }
        DISPLAY.with(|display| *display.borrow_mut() = None);
        LABEL_EDIT.with(|edit| edit.borrow_mut().take());
        APP.with(|state| {
            state.borrow_mut().take();
        });
        EVENTS.with(|events| {
            events.borrow_mut().take();
        });
        if com >= 0 {
            OleUninitialize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidebar_place(label: &str, path: Option<&str>, quick: bool) -> Place {
        Place {
            label: wide(label),
            glyph: 0,
            image: Cell::new(None),
            path: path.map(PathBuf::from),
            quick,
        }
    }

    #[test]
    fn sidebar_refresh_preserves_path_and_section_anchors() {
        let quick = sidebar_place("クイック アクセス", None, false);
        let a = sidebar_place("A", Some(r"C:\A"), true);
        let b = sidebar_place("B", Some(r"C:\B"), true);
        let pc = sidebar_place("PC", None, false);
        let pc_a = sidebar_place("A", Some(r"C:\A"), false);
        let previous = vec![
            quick.clone(),
            a.clone(),
            b.clone(),
            pc.clone(),
            pc_a.clone(),
        ];
        let mut renamed_b = b.clone();
        renamed_b.label = wide("Renamed B");
        let places = vec![
            quick,
            sidebar_place("New", Some(r"C:\New"), true),
            renamed_b,
            a,
            pc,
            pc_a,
        ];
        assert_eq!(sidebar_position(&previous, &places, 4, 1), (Some(5), 3));
        assert_eq!(sidebar_position(&previous, &places, 2, 2), (Some(2), 2));
    }

    #[test]
    fn sidebar_refresh_anchors_deleted_top_to_next_surviving_row() {
        let a = sidebar_place("A", Some(r"C:\A"), true);
        let b = sidebar_place("B", Some(r"C:\B"), true);
        let c = sidebar_place("C", Some(r"C:\C"), true);
        let previous = vec![a.clone(), b.clone(), c.clone()];
        let places = vec![a, c];
        assert_eq!(sidebar_position(&previous, &places, 1, 1), (None, 1));
        assert_eq!(sidebar_position(&previous, &[], 2, 2), (None, 0));
        assert_eq!(sidebar_position(&[], &places, usize::MAX, 0), (None, 0));
    }

    #[test]
    fn back_restores_parent_child_and_scroll_after_child_operations() {
        let root = PathBuf::from(r"C:\root");
        let child = root.join("child");
        let mut tab = Tab::new(1, root.clone());
        tab.selected = vec![root.join("other.txt")];
        tab.scroll = 80;
        tab.visit(child.clone());
        tab.selected = vec![child.join("renamed.txt")];
        tab.scroll = 3;
        assert!(tab.travel(false));
        assert_eq!(tab.path, root);
        assert_eq!(tab.selected, vec![child.clone()]);
        assert_eq!(tab.scroll, 80);
        assert!(tab.travel(true));
        assert_eq!(tab.path, child);
        assert_eq!(tab.selected, vec![child.join("renamed.txt")]);
        assert_eq!(tab.scroll, 3);
    }

    #[test]
    fn revisits_and_branching_keep_independent_history_states() {
        let root = PathBuf::from(r"C:\root");
        let child = root.join("child");
        let grandchild = child.join("nested");
        let mut tab = Tab::new(1, root.clone());
        tab.visit(child.clone());
        tab.scroll = 27;
        tab.visit(grandchild.clone());
        assert!(tab.travel(false));
        assert_eq!(tab.selected, vec![grandchild.clone()]);
        assert_eq!(tab.scroll, 27);
        assert!(tab.travel(false));
        assert_eq!(tab.selected, vec![child.clone()]);
        assert!(tab.travel(true));
        assert_eq!(tab.selected, vec![grandchild.clone()]);
        tab.visit(root.join("sibling"));
        assert!(!tab.travel(true));
        assert!(tab.travel(false));
        assert_eq!(tab.path, child);
        assert_eq!(tab.selected, vec![grandchild]);
        assert_eq!(tab.scroll, 27);
    }

    #[test]
    fn history_jump_keeps_location_state_and_invalid_jumps_do_not_consume_it() {
        let root = PathBuf::from(r"C:\root");
        let child = root.join("child");
        let nested = child.join("nested");
        let mut tab = Tab::new(1, root.clone());
        tab.scroll = 40;
        tab.visit(child.clone());
        tab.scroll = 12;
        tab.visit(nested.clone());
        tab.selected = vec![nested.join("work.txt")];
        assert!(!tab.travel_to(99));
        assert_eq!(tab.selected, vec![nested.join("work.txt")]);
        assert!(tab.travel_to(0));
        assert_eq!(tab.path, root);
        assert_eq!(tab.selected, vec![child.clone()]);
        assert_eq!(tab.scroll, 40);
        assert!(tab.travel_to(2));
        assert_eq!(tab.path, nested);
        assert_eq!(tab.selected, vec![tab.path.join("work.txt")]);
        assert!(tab.travel_to(1));
        assert_eq!(tab.path, child);
        assert_eq!(tab.scroll, 12);
    }

    #[test]
    fn inline_rename_preserves_hidden_extension_and_cancellation() {
        let mut target = RenameTarget {
            tab_id: 1,
            path: PathBuf::from(r"C:\root\original.txt"),
            suffix: Some("txt".into()),
        };
        assert_eq!(
            target.proposed_name(Some("日本語".into())),
            Some("日本語.txt".into())
        );
        assert_eq!(
            target.proposed_name(Some("new.md".into())),
            Some("new.md.txt".into())
        );
        assert_eq!(target.proposed_name(None), None);
        assert_eq!(target.proposed_name(Some(String::new())), None);
        assert_eq!(target.proposed_name(Some("original".into())), None);
        target.suffix = None;
        assert_eq!(
            target.proposed_name(Some("new.md".into())),
            Some("new.md".into())
        );
    }
}
