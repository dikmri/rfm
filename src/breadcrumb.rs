use crate::{
    chrome::Style,
    icons::{self, Icon},
};
use std::{
    cell::RefCell,
    ffi::OsString,
    io,
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::*,
        Input::KeyboardAndMouse::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

pub const NAVIGATE: u32 = WM_APP + 30;
const CLASS: &[u16] = &[
    82, 102, 109, 69, 120, 112, 108, 111, 114, 101, 114, 65, 100, 100, 114, 101, 115, 115, 0,
];
const EDIT_CLASS: &[u16] = &[69, 68, 73, 84, 0];
const WHITE: u32 = 0x00ff_ffff;
const TEXT: u32 = 0x0020_2020;
const BORDER: u32 = 0x00a0_a0a0;
const HOVER: u32 = 0x00e8_e8e8;
const MUTED: u32 = 0x0069_6969;
const ELLIPSIS: [u16; 1] = [0x2026];
const REFRESH_SUBCLASS: usize = 0x5246_4d41;

struct Segment {
    path: PathBuf,
    label: Vec<u16>,
    width: i32,
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Segment(usize),
    Ellipsis,
}

struct Hit {
    target: Target,
    rect: RECT,
}

struct State {
    style: Style,
    edit: HWND,
    refresh: HWND,
    edit_id: usize,
    editing: bool,
    path: PathBuf,
    text: Vec<u16>,
    segments: Vec<Segment>,
    hits: Vec<Hit>,
    hover: Option<Target>,
    navigation: Option<PathBuf>,
}

type Cell = RefCell<State>;

fn px(style: Style, value: i32) -> i32 {
    ((value as i64 * style.dpi.max(1) as i64 + 48) / 96) as i32
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

unsafe fn cell(hwnd: HWND) -> Option<&'static Cell> {
    unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Cell).as_ref() }
}

/// Creates a focusable address box and its native, initially hidden path editor.
pub fn create(parent: HWND, id: usize, edit_id: usize, style: Style) -> HWND {
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: CLASS.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&class);
        let args = (style, edit_id);
        CreateWindowExW(
            0,
            CLASS.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_CLIPCHILDREN,
            0,
            0,
            0,
            px(style, 24),
            parent,
            id as HMENU,
            instance,
            &args as *const _ as *const _,
        )
    }
}

pub fn edit(hwnd: HWND) -> HWND {
    unsafe {
        cell(hwnd)
            .and_then(|cell| cell.try_borrow().ok().map(|state| state.edit))
            .unwrap_or(null_mut())
    }
}

/// Moves the existing owner-drawn refresh button inside the address border.
pub fn attach_refresh(address: HWND, button: HWND) -> io::Result<()> {
    unsafe {
        let existing = cell(address)
            .and_then(|cell| cell.try_borrow().ok().map(|state| state.refresh))
            .ok_or_else(|| {
                io::Error::other(tr!(
                    "アドレス コントロールを利用できません",
                    "Address control is unavailable"
                ))
            })?;
        if button.is_null() || IsWindow(button) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!(
                    "更新ボタンを利用できません",
                    "Refresh button is unavailable"
                ),
            ));
        }
        if !existing.is_null() && existing != button {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                tr!(
                    "アドレス コントロールには既に更新ボタンがあります",
                    "Address control already has a refresh button"
                ),
            ));
        }
        if SetWindowSubclass(
            button,
            Some(refresh_proc),
            REFRESH_SUBCLASS,
            address as usize,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        // Keep the existing tooltip's HWND-based registration and button state.
        // Reparenting can synchronously change focus: do not hold the State borrow.
        SetLastError(0);
        if SetParent(button, address).is_null() && GetLastError() != 0 {
            let error = io::Error::last_os_error();
            RemoveWindowSubclass(button, Some(refresh_proc), REFRESH_SUBCLASS);
            return Err(error);
        }
        let Some(cell) = cell(address) else {
            return Err(io::Error::other(tr!(
                "アドレス コントロールは破棄されました",
                "Address control was destroyed"
            )));
        };
        cell.borrow_mut().refresh = button;
        layout(address, false);
        InvalidateRect(address, null(), 0);
        Ok(())
    }
}

