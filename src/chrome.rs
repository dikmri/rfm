//! Compact, owner-drawn Explorer chrome. Fonts are borrowed from the window owner.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::icons::{self, Icon};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::LazyLock,
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::*,
            Input::KeyboardAndMouse::*,
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

pub const TAB_SELECT: u32 = WM_APP + 10;
pub const TAB_CLOSE: u32 = WM_APP + 11;
pub const TAB_NEW: u32 = WM_APP + 12;

#[derive(Clone, Copy)]
pub struct Style {
    pub font: HFONT,
    pub icons: HFONT,
    pub dpi: u32,
}

const STATE: *const u16 = w!("rfm.chrome.state");
const TOOLTIP: *const u16 = w!("rfm.chrome.tooltip");
const CLASS: *const u16 = w!("rfm.ExplorerTabs");
const SUBCLASS: usize = 0x52464d01;
const TOOLTIP_SUBCLASS: usize = 0x52464d02;
const WHITE: u32 = rgb(255, 255, 255);
const STRIP: u32 = rgb(243, 243, 243);
const SIDE: u32 = rgb(255, 255, 255);
const INK: u32 = rgb(32, 32, 32);
const MUTED: u32 = rgb(105, 105, 105);
const HOVER: u32 = rgb(232, 232, 232);
const SELECTED: u32 = rgb(204, 232, 255);
const ACCENT: u32 = rgb(0, 103, 192);

