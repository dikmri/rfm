#![windows_subsystem = "windows"]
use std::{
    cell::RefCell,
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    ptr::{null, null_mut},
};
use windows::{
    Win32::{
        Foundation::{HWND as WHWND, RECT as WRECT},
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            Ole::{OleInitialize, OleUninitialize},
        },
        UI::Shell::*,
    },
    core::PCWSTR,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Controls::*, HiDpi::*, WindowsAndMessaging::*},
};
thread_local! {static BROWSER: RefCell<Option<IExplorerBrowser>> = const { RefCell::new(None) };}
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_CLOSE {
            let browser = BROWSER.with(|slot| slot.borrow_mut().take());
            if let Some(browser) = browser
                && let Err(error) = browser.Destroy()
            {
                PostQuitMessage(error.code().0);
                return 0;
            }
            DestroyWindow(hwnd);
            return 0;
        }
        if msg == WM_SIZE {
            let browser = BROWSER.with(|v| v.borrow().clone());
            if let Some(b) = browser {
                let mut r: RECT = zeroed();
                GetClientRect(hwnd, &mut r);
                let _ = b.SetRect(
                    None,
                    WRECT {
                        left: 0,
                        top: 0,
                        right: r.right,
                        bottom: r.bottom,
                    },
                );
            }
            return 0;
        }
        if msg == WM_APP + 501 {
            let browser = BROWSER.with(|v| v.borrow().clone());
            return browser
                .and_then(|b| b.GetCurrentView::<IFolderView>().ok())
                .and_then(|v| v.ItemCount(SVGIO_ALLVIEW).ok())
                .map_or(-1, |count| count as isize);
        }
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}
fn main() -> windows::core::Result<()> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(None)?;
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_LISTVIEW_CLASSES | ICC_TREEVIEW_CLASSES,
        });
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: windows_sys::core::w!("rfm.shell.probe"),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
            ..zeroed()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            0,
            class.lpszClassName,
            windows_sys::core::w!("rfm — native Shell probe"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            30,
            30,
            1100,
            670,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        let browser: IExplorerBrowser =
            CoCreateInstance(&ExplorerBrowser, None, CLSCTX_INPROC_SERVER)?;
        let rect = WRECT {
            left: 0,
            top: 0,
            right: 1084,
            bottom: 631,
        };
        let settings = FOLDERSETTINGS {
            ViewMode: FVM_DETAILS.0 as u32,
            fFlags: FWF_AUTOARRANGE.0 as u32,
        };
        browser.Initialize(WHWND(hwnd), &rect, Some(&settings))?;
        browser.SetOptions(EBO_SHOWFRAMES)?;
        browser.SetPropertyBag(windows::core::w!("rfm.probe"))?;
        let path = std::env::args_os()
            .nth(1)
            .unwrap_or_else(|| "D:\\soft\\rfm".into());
        let text: Vec<u16> = path.encode_wide().chain(Some(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(text.as_ptr()), None)?;
        browser.BrowseToObject(&item, SBSP_ABSOLUTE)?;
        BROWSER.with(|v| *v.borrow_mut() = Some(browser.clone()));
        ShowWindow(hwnd, SW_SHOW);
        let mut message: MSG = zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        BROWSER.with(|v| v.borrow_mut().take());
        drop(browser);
        OleUninitialize();
        if message.wParam != 0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                message.wParam as i32,
            )));
        }
        Ok(())
    }
}