pub fn set_path(hwnd: HWND, path: &Path) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(state) = cell.try_borrow() else { return };
        // Listing and selection refreshes must not reset an active native edit.
        if state.path == path {
            return;
        }
    }
    let mut segments: Vec<Segment> = Vec::new();
    let mut cumulative = PathBuf::new();
    for component in path.components() {
        cumulative.push(component.as_os_str());
        if component == Component::RootDir && !segments.is_empty() {
            segments[0].path = cumulative.clone();
            continue;
        }
        let label = match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    OsString::from(format!("{}:", letter as char))
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    let mut label = OsString::from("\\\\");
                    label.push(server);
                    label.push("\\");
                    label.push(share);
                    label
                }
                _ => component.as_os_str().to_os_string(),
            },
            _ => component.as_os_str().to_os_string(),
        };
        segments.push(Segment {
            path: cumulative.clone(),
            label: label.encode_wide().collect(),
            width: 0,
        });
    }
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        state.path = path.to_path_buf();
        state.text = wide(path.as_os_str());
        state.segments = segments;
        state.hover = None;
        let caption = state.text.as_ptr();
        drop(state);
        // WM_SETTEXT is handled by DefWindowProc and never changes the cached path.
        // Expose the real path to native accessibility and window-text consumers.
        SetWindowTextW(hwnd, caption);
        end_edit(hwnd);
        layout(hwnd, true);
        InvalidateRect(hwnd, null(), 0);
    }
}

pub fn take_navigation(hwnd: HWND) -> Option<PathBuf> {
    unsafe { cell(hwnd)?.try_borrow_mut().ok()?.navigation.take() }
}

pub fn begin_edit(hwnd: HWND) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        if state.edit.is_null() {
            return;
        }
        let edit = state.edit;
        let text = if state.editing {
            None
        } else {
            Some(state.text.clone())
        };
        state.editing = true;
        state.hover = None;
        drop(state);
        if let Some(text) = text {
            SetWindowTextW(edit, text.as_ptr());
        }
        ShowWindow(edit, SW_SHOW);
        SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, 0, -1);
        InvalidateRect(hwnd, null(), 0);
    }
}

pub fn end_edit(hwnd: HWND) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        if !state.editing {
            return;
        }
        state.editing = false;
        let edit = state.edit;
        drop(state);
        ShowWindow(edit, SW_HIDE);
        InvalidateRect(hwnd, null(), 0);
    }
}

pub fn set_style(hwnd: HWND, style: Style) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        state.style = style;
        let edit = state.edit;
        drop(state);
        SendMessageW(edit, WM_SETFONT, style.font as usize, 1);
        layout(hwnd, true);
        InvalidateRect(hwnd, null(), 0);
    }
}