const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}
fn px(style: Style, n: i32) -> i32 {
    ((n as i64 * style.dpi.max(96) as i64 + 48) / 96) as i32
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn text_len(s: &[u16]) -> i32 {
    s.iter()
        .position(|&c| c == 0)
        .unwrap_or(s.len())
        .min(i32::MAX as usize) as i32
}
fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right: right.max(left),
        bottom: bottom.max(top),
    }
}
fn contains(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}
fn point(l: LPARAM) -> (i32, i32) {
    (
        (l as u16 as i16) as i32,
        ((l as usize >> 16) as u16 as i16) as i32,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    Tab(usize),
    Close(usize),
    New,
    Left,
    Right,
}
struct Tabs {
    labels: Vec<Vec<u16>>,
    selected: usize,
    scroll: i32,
    wheel: i32,
    hot: Hit,
    down: Hit,
}
enum Kind {
    Tabs(Tabs),
    Button {
        glyph: u16,
        label: Vec<u16>,
        tooltip: Vec<u16>,
        hot: bool,
    },
    Sidebar {
        hover: i32,
    },
}
struct State {
    style: Style,
    kind: Kind,
}

unsafe fn state(hwnd: HWND) -> *mut State {
    GetPropW(hwnd, STATE) as *mut State
}

pub fn draw_shell_icon(dc: HDC, area: RECT, index: i32, style: Style) {
    static IMAGES: LazyLock<usize> = LazyLock::new(|| crate::shell::system_image_list() as usize);
    unsafe {
        let size = px(style, 16)
            .min((area.right - area.left).min(area.bottom - area.top))
            .max(1);
        let params = IMAGELISTDRAWPARAMS {
            cbSize: size_of::<IMAGELISTDRAWPARAMS>() as u32,
            himl: *IMAGES as HIMAGELIST,
            i: index,
            hdcDst: dc,
            x: area.left + (area.right - area.left - size) / 2,
            y: area.top + (area.bottom - area.top - size) / 2,
            cx: size,
            cy: size,
            rgbBk: 0xffff_ffff,
            rgbFg: 0xffff_ffff,
            fStyle: ILD_TRANSPARENT | ILD_SCALE,
            ..zeroed()
        };
        ImageList_DrawIndirect(&params);
    }
}

pub fn draw_folder(dc: HDC, area: RECT, style: Style) {
    let size = px(style, 16)
        .min(area.right - area.left)
        .min(area.bottom - area.top)
        .max(1);
    let x = area.left + (area.right - area.left - size) / 2;
    let y = area.top + (area.bottom - area.top - size) / 2;
    icons::draw(
        dc,
        Icon::Folder,
        rect(x, y, x + size, y + size),
        true,
        false,
        false,
    );
}
unsafe fn fill(dc: HDC, area: RECT, color: u32) {
    SetDCBrushColor(dc, color);
    FillRect(dc, &area, GetStockObject(DC_BRUSH) as HBRUSH);
}
unsafe fn rounded(dc: HDC, area: RECT, color: u32, radius: i32) {
    if area.right <= area.left || area.bottom <= area.top {
        return;
    }
    let brush = SelectObject(dc, GetStockObject(DC_BRUSH));
    let pen = SelectObject(dc, GetStockObject(NULL_PEN));
    SetDCBrushColor(dc, color);
    RoundRect(
        dc,
        area.left,
        area.top,
        area.right,
        area.bottom,
        radius,
        radius,
    );
    SelectObject(dc, pen);
    SelectObject(dc, brush);
}
unsafe fn text(dc: HDC, font: HFONT, value: &[u16], mut area: RECT, color: u32, flags: u32) {
    if area.right <= area.left || area.bottom <= area.top {
        return;
    }
    let old = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    DrawTextW(
        dc,
        value.as_ptr(),
        text_len(value),
        &mut area,
        flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    SelectObject(dc, old);
}
unsafe fn line(dc: HDC, a: (i32, i32), b: (i32, i32), color: u32) {
    let old = SelectObject(dc, GetStockObject(DC_PEN));
    SetDCPenColor(dc, color);
    MoveToEx(dc, a.0, a.1, null_mut());
    LineTo(dc, b.0, b.1);
    SelectObject(dc, old);
}
unsafe fn cross(dc: HDC, area: RECT, style: Style, color: u32) {
    let x = (area.left + area.right) / 2;
    let y = (area.top + area.bottom) / 2;
    let d = px(style, 2).max(2);
    line(dc, (x - d, y - d), (x + d + 1, y + d + 1), color);
    line(dc, (x - d, y + d), (x + d + 1, y - d - 1), color);
}
unsafe fn track(hwnd: HWND) {
    let mut t = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    TrackMouseEvent(&mut t);
}

// All paint and hit testing use this exact geometry, including the viewport clip.
struct Layout {
    view: RECT,
    plus: RECT,
    left: RECT,
    right: RECT,
    tab_width: i32,
    max_scroll: i32,
}
fn layout(width: i32, height: i32, count: usize, style: Style) -> Layout {
    let gap = px(style, 2).min(width.max(0));
    let unit = px(style, 24);
    let top = px(style, 1).min(height.max(0));
    let bottom = (height - px(style, 1)).max(top);
    let available = (width - gap - unit - px(style, 6)).max(0);
    let overflow = (count as i64 * px(style, 80) as i64) > available as i64;
    let reserved = if overflow { unit * 2 } else { 0 };
    let view_width = (available - reserved).max(0);
    let tab_width = if count == 0 {
        px(style, 160)
    } else {
        ((view_width as i64 / count as i64)
            .min(px(style, 160) as i64)
            .max(px(style, 80) as i64)) as i32
    };
    let total = (count as i64 * tab_width as i64).min(i32::MAX as i64) as i32;
    let view = rect(gap, top, gap + view_width, bottom);
    let after_tabs = if overflow {
        view.right
    } else {
        view.left + total
    };
    let empty = rect(0, 0, 0, 0);
    let (left, right, plus_x) = if overflow {
        // On exceptionally narrow windows reserve the add button first; wheel
        // scrolling remains available even when the arrow pair is clipped.
        (
            rect(
                after_tabs,
                top,
                (after_tabs + unit).min((width - unit).max(0)),
                bottom,
            ),
            rect(
                after_tabs + unit,
                top,
                (after_tabs + unit * 2).min((width - unit).max(0)),
                bottom,
            ),
            (width - unit - px(style, 6)).max(0),
        )
    } else {
        (empty, empty, after_tabs + px(style, 4))
    };
    Layout {
        view,
        plus: rect(plus_x, top, (plus_x + unit).min(width.max(0)), bottom),
        left,
        right,
        tab_width,
        max_scroll: (total - view_width).max(0),
    }
}
fn tab_rect(l: &Layout, index: usize, scroll: i32) -> RECT {
    let x = (l.view.left as i64 + index as i64 * l.tab_width as i64 - scroll as i64)
        .clamp(i32::MIN as i64, i32::MAX as i64 - l.tab_width as i64) as i32;
    rect(x, l.view.top, x + l.tab_width, l.view.bottom)
}
fn close_rect(tab: RECT, style: Style) -> RECT {
    let size = px(style, 16).min((tab.bottom - tab.top).max(0));
    let y = (tab.top + tab.bottom - size) / 2;
    rect(
        tab.right - px(style, 5) - size,
        y,
        tab.right - px(style, 5),
        y + size,
    )
}
fn hit(l: &Layout, tabs: &Tabs, style: Style, x: i32, y: i32) -> Hit {
    if contains(l.plus, x, y) {
        return Hit::New;
    }
    if contains(l.left, x, y) {
        return Hit::Left;
    }
    if contains(l.right, x, y) {
        return Hit::Right;
    }
    if contains(l.view, x, y) {
        let logical = x - l.view.left + tabs.scroll;
        let index = (logical / l.tab_width) as usize;
        if index < tabs.labels.len() {
            return if contains(close_rect(tab_rect(l, index, tabs.scroll), style), x, y) {
                Hit::Close(index)
            } else {
                Hit::Tab(index)
            };
        }
    }
    Hit::None
}
unsafe fn client_layout(hwnd: HWND, style: Style, tabs: &Tabs) -> Layout {
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    layout(r.right, r.bottom, tabs.labels.len(), style)
}
unsafe fn reveal(hwnd: HWND, ptr: *mut State) {
    let style = (*ptr).style;
    if let Kind::Tabs(tabs) = &mut (*ptr).kind {
        let l = client_layout(hwnd, style, tabs);
        let start = (tabs.selected as i64 * l.tab_width as i64).min(i32::MAX as i64) as i32;
        let width = l.view.right - l.view.left;
        if start < tabs.scroll {
            tabs.scroll = start;
        }
        if start.saturating_add(l.tab_width) > tabs.scroll.saturating_add(width) {
            tabs.scroll = start.saturating_add(l.tab_width).saturating_sub(width);
        }
        tabs.scroll = tabs.scroll.clamp(0, l.max_scroll);
        tabs.hot = Hit::None;
    }
}

pub fn create_tabs(parent: HWND, id: usize, style: Style) -> HWND {
    static REGISTERED: LazyLock<bool> = LazyLock::new(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tabs_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: CLASS,
            ..zeroed()
        };
        RegisterClassW(&wc) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS
    });
    unsafe {
        if !*REGISTERED {
            return null_mut();
        }
        let hwnd = CreateWindowExW(
            0,
            CLASS,
            tr_w!("タブ", "Tabs"),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            parent,
            id as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return hwnd;
        }
        let ptr = Box::into_raw(Box::new(State {
            style,
            kind: Kind::Tabs(Tabs {
                labels: Vec::new(),
                selected: 0,
                scroll: 0,
                wheel: 0,
                hot: Hit::None,
                down: Hit::None,
            }),
        }));
        if SetPropW(hwnd, STATE, ptr as HANDLE) == 0 {
            drop(Box::from_raw(ptr));
            DestroyWindow(hwnd);
            return null_mut();
        }
        ShowWindow(hwnd, SW_SHOW);
        hwnd
    }
}

