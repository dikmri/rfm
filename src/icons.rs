//! UI-thread-owned chrome icons. Embedded themes never parse SVG at runtime.
#![allow(unsafe_op_in_unsafe_fn)]

use std::{
    cell::RefCell,
    io,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Controls::*, HiDpi::OpenThemeDataForDpi, Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

use crate::settings::IconTheme;

// Discriminants are resource offsets: keep this order aligned with rfm.rc and sources.json.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Icon {
    Back,
    Forward,
    Up,
    Refresh,
    History,
    Desktop,
    Downloads,
    Documents,
    Pictures,
    Music,
    Videos,
    Folder,
    Computer,
    Search,
    NewFolder,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    Sort,
    More,
}

const COUNT: usize = Icon::More as usize + 1;
const NAV_PARTS: [i32; 3] = [NAV_BACKBUTTON, NAV_FORWARDBUTTON, NAV_MENUBUTTON];
const GLYPHS: [u16; COUNT] = [
    0xE72B, 0xE72A, 0xE74A, 0xE72C, 0xE70D, 0xE7F4, 0xE896, 0xE8A5, 0xEB9F, 0xE8D6, 0xE714, 0xE8B7,
    0xE7F8, 0xE721, 0xE8F4, 0xE8C6, 0xE8C8, 0xE77F, 0xE8AC, 0xE74D, 0xE8CB, 0xE712,
];

struct Cache {
    theme: IconTheme,
    size: i32,
    navigation: HTHEME,
    navigation_parts: [bool; 3],
    font: HFONT,
    navigation_font: HFONT,
    // The second half holds precomputed pale disabled images.
    images: [HICON; COUNT * 2],
}

impl Drop for Cache {
    fn drop(&mut self) {
        unsafe {
            for &image in &self.images {
                if !image.is_null() {
                    DestroyIcon(image);
                }
            }
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            if !self.navigation_font.is_null() {
                DeleteObject(self.navigation_font);
            }
            if self.navigation != 0 {
                CloseThemeData(self.navigation);
            }
        }
    }
}

thread_local! {
    static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
}

fn scaled(logical: i32, dpi: u32) -> i32 {
    ((logical as i64 * dpi.max(96) as i64 + 48) / 96).clamp(1, i32::MAX as i64) as i32
}

unsafe fn font(height: i32) -> io::Result<HFONT> {
    let result = CreateFontW(
        -height,
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
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        w!("Segoe MDL2 Assets"),
    );
    if result.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

unsafe fn stock_icon(id: SHSTOCKICONID, size: i32) -> io::Result<HICON> {
    let mut info: SHSTOCKICONINFO = zeroed();
    info.cbSize = size_of::<SHSTOCKICONINFO>() as u32;
    let flags = SHGSI_ICON
        | if size <= 16 {
            SHGSI_SMALLICON
        } else {
            SHGSI_LARGEICON
        };
    let hr = SHGetStockIconInfo(id, flags, &mut info);
    if hr < 0 {
        return Err(io::Error::other(format!(
            "SHGetStockIconInfo: 0x{:08X}",
            hr as u32
        )));
    }
    let image = CopyImage(info.hIcon, IMAGE_ICON, size, size, 0) as HICON;
    DestroyIcon(info.hIcon);
    if image.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(image)
    }
}