// Layout and text measurements are cached here, never performed during painting.
unsafe fn layout(hwnd: HWND, measure: bool) {
    unsafe {
        let Some(cell) = cell(hwnd) else { return };
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        let style = state.style;
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &mut client);
        if measure {
            let dc = GetDC(hwnd);
            if !dc.is_null() {
                let old = SelectObject(dc, style.font);
                for segment in &mut state.segments {
                    let mut size: SIZE = zeroed();
                    GetTextExtentPoint32W(
                        dc,
                        segment.label.as_ptr(),
                        segment.label.len() as i32,
                        &mut size,
                    );
                    segment.width = size.cx + px(style, 8);
                }
                SelectObject(dc, old);
                ReleaseDC(hwnd, dc);
            }
        }
        state.hits.clear();
        state.hover = None;
        let refresh = state.refresh;
        let refresh_width = if refresh.is_null() {
            0
        } else {
            px(style, 22).min((client.right - 2).max(0))
        };
        let refresh_left = (client.right - 1 - refresh_width).max(1);
        let content_right = if refresh.is_null() {
            (client.right - 1).max(1)
        } else {
            refresh_left
        };
        let left = px(style, 24);
        let right = (content_right - px(style, 2)).max(left);
        let available = right - left;
        let gap = px(style, 8);
        let count = state.segments.len();
        let total: i32 = state
            .segments
            .iter()
            .map(|segment| segment.width)
            .sum::<i32>()
            + gap * count.saturating_sub(1) as i32;
        let top = px(style, 2);
        let bottom = (client.bottom - px(style, 2)).max(top);
        let mut x = left;
        let mut add = |state: &mut State, target: Target, width: i32| {
            let width = width.max(0).min((right - x).max(0));
            if width > 0 {
                state.hits.push(Hit {
                    target,
                    rect: RECT {
                        left: x,
                        top,
                        right: x + width,
                        bottom,
                    },
                });
            }
            x += width + gap;
        };
        if count > 0 && available > 0 {
            if total <= available || count == 1 {
                for index in 0..count {
                    let width = state.segments[index].width.min(available);
                    add(&mut state, Target::Segment(index), width);
                }
            } else if count == 2 {
                let root = state.segments[0].width.min((available - gap).max(0) / 3);
                add(&mut state, Target::Segment(0), root);
                add(
                    &mut state,
                    Target::Segment(1),
                    (available - root - gap).max(0),
                );
            } else if available < px(style, 90) {
                // At exceptionally narrow sizes the root remains a real target;
                // the remaining blank region still opens the complete editor.
                add(&mut state, Target::Segment(0), available);
            } else {
                let root = state.segments[0].width.min(available / 3);
                let ellipsis = px(style, 25);
                let tail_space = (available - root - ellipsis - 2 * gap).max(0);
                let mut start = count - 1;
                let mut tail_width = state.segments[start].width.min(tail_space);
                while start > 1 {
                    let width = state.segments[start - 1].width + gap;
                    if tail_width + width > tail_space {
                        break;
                    }
                    start -= 1;
                    tail_width += width;
                }
                add(&mut state, Target::Segment(0), root);
                if start > 1 {
                    add(&mut state, Target::Ellipsis, ellipsis);
                }
                for index in start..count {
                    let width = if index == count - 1 {
                        state.segments[index].width.min(tail_space)
                    } else {
                        state.segments[index].width
                    };
                    add(&mut state, Target::Segment(index), width);
                }
            }
        }
        let edit = state.edit;
        let edit_left = px(style, 24).min(content_right);
        let height = px(style, 20).min((client.bottom - 2).max(0));
        let y = ((client.bottom - height) / 2).max(1);
        let width = (content_right - edit_left - px(style, 2)).max(0);
        drop(state);
        MoveWindow(edit, edit_left, y, width, height, 1);
        if !refresh.is_null() {
            MoveWindow(
                refresh,
                refresh_left,
                1,
                refresh_width,
                (client.bottom - 2).max(0),
                1,
            );
        }
    }
}

