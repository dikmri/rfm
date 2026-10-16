//! Native modal progress keeps file operations unavailable during an approved update.
use crate::{
    update_install::{Pending, Restart, Stage},
    update_net::{self, Release},
};
use std::{
    cell::{Cell, RefCell},
    io,
    mem::size_of,
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    UI::{Controls::*, WindowsAndMessaging::*},
};

static WORKING: AtomicBool = AtomicBool::new(false);

pub fn is_working() -> bool {
    WORKING.load(Ordering::Acquire)
}

struct WorkGuard;
impl Drop for WorkGuard {
    fn drop(&mut self) {
        WORKING.store(false, Ordering::Release);
    }
}

struct Progress {
    receiver: Receiver<io::Result<Pending>>,
    cancel: Arc<AtomicBool>,
    result: RefCell<Option<io::Result<Pending>>>,
    automatic_close: Cell<bool>,
}

unsafe extern "system" fn progress_callback(
    hwnd: HWND,
    notification: u32,
    button: WPARAM,
    _: LPARAM,
    data: isize,
) -> i32 {
    // No RefCell borrow spans a synchronous task-dialog message: it can invoke
    // this callback again before SendMessageW returns.
    let progress = unsafe { &*(data as *const Progress) };
    match notification as i32 {
        TDN_CREATED => unsafe {
            SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE as u32, 1, 40);
        },
        TDN_TIMER => {
            let result = match progress.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(io::Error::other(tr!(
                    "更新処理が中断されました。現在のrfmは変更されていません。",
                    "The update worker stopped. Your installed rfm has not been changed."
                )))),
            };
            if let Some(result) = result {
                *progress.result.borrow_mut() = Some(result);
                progress.automatic_close.set(true);
                unsafe {
                    SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as usize, 0);
                }
                progress.automatic_close.set(false);
            }
        }
        TDN_BUTTON_CLICKED if button as i32 == IDCANCEL && !progress.automatic_close.get() => {
            progress.cancel.store(true, Ordering::Release);
            progress.result.borrow_mut().take();
        }
        _ => {}
    }
    0
}

/// Only call after consent. Cancel closes the dialog immediately; a blocked
/// WinHTTP call finishes under its timeout, without spawning another worker.
pub fn prepare(owner: HWND, release: Release, restart: Restart) -> io::Result<Option<Pending>> {
    if WORKING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(io::Error::other(tr!(
            "前回の更新処理が終了するまでお待ちください。",
            "Wait for the previous update operation to finish."
        )));
    }
    let guard = WorkGuard;
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let (sender, receiver) = mpsc::channel();
    let title: Vec<u16> = tr_format!(
        "rfm {} に更新しています",
        "Updating to rfm {}",
        release.version
    )
    .encode_utf16()
    .chain(Some(0))
    .collect();
    std::thread::Builder::new()
        .name("rfm-update".into())
        .spawn(move || {
            let _guard = guard;
            let result = (|| {
                let stage = Stage::new(restart)?;
                let expected =
                    update_net::download(&release, stage.candidate_path(), &worker_cancel)?;
                if worker_cancel.load(Ordering::Acquire) {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                stage.prepare(expected)
            })();
            // A cancelled dialog drops its receiver. Dropping an undelivered Pending
            // aborts the helper; merely finishing a download never commits an update.
            let _ = sender.send(result);
        })?;
    let progress = Progress {
        receiver,
        cancel,
        result: RefCell::new(None),
        automatic_close: Cell::new(false),
    };
    let buttons = [TASKDIALOG_BUTTON {
        nButtonID: IDCANCEL,
        pszButtonText: tr_w!("キャンセル", "Cancel"),
    }];
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_SHOW_MARQUEE_PROGRESS_BAR
            | TDF_CALLBACK_TIMER
            | TDF_ALLOW_DIALOG_CANCELLATION
            | TDF_POSITION_RELATIVE_TO_WINDOW,
        pszWindowTitle: tr_w!("rfm — 更新", "rfm — Update"),
        pszMainInstruction: title.as_ptr(),
        pszContent: tr_w!(
            "公式GitHubリリースをダウンロードし、サイズとSHA-256を検証しています。\n完了後にrfmを再起動し、開いているフォルダーを復元します。",
            "Downloading the official GitHub release and verifying its size and SHA-256.\nrfm will restart and restore your open folders when ready."
        ),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: IDCANCEL,
        pfCallback: Some(progress_callback),
        lpCallbackData: &progress as *const Progress as isize,
        cxWidth: 320,
        ..Default::default()
    };
    let result = unsafe { TaskDialogIndirect(&config, null_mut(), null_mut(), null_mut()) };
    progress.cancel.store(true, Ordering::Release);
    if result < 0 {
        return Err(io::Error::other(tr_format!(
            "更新画面を開けませんでした (0x{:08X})。現在のrfmは変更されていません。",
            "Could not open the update dialog (0x{:08X}). Your installed rfm has not been changed.",
            result as u32
        )));
    }
    match progress.result.into_inner() {
        Some(Ok(pending)) => Ok(Some(pending)),
        Some(Err(error)) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Some(Err(error)) => Err(error),
        None => Ok(None),
    }
}