// Prepare pale premultiplied BGRA stock icons, including legacy mask-only icons.
unsafe fn pale_icon(image: HICON, size: i32) -> io::Result<HICON> {
    let dc = CreateCompatibleDC(null_mut());
    if dc.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut info: BITMAPINFO = zeroed();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = size;
    info.bmiHeader.biHeight = -size;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut pixels = null_mut();
    let color = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
    if color.is_null() {
        DeleteDC(dc);
        return Err(io::Error::last_os_error());
    }
    let length = size as usize * size as usize;
    std::ptr::write_bytes(pixels as *mut u32, 0, length);
    let old = SelectObject(dc, color);
    let mut drawn = DrawIconEx(dc, 0, 0, image, size, size, 0, null_mut(), DI_NORMAL);
    GdiFlush();
    let has_alpha = std::slice::from_raw_parts(pixels as *const u32, length)
        .iter()
        .any(|pixel| pixel >> 24 != 0);
    if drawn != 0 && !has_alpha {
        // A legacy icon's AND mask is white for transparent and black for opaque.
        drawn = DrawIconEx(dc, 0, 0, image, size, size, 0, null_mut(), DI_MASK);
        GdiFlush();
    }
    SelectObject(dc, old);
    if drawn == 0 {
        DeleteDC(dc);
        DeleteObject(color);
        return Err(io::Error::last_os_error());
    }
    for pixel in std::slice::from_raw_parts_mut(pixels as *mut u32, length) {
        let alpha = if has_alpha {
            (*pixel >> 24) * 3 / 5
        } else if *pixel & 0x00ff_ffff == 0 {
            153
        } else {
            0
        };
        let gray = alpha * 160 / 255;
        *pixel = (alpha << 24) | (gray << 16) | (gray << 8) | gray;
    }
    let mask = CreateBitmap(size, size, 1, 1, null());
    if mask.is_null() {
        DeleteDC(dc);
        DeleteObject(color);
        return Err(io::Error::last_os_error());
    }
    let old = SelectObject(dc, mask);
    let cleared = PatBlt(dc, 0, 0, size, size, BLACKNESS);
    SelectObject(dc, old);
    DeleteDC(dc);
    if cleared == 0 {
        DeleteObject(mask);
        DeleteObject(color);
        return Err(io::Error::last_os_error());
    }
    let icon_info = ICONINFO {
        fIcon: 1,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let result = CreateIconIndirect(&icon_info);
    DeleteObject(mask);
    DeleteObject(color);
    if result.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Rebuild all resources, including theme handles after WM_THEMECHANGED. On
/// failure the previous complete cache remains active and partial resources drop.
pub fn prepare(owner: HWND, theme: IconTheme, dpi: u32) -> io::Result<()> {
    let mut next = Cache {
        theme,
        size: scaled(16, dpi),
        navigation: 0,
        navigation_parts: [false; 3],
        font: null_mut(),
        navigation_font: null_mut(),
        images: [null_mut(); COUNT * 2],
    };
    unsafe {
        if theme == IconTheme::Windows {
            next.navigation = OpenThemeDataForDpi(owner, w!("Navigation"), dpi.max(96));
            if next.navigation != 0 {
                for (index, part) in NAV_PARTS.iter().enumerate() {
                    next.navigation_parts[index] =
                        IsThemePartDefined(next.navigation, *part, 1) != 0;
                }
            }
            next.font = font(scaled(16, dpi))?;
            next.navigation_font = font(scaled(12, dpi))?;
            // Known-folder sidebar images are owned by the system image list in chrome.
            for (icon, stock) in [
                (Icon::Folder, SIID_FOLDER),
                (Icon::Computer, SIID_DESKTOPPC),
            ] {
                let index = icon as usize;
                next.images[index] = stock_icon(stock, next.size)?;
                next.images[index + COUNT] = pale_icon(next.images[index], next.size)?;
            }
        } else {
            let module = GetModuleHandleW(null());
            if module.is_null() {
                return Err(io::Error::last_os_error());
            }
            // Normal groups: Lucide 1000..1021, Tabler 2000..2021; disabled +100.
            let base = if theme == IconTheme::Lucide {
                1000
            } else {
                2000
            };
            for disabled in 0..2 {
                for index in 0..COUNT {
                    let id = base + disabled * 100 + index;
                    let image = LoadImageW(
                        module,
                        id as *const u16,
                        IMAGE_ICON,
                        next.size,
                        next.size,
                        0,
                    ) as HICON;
                    if image.is_null() {
                        return Err(io::Error::other(format!(
                            "LoadImageW icon resource {id}: {}",
                            io::Error::last_os_error()
                        )));
                    }
                    next.images[disabled * COUNT + index] = image;
                }
            }
        }
    }
    CACHE.with(|cache| *cache.borrow_mut() = Some(next));
    Ok(())
}

/// Draw using only prepared handles. False means that the caller must use its
/// existing rendering path (unprepared cache or a genuine native drawing error).
pub fn draw(dc: HDC, icon: Icon, bounds: RECT, enabled: bool, hot: bool, pressed: bool) -> bool {
    CACHE.with(|cache| {
        let cache = cache.borrow();
        let Some(cache) = cache.as_ref() else {
            return false;
        };
        let size = cache
            .size
            .min(bounds.right - bounds.left)
            .min(bounds.bottom - bounds.top);
        if size <= 0 || dc.is_null() {
            return false;
        }
        let area = RECT {
            left: bounds.left + (bounds.right - bounds.left - size) / 2,
            top: bounds.top + (bounds.bottom - bounds.top - size) / 2,
            right: bounds.left + (bounds.right - bounds.left + size) / 2,
            bottom: bounds.top + (bounds.bottom - bounds.top + size) / 2,
        };
        unsafe {
            if cache.theme == IconTheme::Windows {
                let part = match icon {
                    Icon::Back => Some(0),
                    Icon::Forward => Some(1),
                    Icon::History => Some(2),
                    _ => None,
                };
                if let Some(part) = part {
                    let state = if !enabled {
                        4
                    } else if pressed {
                        3
                    } else if hot {
                        2
                    } else {
                        1
                    };
                    if cache.navigation_parts[part]
                        && DrawThemeBackground(
                            cache.navigation,
                            dc,
                            NAV_PARTS[part],
                            state,
                            &area,
                            null(),
                        ) >= 0
                    {
                        return true;
                    }
                }
            }
            let image = cache.images[icon as usize + if enabled { 0 } else { COUNT }];
            if !image.is_null() {
                return DrawIconEx(
                    dc,
                    area.left,
                    area.top,
                    image,
                    size,
                    size,
                    0,
                    null_mut(),
                    DI_NORMAL,
                ) != 0;
            }
            if cache.theme != IconTheme::Windows {
                return false;
            }
            let navigation = matches!(
                icon,
                Icon::Back | Icon::Forward | Icon::Up | Icon::Refresh | Icon::History
            );
            let font = if navigation {
                cache.navigation_font
            } else {
                cache.font
            };
            let previous_font = SelectObject(dc, font);
            let previous_mode = SetBkMode(dc, TRANSPARENT as i32);
            let color = if !enabled {
                0x00b8_b8b8
            } else if hot || pressed {
                0x0060_6060
            } else {
                0x0070_7070
            };
            let previous_color = SetTextColor(dc, color);
            let mut text_area = area;
            let result = DrawTextW(
                dc,
                [GLYPHS[icon as usize]].as_ptr(),
                1,
                &mut text_area,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            ) != 0;
            SetTextColor(dc, previous_color);
            SetBkMode(dc, previous_mode);
            SelectObject(dc, previous_font);
            result
        }
    })
}

pub fn from_glyph(glyph: u16) -> Option<Icon> {
    Some(match glyph {
        0xE72B => Icon::Back,
        0xE72A => Icon::Forward,
        0xE74A => Icon::Up,
        0xE72C => Icon::Refresh,
        0xE70D => Icon::History,
        0xE7F4 => Icon::Desktop,
        0xE896 => Icon::Downloads,
        0xE8A5 => Icon::Documents,
        0xEB9F => Icon::Pictures,
        0xE8D6 => Icon::Music,
        0xE714 => Icon::Videos,
        0xE8B7 => Icon::Folder,
        0xE7F8 => Icon::Computer,
        0xE721 => Icon::Search,
        0xE710 => Icon::NewFolder,
        0xE8C6 => Icon::Cut,
        0xE8C8 => Icon::Copy,
        0xE77F => Icon::Paste,
        0xE8AC => Icon::Rename,
        0xE74D => Icon::Delete,
        0xE8CB => Icon::Sort,
        0xE712 => Icon::More,
        _ => return None,
    })
}

pub fn shutdown() {
    CACHE.with(|cache| *cache.borrow_mut() = None);
}