fn contains(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

unsafe fn paint(hwnd: HWND) {
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
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &mut client);
        let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let old_pen = SelectObject(dc, GetStockObject(DC_PEN));
        SetDCBrushColor(dc, WHITE);
        FillRect(dc, &client, GetStockObject(DC_BRUSH));
        SetDCPenColor(dc, BORDER);
        Rectangle(dc, 0, 0, client.right, client.bottom);
        let old_font = SelectObject(dc, state.style.font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, MUTED);
        let icon_top = ((client.bottom - px(state.style, 16)) / 2).max(1);
        let icon = RECT {
            left: px(state.style, 4),
            top: icon_top,
            right: px(state.style, 20),
            bottom: icon_top + px(state.style, 16),
        };
        icons::draw(dc, Icon::Folder, icon, true, false, false);
        if !state.editing {
            for (index, hit) in state.hits.iter().enumerate() {
                if state.hover == Some(hit.target) {
                    SetDCBrushColor(dc, HOVER);
                    SetDCPenColor(dc, HOVER);
                    RoundRect(
                        dc,
                        hit.rect.left,
                        hit.rect.top,
                        hit.rect.right,
                        hit.rect.bottom,
                        px(state.style, 5),
                        px(state.style, 5),
                    );
                }
                let mut text = hit.rect;
                text.left += px(state.style, 4);
                text.right -= px(state.style, 4);
                let label = match hit.target {
                    Target::Segment(segment) => &state.segments[segment].label[..],
                    Target::Ellipsis => &ELLIPSIS,
                };
                SetTextColor(dc, TEXT);
                if text.right > text.left {
                    DrawTextW(
                        dc,
                        label.as_ptr(),
                        label.len() as i32,
                        &mut text,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
                if index + 1 < state.hits.len() {
                    SetDCPenColor(dc, MUTED);
                    let x = hit.rect.right + px(state.style, 4);
                    let y = client.bottom / 2;
                    MoveToEx(dc, x, y - px(state.style, 3), null_mut());
                    LineTo(dc, x + px(state.style, 3), y);
                    LineTo(dc, x, y + px(state.style, 3));
                }
            }
        }
        SelectObject(dc, old_font);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        EndPaint(hwnd, &ps);
    }
}

// The address remains one tab stop in main's native dialog navigation. Route
// its two nested controls explicitly, without exposing a hidden edit as a stop.
unsafe fn move_tab_focus(address: HWND, from_refresh: bool) {
    unsafe {
        let refresh = cell(address)
            .and_then(|cell| cell.try_borrow().ok().map(|state| state.refresh))
            .unwrap_or(null_mut());
        let backwards = GetKeyState(VK_SHIFT as i32) < 0;
        if from_refresh && backwards {
            begin_edit(address);
        } else if !from_refresh
            && !backwards
            && !refresh.is_null()
            && IsWindowEnabled(refresh) != 0
            && IsWindowVisible(refresh) != 0
        {
            SetFocus(refresh);
        } else {
            let next = GetNextDlgTabItem(GetParent(address), address, backwards as i32);
            if !next.is_null() {
                SetFocus(next);
            }
        }
    }
}

unsafe extern "system" fn refresh_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    address: usize,
) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        if wparam == VK_TAB as usize
            && GetKeyState(VK_CONTROL as i32) >= 0
            && GetKeyState(VK_MENU as i32) >= 0
        {
            if message == WM_GETDLGCODE {
                return DefSubclassProc(hwnd, message, wparam, lparam) | DLGC_WANTTAB as isize;
            }
            if message == WM_KEYDOWN {
                move_tab_focus(address as HWND, true);
                return 0;
            }
        }
        if message == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(refresh_proc), REFRESH_SUBCLASS);
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }))
    .unwrap_or(0)
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    parent: usize,
) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        if wparam == VK_TAB as usize
            && GetKeyState(VK_CONTROL as i32) >= 0
            && GetKeyState(VK_MENU as i32) >= 0
        {
            if message == WM_GETDLGCODE {
                return DefSubclassProc(hwnd, message, wparam, lparam) | DLGC_WANTTAB as isize;
            }
            if message == WM_KEYDOWN {
                move_tab_focus(parent as HWND, false);
                return 0;
            }
        }
        if message == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(edit_proc), 1);
        } else if message == WM_KILLFOCUS {
            end_edit(parent as HWND);
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }))
    .unwrap_or(0)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        handle_message(hwnd, message, wparam, lparam)
    }))
    .unwrap_or(0)
}