pub fn set_tabs(hwnd: HWND, labels: &[Vec<u16>], selected: usize) {
    unsafe {
        let ptr = state(hwnd);
        if ptr.is_null() {
            return;
        }
        if let Kind::Tabs(tabs) = &mut (*ptr).kind {
            if tabs.labels != labels {
                tabs.labels = labels.to_vec();
            }
            tabs.selected = selected.min(tabs.labels.len().saturating_sub(1));
            tabs.down = Hit::None;
        } else {
            return;
        }
        reveal(hwnd, ptr);
        InvalidateRect(hwnd, null(), 0);
    }
}

pub fn set_style(hwnd: HWND, style: Style) {
    unsafe {
        let ptr = state(hwnd);
        if ptr.is_null() {
            return;
        }
        (*ptr).style = style;
        let sidebar = matches!((*ptr).kind, Kind::Sidebar { .. });
        if matches!((*ptr).kind, Kind::Tabs(_)) {
            reveal(hwnd, ptr);
        }
        // Do not retain a reference into userdata across these synchronous sends.
        SendMessageW(hwnd, WM_SETFONT, style.font as usize, 0);
        if sidebar {
            SendMessageW(hwnd, LB_SETITEMHEIGHT, 0, px(style, 22) as LPARAM);
        }
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn paint_tabs(hwnd: HWND, dc: HDC, ptr: *mut State) {
    let style = (*ptr).style;
    let Kind::Tabs(tabs) = &(*ptr).kind else {
        return;
    };
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    fill(dc, client, STRIP);
    let l = layout(client.right, client.bottom, tabs.labels.len(), style);
    let saved = SaveDC(dc);
    IntersectClipRect(dc, l.view.left, l.view.top, l.view.right, l.view.bottom);
    let first = (tabs.scroll / l.tab_width).max(0) as usize;
    let last = ((tabs.scroll.saturating_add(l.view.right - l.view.left)) / l.tab_width + 1).max(0)
        as usize;
    for i in first..last.min(tabs.labels.len()) {
        let tab = tab_rect(&l, i, tabs.scroll);
        let body = rect(
            tab.left + px(style, 1),
            tab.top,
            tab.right - px(style, 1),
            tab.bottom,
        );
        let hot = matches!(tabs.hot, Hit::Tab(n) | Hit::Close(n) if n == i);
        if i == tabs.selected {
            rounded(dc, body, WHITE, px(style, 4));
        } else if hot {
            rounded(dc, body, HOVER, px(style, 4));
        }
        let icon = rect(
            tab.left + px(style, 7),
            tab.top,
            tab.left + px(style, 25),
            tab.bottom,
        );
        draw_folder(dc, icon, style);
        let close = close_rect(tab, style);
        text(
            dc,
            style.font,
            &tabs.labels[i],
            rect(
                icon.right + px(style, 5),
                tab.top,
                close.left - px(style, 4),
                tab.bottom,
            ),
            INK,
            DT_END_ELLIPSIS,
        );
        if tabs.hot == Hit::Close(i) {
            rounded(
                dc,
                close,
                if tabs.down == Hit::Close(i) {
                    rgb(214, 214, 214)
                } else {
                    HOVER
                },
                px(style, 6),
            );
        }
        cross(dc, close, style, MUTED);
        if i != tabs.selected && !hot && i + 1 < tabs.labels.len() && i + 1 != tabs.selected {
            line(
                dc,
                (tab.right - 1, tab.top + px(style, 9)),
                (tab.right - 1, tab.bottom - px(style, 9)),
                rgb(218, 218, 218),
            );
        }
    }
    RestoreDC(dc, saved);
    for (area, target, direction) in [
        (l.left, Hit::Left, -1),
        (l.right, Hit::Right, 1),
        (l.plus, Hit::New, 0),
    ] {
        if area.right <= area.left {
            continue;
        }
        if tabs.hot == target {
            rounded(dc, area, HOVER, px(style, 6));
        }
        let x = (area.left + area.right) / 2;
        let y = (area.top + area.bottom) / 2;
        let d = px(style, 4).max(2);
        let color = if (target == Hit::Left && tabs.scroll == 0)
            || (target == Hit::Right && tabs.scroll >= l.max_scroll)
        {
            rgb(170, 170, 170)
        } else {
            INK
        };
        if direction == 0 {
            line(dc, (x - d, y), (x + d + 1, y), color);
            line(dc, (x, y - d), (x, y + d + 1), color);
        } else {
            line(
                dc,
                (x - direction * d / 2, y - d),
                (x + direction * d / 2, y),
                color,
            );
            line(
                dc,
                (x + direction * d / 2, y),
                (x - direction * d / 2, y + d + 1),
                color,
            );
        }
    }
}

unsafe extern "system" fn tabs_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    std::panic::catch_unwind(|| unsafe { tabs_message(hwnd, msg, w, l) }).unwrap_or(0)
}
unsafe fn tabs_message(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = state(hwnd);
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let saved = SaveDC(dc);
            paint_tabs(hwnd, dc, ptr);
            RestoreDC(dc, saved);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            reveal(hwnd, ptr);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_GETTEXTLENGTH | WM_GETTEXT => {
            let Kind::Tabs(tabs) = &(*ptr).kind else {
                return 0;
            };
            let label = tabs
                .labels
                .get(tabs.selected)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let len = text_len(label) as usize;
            if msg == WM_GETTEXTLENGTH {
                return len as LRESULT;
            }
            if w == 0 || l == 0 {
                return 0;
            }
            let len = len.min(w - 1);
            std::ptr::copy_nonoverlapping(label.as_ptr(), l as *mut u16, len);
            *(l as *mut u16).add(len) = 0;
            len as LRESULT
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONUP => {
            let style = (*ptr).style;
            let (x, y) = point(l);
            let target = if let Kind::Tabs(tabs) = &(*ptr).kind {
                hit(&client_layout(hwnd, style, tabs), tabs, style, x, y)
            } else {
                Hit::None
            };
            let mut action = Hit::None;
            if let Kind::Tabs(tabs) = &mut (*ptr).kind {
                tabs.hot = target;
                if msg == WM_LBUTTONDOWN {
                    tabs.down = target;
                }
                if msg == WM_LBUTTONUP {
                    if tabs.down == target {
                        action = target;
                    }
                    tabs.down = Hit::None;
                }
                if msg == WM_MBUTTONUP {
                    action = match target {
                        Hit::Tab(i) | Hit::Close(i) => Hit::Close(i),
                        _ => Hit::None,
                    };
                }
            }
            if msg == WM_MOUSEMOVE {
                track(hwnd);
            }
            if msg == WM_LBUTTONDOWN {
                SetCapture(hwnd);
            }
            if msg == WM_LBUTTONUP {
                ReleaseCapture();
            }
            match action {
                Hit::Tab(i) => {
                    PostMessageW(GetParent(hwnd), TAB_SELECT, i, 0);
                }
                Hit::Close(i) => {
                    PostMessageW(GetParent(hwnd), TAB_CLOSE, i, 0);
                }
                Hit::New => {
                    PostMessageW(GetParent(hwnd), TAB_NEW, 0, 0);
                }
                Hit::Left | Hit::Right => {
                    if let Kind::Tabs(tabs) = &mut (*ptr).kind {
                        let geometry = client_layout(hwnd, style, tabs);
                        let step = geometry.tab_width * if action == Hit::Left { -1 } else { 1 };
                        tabs.scroll = tabs
                            .scroll
                            .saturating_add(step)
                            .clamp(0, geometry.max_scroll);
                        tabs.hot = hit(&geometry, tabs, style, x, y);
                    }
                }
                _ => {}
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let style = (*ptr).style;
            if let Kind::Tabs(tabs) = &mut (*ptr).kind {
                let geometry = client_layout(hwnd, style, tabs);
                let delta = ((w >> 16) as u16 as i16) as i32;
                tabs.wheel += if msg == WM_MOUSEWHEEL { -delta } else { delta };
                // Pixel-proportional movement preserves high-resolution touchpad input.
                let movement = tabs.wheel * geometry.tab_width / 120;
                if movement != 0 {
                    tabs.scroll = tabs
                        .scroll
                        .saturating_add(movement)
                        .clamp(0, geometry.max_scroll);
                    tabs.wheel = 0;
                    tabs.hot = Hit::None;
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSELEAVE | WM_CAPTURECHANGED | WM_CANCELMODE => {
            if let Kind::Tabs(tabs) = &mut (*ptr).kind {
                tabs.hot = Hit::None;
                if msg != WM_MOUSELEAVE {
                    tabs.down = Hit::None;
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_NCDESTROY => {
            RemovePropW(hwnd, STATE);
            drop(Box::from_raw(ptr));
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe extern "system" fn tooltip_parent_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    std::panic::catch_unwind(|| unsafe {
        if msg == WM_NCDESTROY {
            let tooltip = RemovePropW(hwnd, TOOLTIP) as HWND;
            RemoveWindowSubclass(hwnd, Some(tooltip_parent_proc), TOOLTIP_SUBCLASS);
            if !tooltip.is_null() && IsWindow(tooltip) != 0 {
                DestroyWindow(tooltip);
            }
        }
        DefSubclassProc(hwnd, msg, w, l)
    })
    .unwrap_or(0)
}
unsafe fn tooltip_for(parent: HWND) -> HWND {
    let existing = GetPropW(parent, TOOLTIP) as HWND;
    if !existing.is_null() {
        return existing;
    }
    let tooltip = CreateWindowExW(
        WS_EX_TOPMOST,
        TOOLTIPS_CLASSW,
        null(),
        WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        parent,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if tooltip.is_null() {
        return tooltip;
    }
    if SetPropW(parent, TOOLTIP, tooltip as HANDLE) == 0
        || SetWindowSubclass(parent, Some(tooltip_parent_proc), TOOLTIP_SUBCLASS, 0) == 0
    {
        RemovePropW(parent, TOOLTIP);
        DestroyWindow(tooltip);
        return null_mut();
    }
    SendMessageW(tooltip, TTM_SETMAXTIPWIDTH, 0, 420);
    tooltip
}
unsafe fn tool_info(parent: HWND, hwnd: HWND, label: *mut u16) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: parent,
        uId: hwnd as usize,
        lpszText: label,
        ..zeroed()
    }
}

pub fn create_button(
    parent: HWND,
    id: usize,
    glyph: u16,
    label: &str,
    tooltip: &str,
    style: Style,
) -> HWND {
    unsafe {
        let label_w = wide(label);
        let tooltip_w = wide(tooltip);
        let name = if label.is_empty() {
            &tooltip_w
        } else {
            &label_w
        };
        let hwnd = CreateWindowExW(
            0,
            WC_BUTTONW,
            name.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
            0,
            0,
            0,
            0,
            parent,
            id as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return hwnd;
        }
        let ptr = Box::into_raw(Box::new(State {
            style,
            kind: Kind::Button {
                glyph,
                label: label_w,
                tooltip: tooltip_w,
                hot: false,
            },
        }));
        if SetPropW(hwnd, STATE, ptr as HANDLE) == 0
            || SetWindowSubclass(hwnd, Some(control_proc), SUBCLASS, 0) == 0
        {
            RemovePropW(hwnd, STATE);
            drop(Box::from_raw(ptr));
            DestroyWindow(hwnd);
            return null_mut();
        }
        let manager = tooltip_for(parent);
        if !manager.is_null() && !tooltip.is_empty() {
            let text = match &mut (*ptr).kind {
                Kind::Button { tooltip, .. } => tooltip.as_mut_ptr(),
                _ => null_mut(),
            };
            let info = tool_info(parent, hwnd, text);
            SendMessageW(manager, TTM_ADDTOOLW, 0, &info as *const _ as LPARAM);
        }
        SendMessageW(hwnd, WM_SETFONT, style.font as usize, 0);
        hwnd
    }
}

pub fn draw_button(item: &DRAWITEMSTRUCT) -> bool {
    unsafe {
        let ptr = state(item.hwndItem);
        if ptr.is_null() {
            return false;
        }
        let Kind::Button {
            glyph, label, hot, ..
        } = &(*ptr).kind
        else {
            return false;
        };
        let style = (*ptr).style;
        let dc = item.hDC;
        let saved = SaveDC(dc);
        let r = item.rcItem;
        fill(dc, r, WHITE);
        let pressed = item.itemState & ODS_SELECTED != 0;
        let disabled = item.itemState & ODS_DISABLED != 0;
        let role = icons::from_glyph(*glyph);
        let navigation = matches!(
            role,
            Some(Icon::Back | Icon::Forward | Icon::Up | Icon::Refresh | Icon::History)
        );
        let body = rect(
            r.left + px(style, 2),
            r.top + px(style, 2),
            r.right - px(style, 2),
            r.bottom - px(style, 2),
        );
        if !disabled && (*hot || pressed) {
            if navigation {
                fill(
                    dc,
                    body,
                    if pressed {
                        rgb(204, 228, 247)
                    } else {
                        rgb(229, 243, 255)
                    },
                );
            } else {
                rounded(
                    dc,
                    body,
                    if pressed { rgb(219, 219, 219) } else { HOVER },
                    px(style, 3),
                );
            }
        }
        let color = if disabled { rgb(165, 165, 165) } else { INK };
        let has_label = text_len(label) != 0;
        if *glyph != 0 {
            let icon = if has_label {
                rect(
                    r.left + px(style, 9),
                    r.top,
                    r.left + px(style, 29),
                    r.bottom,
                )
            } else {
                r
            };
            let size = px(style, 16)
                .min(icon.right - icon.left)
                .min(icon.bottom - icon.top)
                .max(1);
            let x = icon.left + (icon.right - icon.left - size) / 2;
            let y = icon.top + (icon.bottom - icon.top - size) / 2;
            let bounds = rect(x, y, x + size, y + size);
            if !role.is_some_and(|icon| icons::draw(dc, icon, bounds, !disabled, *hot, pressed)) {
                text(dc, style.icons, &[*glyph], bounds, color, DT_CENTER);
            }
        }
        if has_label {
            let left = if *glyph == 0 {
                r.left + px(style, 10)
            } else {
                r.left + px(style, 30)
            };
            text(
                dc,
                style.font,
                label,
                rect(left, r.top, r.right - px(style, 9), r.bottom),
                color,
                DT_END_ELLIPSIS | if *glyph == 0 { DT_CENTER } else { DT_LEFT },
            );
        }
        if item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0 {
            DrawFocusRect(
                dc,
                &rect(
                    body.left + px(style, 3),
                    body.top + px(style, 3),
                    body.right - px(style, 3),
                    body.bottom - px(style, 3),
                ),
            );
        }
        RestoreDC(dc, saved);
        true
    }
}

pub fn create_sidebar_host(parent: HWND) -> HWND {
    static REGISTERED: LazyLock<bool> = LazyLock::new(|| unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(sidebar_host_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: w!("rfm.SidebarHost"),
            ..zeroed()
        };
        RegisterClassW(&wc) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS
    });
    unsafe {
        if !*REGISTERED {
            return null_mut();
        }
        // The system LISTBOX class has CS_PARENTDC, which is incompatible with
        // WS_EX_COMPOSITED. Buffer a class-style-zero host instead, so native
        // scroll/selection drawing is confined to the sidebar surface.
        CreateWindowExW(
            WS_EX_COMPOSITED | WS_EX_CONTROLPARENT,
            w!("rfm.SidebarHost"),
            null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            parent,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    }
}

unsafe extern "system" fn sidebar_host_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_SIZE => {
                let child = GetWindow(hwnd, GW_CHILD);
                if !child.is_null() {
                    let mut client: RECT = zeroed();
                    GetClientRect(hwnd, &mut client);
                    MoveWindow(child, 0, 0, client.right, client.bottom, 1);
                }
                0
            }
            WM_COMMAND | WM_NOTIFY | WM_DRAWITEM | WM_MEASUREITEM | WM_CONTEXTMENU
            | WM_CTLCOLORLISTBOX => SendMessageW(GetParent(hwnd), message, w, l),
            WM_ERASEBKGND => 1,
            _ => DefWindowProcW(hwnd, message, w, l),
        }
    }
}

pub fn style_sidebar(hwnd: HWND, style: Style) {
    unsafe {
        if state(hwnd).is_null() {
            let ptr = Box::into_raw(Box::new(State {
                style,
                kind: Kind::Sidebar { hover: -1 },
            }));
            if SetPropW(hwnd, STATE, ptr as HANDLE) == 0
                || SetWindowSubclass(hwnd, Some(control_proc), SUBCLASS, 0) == 0
            {
                RemovePropW(hwnd, STATE);
                drop(Box::from_raw(ptr));
                return;
            }
        }
        // The owner creates LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOTIFY.
        // Remove only the old native frame, retaining the listbox's input model.
        let bits = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, bits & !(WS_BORDER as isize));
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_CLIENTEDGE as isize));
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        set_style(hwnd, style);
    }
}

pub fn draw_sidebar(item: &DRAWITEMSTRUCT, label: &[u16], glyph: u16, section: bool, style: Style) {
    unsafe {
        let dc = item.hDC;
        let saved = SaveDC(dc);
        let r = item.rcItem;
        fill(dc, r, SIDE);
        let ptr = state(item.hwndItem);
        let hot = if ptr.is_null() {
            false
        } else {
            matches!(&(*ptr).kind, Kind::Sidebar { hover } if *hover >= 0 && *hover as u32 == item.itemID)
        };
        let selected = item.itemState & ODS_SELECTED != 0 && !section;
        let body = rect(
            r.left + px(style, 1),
            r.top,
            r.right - px(style, 1),
            r.bottom,
        );
        if selected || (hot && !section) {
            rounded(
                dc,
                body,
                if selected { SELECTED } else { HOVER },
                px(style, 1),
            );
        }
        let text_left = if section {
            r.left + px(style, 8)
        } else {
            r.left + px(style, 36)
        };
        if glyph != 0 {
            let size = px(style, 16).min(r.bottom - r.top).max(1);
            let x = r.left + px(style, 13);
            let y = r.top + (r.bottom - r.top - size) / 2;
            let bounds = rect(x, y, x + size, y + size);
            if !icons::from_glyph(glyph)
                .is_some_and(|icon| icons::draw(dc, icon, bounds, true, false, false))
            {
                text(
                    dc,
                    style.icons,
                    &[glyph],
                    bounds,
                    if selected { ACCENT } else { MUTED },
                    DT_CENTER,
                );
            }
        }
        text(
            dc,
            style.font,
            label,
            rect(
                if section && glyph != 0 {
                    r.left + px(style, 48)
                } else {
                    text_left
                },
                r.top,
                r.right - px(style, 13),
                r.bottom,
            ),
            if section { MUTED } else { INK },
            DT_END_ELLIPSIS,
        );
        if !section && item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0 {
            DrawFocusRect(
                dc,
                &rect(
                    body.left + px(style, 2),
                    body.top + px(style, 2),
                    body.right - px(style, 2),
                    body.bottom - px(style, 2),
                ),
            );
        }
        RestoreDC(dc, saved);
    }
}

unsafe fn invalidate_sidebar_row(hwnd: HWND, index: i32) {
    if index < 0 {
        return;
    }
    let mut area: RECT = zeroed();
    if SendMessageW(
        hwnd,
        LB_GETITEMRECT,
        index as WPARAM,
        &mut area as *mut RECT as LPARAM,
    ) != LB_ERR as LRESULT
    {
        InvalidateRect(hwnd, &area, 0);
    }
}

unsafe extern "system" fn control_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    std::panic::catch_unwind(|| unsafe { control_message(hwnd, msg, w, l) }).unwrap_or(0)
}
unsafe fn control_message(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = state(hwnd);
    if ptr.is_null() {
        return DefSubclassProc(hwnd, msg, w, l);
    }
    match msg {
        WM_MOUSEMOVE => {
            // LB_ITEMFROMPOINT can synchronously send owner-draw callbacks. No
            // userdata reference spans that send or the default subclass call.
            let sidebar = matches!((*ptr).kind, Kind::Sidebar { .. });
            let index = if sidebar {
                SendMessageW(hwnd, LB_ITEMFROMPOINT, 0, l) as u32
            } else {
                u32::MAX
            };
            let ptr = state(hwnd);
            if ptr.is_null() {
                return DefSubclassProc(hwnd, msg, w, l);
            }
            let mut rows = None;
            let mut changed = false;
            match &mut (*ptr).kind {
                Kind::Button { hot, .. } => {
                    changed = !*hot;
                    *hot = true;
                }
                Kind::Sidebar { hover } => {
                    let next = if index >> 16 == 0 {
                        (index & 0xffff) as i32
                    } else {
                        -1
                    };
                    if *hover != next {
                        rows = Some((*hover, next));
                    }
                    changed = *hover != next;
                    *hover = next;
                }
                _ => {}
            }
            track(hwnd);
            if let Some((old, next)) = rows {
                invalidate_sidebar_row(hwnd, old);
                invalidate_sidebar_row(hwnd, next);
            } else if changed {
                InvalidateRect(hwnd, null(), 0);
            }
        }
        WM_MOUSELEAVE => {
            let row = match &mut (*ptr).kind {
                Kind::Button { hot, .. } => {
                    *hot = false;
                    None
                }
                Kind::Sidebar { hover } => {
                    let old = *hover;
                    *hover = -1;
                    Some(old)
                }
                _ => None,
            };
            if let Some(old) = row {
                invalidate_sidebar_row(hwnd, old);
            } else {
                InvalidateRect(hwnd, null(), 0);
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_VSCROLL | WM_KEYDOWN | LB_SETTOPINDEX
        | LB_SETCURSEL | LB_SETCARETINDEX | LB_SELECTSTRING | LB_RESETCONTENT => {
            let old = if let Kind::Sidebar { hover } = &mut (*ptr).kind {
                let old = *hover;
                *hover = -1;
                old
            } else {
                -1
            };
            // The listbox scrolls existing pixels and invalidates exposed rows.
            // Do not invalidate its entire client before that operation: only
            // the old hot row needs repainting in addition to native damage.
            invalidate_sidebar_row(hwnd, old);
        }
        WM_ERASEBKGND if matches!((*ptr).kind, Kind::Sidebar { .. }) => {
            let mut r: RECT = zeroed();
            if GetClipBox(w as HDC, &mut r) != NULLREGION {
                fill(w as HDC, r, SIDE);
            }
            // This erase and the following owner draw share the compositor's
            // off-screen surface, including the empty area below the last row.
            return 1;
        }
        WM_ENABLE | WM_SETFOCUS | WM_KILLFOCUS => {
            InvalidateRect(hwnd, null(), 0);
        }
        WM_NCDESTROY => {
            let button = matches!((*ptr).kind, Kind::Button { .. });
            RemoveWindowSubclass(hwnd, Some(control_proc), SUBCLASS);
            RemovePropW(hwnd, STATE);
            if button {
                let parent = GetParent(hwnd);
                let manager = GetPropW(parent, TOOLTIP) as HWND;
                if !manager.is_null() && IsWindow(manager) != 0 {
                    let info = tool_info(parent, hwnd, null_mut());
                    SendMessageW(manager, TTM_DELTOOLW, 0, &info as *const _ as LPARAM);
                }
            }
            drop(Box::from_raw(ptr));
            return DefSubclassProc(hwnd, msg, w, l);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, w, l)
}