unsafe fn handle_message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_NCCREATE => {
                let create = &*(lparam as *const CREATESTRUCTW);
                let &(style, edit_id) = &*(create.lpCreateParams as *const (Style, usize));
                let state = State {
                    style,
                    edit: null_mut(),
                    refresh: null_mut(),
                    edit_id,
                    editing: false,
                    path: PathBuf::new(),
                    text: vec![0],
                    segments: Vec::new(),
                    hits: Vec::new(),
                    hover: None,
                    navigation: None,
                };
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_USERDATA,
                    Box::into_raw(Box::new(RefCell::new(state))) as isize,
                );
                return 1;
            }
            WM_CREATE => {
                let (style, id) = {
                    let Some(cell) = cell(hwnd) else { return -1 };
                    let state = cell.borrow();
                    (state.style, state.edit_id)
                };
                let editor = CreateWindowExW(
                    0,
                    EDIT_CLASS.as_ptr(),
                    null(),
                    WS_CHILD | ES_AUTOHSCROLL as u32,
                    0,
                    0,
                    0,
                    0,
                    hwnd,
                    id as HMENU,
                    GetModuleHandleW(null()),
                    null(),
                );
                if editor.is_null() {
                    return -1;
                }
                if let Some(cell) = cell(hwnd) {
                    cell.borrow_mut().edit = editor;
                }
                if SetWindowSubclass(editor, Some(edit_proc), 1, hwnd as usize) == 0 {
                    return -1;
                }
                SendMessageW(editor, WM_SETFONT, style.font as usize, 0);
                SendMessageW(
                    editor,
                    EM_SETMARGINS,
                    (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
                    0,
                );
                layout(hwnd, true);
                return 0;
            }
            WM_SIZE => {
                layout(hwnd, false);
                InvalidateRect(hwnd, null(), 0);
                return 0;
            }
            WM_PAINT => {
                paint(hwnd);
                return 0;
            }
            WM_ERASEBKGND => return 1,
            WM_COMMAND | WM_DRAWITEM => {
                // Native button drawing and command handling remain owned by main.
                // In particular, never forward while holding a State borrow.
                return SendMessageW(GetParent(hwnd), message, wparam, lparam);
            }
            WM_CTLCOLOREDIT => {
                let dc = wparam as HDC;
                SetTextColor(dc, TEXT);
                SetBkColor(dc, WHITE);
                SetDCBrushColor(dc, WHITE);
                return GetStockObject(DC_BRUSH) as isize;
            }
            WM_SETFOCUS => {
                begin_edit(hwnd);
                return 0;
            }
            WM_MOUSEMOVE => {
                let x = lparam as u16 as i16 as i32;
                let y = (lparam >> 16) as u16 as i16 as i32;
                let changed = if let Some(cell) = cell(hwnd) {
                    if let Ok(mut state) = cell.try_borrow_mut() {
                        let hover = state
                            .hits
                            .iter()
                            .find(|hit| contains(&hit.rect, x, y))
                            .map(|hit| hit.target);
                        let changed = state.hover != hover;
                        state.hover = hover;
                        changed
                    } else {
                        false
                    }
                } else {
                    false
                };
                let mut tracking = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tracking);
                if changed {
                    InvalidateRect(hwnd, null(), 0);
                }
                return 0;
            }
            WM_MOUSELEAVE => {
                if let Some(cell) = cell(hwnd)
                    && let Ok(mut state) = cell.try_borrow_mut()
                {
                    state.hover = None;
                }
                InvalidateRect(hwnd, null(), 0);
                return 0;
            }
            WM_LBUTTONDOWN => {
                let x = lparam as u16 as i16 as i32;
                let y = (lparam >> 16) as u16 as i16 as i32;
                let navigate = if let Some(cell) = cell(hwnd) {
                    if let Ok(mut state) = cell.try_borrow_mut() {
                        let target = state
                            .hits
                            .iter()
                            .find(|hit| contains(&hit.rect, x, y))
                            .map(|hit| hit.target);
                        if let Some(Target::Segment(index)) = target {
                            state.navigation = Some(state.segments[index].path.clone());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if navigate {
                    PostMessageW(GetParent(hwnd), NAVIGATE, hwnd as usize, 0);
                } else {
                    begin_edit(hwnd);
                }
                return 0;
            }
            WM_SETCURSOR => {
                if lparam as u16 as u32 == HTCLIENT {
                    let over_segment = cell(hwnd)
                        .and_then(|cell| cell.try_borrow().ok().map(|state| state.hover.is_some()))
                        .unwrap_or(false);
                    SetCursor(LoadCursorW(
                        null_mut(),
                        if over_segment { IDC_HAND } else { IDC_IBEAM },
                    ));
                    return 1;
                }
            }
            WM_NCDESTROY => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Cell;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !state.is_null() {
                    drop(Box::from_raw(state));
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
