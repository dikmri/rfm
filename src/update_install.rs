//! Consent-gated, per-user native executable replacement.
//!
//! The parent never renames its executable. A copy of that exact executable holds
//! the operation lease, pins the parent process, and waits for explicit commit.
use std::{
    ffi::{OsStr, OsString, c_void},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf, Prefix},
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, LocalFree,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Globalization::{LCMAP_UPPERCASE, LCMapStringEx, LOCALE_NAME_INVARIANT},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ELEVATION,
        TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileRenameInfo,
        GetFileInformationByHandle, READ_CONTROL, SetFileInformationByHandle,
    },
    System::{
        Com::CoTaskMemFree,
        Threading::{
            CREATE_NO_WINDOW, CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess,
            GetProcessTimes, OpenEventW, OpenMutexW, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
            ResetEvent, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
        },
    },
    UI::{
        Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
        WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
    },
};

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: HANDLE, buffer: *mut u8, length: u32, flags: u32) -> i32;
}

const SCHEMA: u32 = 1;
const MAX_MANIFEST: u64 = 2 * 1024 * 1024;
const MAX_EXE: u64 = 256 * 1024 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const CLEAN_TIMEOUT: Duration = Duration::from_secs(120);
const JOB_FILES: &[&str] = &[
    "candidate.exe",
    "helper.exe",
    "manifest.json",
    "ready.json",
    "launch.json",
    "cleanup.json",
    "replacement.json",
    "error.txt",
];

#[derive(Clone, Debug)]
pub struct Restart {
    pub folders: Vec<PathBuf>,
    pub active: usize,
    pub japanese: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRestart {
    folders: Vec<Vec<u16>>,
    active: usize,
    japanese: bool,
}

impl WireRestart {
    fn from_restart(restart: Restart) -> io::Result<Self> {
        let wire = Self {
            folders: restart
                .folders
                .iter()
                .map(|p| p.as_os_str().encode_wide().collect())
                .collect(),
            active: restart.active,
            japanese: restart.japanese,
        };
        wire.validate()?;
        Ok(wire)
    }

    fn validate(&self) -> io::Result<()> {
        if self.folders.is_empty() || self.folders.len() > 512 || self.active >= self.folders.len()
        {
            return Err(invalid(tr!(
                "再開するタブの情報が不正です。",
                "Invalid restart tab information."
            )));
        }
        let mut total = 0usize;
        for value in &self.folders {
            total = total.saturating_add(value.len());
            validate_absolute(&from_wide(value))?;
        }
        if total > 200_000 {
            return Err(invalid(tr!(
                "再開するタブの情報が大きすぎます。",
                "Restart tab information is too large."
            )));
        }
        Ok(())
    }

    fn restart(&self) -> Restart {
        Restart {
            folders: self.folders.iter().map(|p| from_wide(p)).collect(),
            active: self.active,
            japanese: self.japanese,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    volume: u32,
    high: u32,
    low: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    job: String,
    target: Vec<u16>,
    target_id: Identity,
    directory_id: Identity,
    parent_pid: u32,
    parent_started: u64,
    old_hash: [u8; 32],
    new_hash: [u8; 32],
    restart: WireRestart,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessRecord {
    schema: u32,
    pid: u32,
    started: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    schema: u32,
    expected_hash: [u8; 32],
    rollback: bool,
    reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    schema: u32,
    identity: Identity,
}

struct ResumeState {
    job: PathBuf,
    backup: PathBuf,
    partial: PathBuf,
    failed: PathBuf,
    old_hash: [u8; 32],
    old_id: Identity,
    replacement_id: Option<Identity>,
    helper: OwnedHandle,
    resumed: OwnedHandle,
    directories: Vec<File>,
    target_directory: File,
    reason: Option<String>,
}

static RESUME: Mutex<Option<ResumeState>> = Mutex::new(None);

struct PrivateSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    user: Vec<usize>,
}

impl Drop for PrivateSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

fn current_token() -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    win(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    owned(token)
}

impl PrivateSecurity {
    fn new() -> io::Result<Self> {
        let token = current_token()?;
        let mut elevation = TOKEN_ELEVATION::default();
        let mut needed = 0;
        win(unsafe {
            GetTokenInformation(
                raw(&token),
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut needed,
            )
        })?;
        if elevation.TokenIsElevated != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                tr!(
                    "管理者として実行中は更新できません。通常の権限で起動し直してください。",
                    "Updates are disabled while running elevated. Restart rfm without administrator privileges."
                ),
            ));
        }
        Self::from_token(&token)
    }

    // ACL construction is also used by filesystem tests on elevated CI runners;
    // only new() is an updater entry-point and enforces the elevation policy.
    fn from_token(token: &OwnedHandle) -> io::Result<Self> {
        let mut needed = 0;
        unsafe {
            GetTokenInformation(raw(token), TokenUser, null_mut(), 0, &mut needed);
        }
        if needed == 0 || needed > 65_536 {
            return Err(invalid(tr!(
                "ユーザートークンを確認できません。",
                "Cannot validate the user token."
            )));
        }
        let mut user = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        win(unsafe {
            GetTokenInformation(
                raw(token),
                TokenUser,
                user.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        })?;
        let sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        let mut sid_text = null_mut();
        win(unsafe { ConvertSidToStringSidW(sid, &mut sid_text) })?;
        let sid_string = unsafe { String::from_utf16_lossy(wide_slice(sid_text)) };
        unsafe {
            LocalFree(sid_text.cast());
        }
        let descriptor_text = wide(OsStr::new(&format!(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid_string})"
        )));
        let mut descriptor = null_mut();
        win(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor_text.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        })?;
        Ok(Self { descriptor, user })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }

    fn user_sid(&self) -> *mut c_void {
        unsafe { (*(self.user.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn validate_directory(&self, file: &File, strict: bool) -> io::Result<()> {
        let mut owner = null_mut();
        let mut acl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let result = (|| {
            if owner.is_null() || unsafe { EqualSid(owner, self.user_sid()) } == 0 || acl.is_null()
            {
                return Err(unsafe_directory());
            }
            for index in 0..unsafe { (*acl).AceCount } {
                let mut ace = null_mut();
                win(unsafe { GetAce(acl, u32::from(index), &mut ace) })?;
                let header = unsafe { &*ace.cast::<ACE_HEADER>() };
                // Only ordinary allow/deny ACEs are understood. Reject exotic ACEs
                // instead of accidentally treating an object/callback allow as safe.
                if header.AceType == 1 || header.AceFlags & 0x08 != 0 {
                    continue;
                }
                if header.AceType != 0
                    || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
                {
                    return Err(unsafe_directory());
                }
                let allow = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = (&allow.SidStart as *const u32).cast_mut().cast();
                let trusted = unsafe {
                    EqualSid(sid, self.user_sid()) != 0
                        || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                        || (!strict && IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0)
                };
                // Write data/add file, append/add directory, write EA/attributes,
                // delete child, DELETE, WRITE_DAC, WRITE_OWNER, generic write/all.
                if !trusted && (strict || allow.Mask & 0x500d_0156 != 0) {
                    return Err(unsafe_directory());
                }
            }
            Ok(())
        })();
        unsafe {
            LocalFree(descriptor);
        }
        result
    }
}

struct Events {
    ready: OwnedHandle,
    commit: OwnedHandle,
    abort: OwnedHandle,
    resumed: OwnedHandle,
}

impl Events {
    fn create(job: &str, security: &PrivateSecurity) -> io::Result<Self> {
        let attributes = security.attributes();
        let create = |kind| {
            let name = wide(OsStr::new(&event_name(job, kind)));
            let handle = unsafe { CreateEventW(&attributes, 1, 0, name.as_ptr()) };
            let error = unsafe { GetLastError() };
            let handle = owned(handle)?;
            if error == ERROR_ALREADY_EXISTS {
                return Err(invalid(tr!(
                    "更新用の通知が既に存在します。",
                    "An update notification already exists."
                )));
            }
            Ok(handle)
        };
        Ok(Self {
            ready: create("ready")?,
            commit: create("commit")?,
            abort: create("abort")?,
            resumed: create("resumed")?,
        })
    }

    fn open(job: &str) -> io::Result<Self> {
        let open = |kind| {
            let name = wide(OsStr::new(&event_name(job, kind)));
            owned(unsafe {
                OpenEventW(
                    SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                    0,
                    name.as_ptr(),
                )
            })
        };
        Ok(Self {
            ready: open("ready")?,
            commit: open("commit")?,
            abort: open("abort")?,
            resumed: open("resumed")?,
        })
    }
}

/// A staging directory and its target lease. Dropping it never modifies the app.
pub struct Stage {
    job: PathBuf,
    candidate: PathBuf,
    target: PathBuf,
    manifest: Manifest,
    _target: File,
    _target_directory: File,
    _stage_directories: Vec<File>,
    _lease: OwnedHandle,
    cleanup: bool,
}

impl Stage {
    pub fn new(restart: Restart) -> io::Result<Self> {
        let restart = WireRestart::from_restart(restart)?;
        let security = PrivateSecurity::new()?;
        let target = fs::canonicalize(std::env::current_exe()?)?;
        validate_absolute(&target)?;
        let target_directory = open_directory(target.parent().ok_or_else(|| {
            invalid(tr!(
                "実行ファイルの場所が不正です。",
                "Invalid executable location."
            ))
        })?)?;
        security.validate_directory(&target_directory, false)?;
        let mut target_file = open_image(&target, true)?;
        let target_id = identity(&target_file)?;
        let directory_id = identity(&target_directory)?;
        let lease = create_lease(&target, &security)?;
        let old_hash = hash_file(&mut target_file)?;
        let parent_started = process_started(unsafe { GetCurrentProcess() })?;
        let (base, mut directories) = stage_base(&security, true)?;
        let job_id = random_id()?;
        let job = base.join(&job_id);
        create_private_directory(&job, &security, false)?;
        let job_directory = match open_directory(&job).and_then(|file| {
            security.validate_directory(&file, true)?;
            Ok(file)
        }) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir(&job);
                return Err(error);
            }
        };
        directories.push(job_directory);
        let manifest = Manifest {
            schema: SCHEMA,
            job: job_id,
            target: target.as_os_str().encode_wide().collect(),
            target_id,
            directory_id,
            parent_pid: std::process::id(),
            parent_started,
            old_hash,
            new_hash: [0; 32],
            restart,
        };
        let stage = Self {
            candidate: job.join("candidate.exe"),
            job,
            target,
            manifest,
            _target: target_file,
            _target_directory: target_directory,
            _stage_directories: directories,
            _lease: lease,
            cleanup: true,
        };
        // Probe create, durable write, rename and delete in the target directory;
        // the target handle separately proves DELETE access to the existing exe.
        probe_target_directory(&stage.target, &stage.manifest.job)?;
        Ok(stage)
    }

    pub fn candidate_path(&self) -> &Path {
        &self.candidate
    }

    pub fn prepare(mut self, expected: [u8; 32]) -> io::Result<Pending> {
        let mut candidate = open_image(&self.candidate, false)?;
        verify_image(&mut candidate, expected)?;
        self.manifest.new_hash = expected;
        let mut original = self._target.try_clone()?;
        original.seek(SeekFrom::Start(0))?;
        let mut helper = create_new(&self.job.join("helper.exe"), false)?;
        io::copy(&mut original, &mut helper)?;
        helper.sync_all()?;
        if hash_file(&mut helper)? != self.manifest.old_hash {
            return Err(invalid(tr!(
                "更新ヘルパーのコピーを検証できません。",
                "Cannot verify the update helper copy."
            )));
        }
        drop(helper);
        write_json_new(&self.job.join("manifest.json"), &self.manifest)?;
        let security = PrivateSecurity::new()?;
        let events = Events::create(&self.manifest.job, &security)?;
        let child = spawn(
            &self.job.join("helper.exe"),
            "--rfm-apply-update",
            &self.job,
        )?;
        let mut pending = Pending {
            stage: Some(self),
            child: Some(child),
            events,
            committed: false,
            _candidate: Some(candidate),
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if signaled(&pending.events.ready)? {
                break;
            }
            if pending
                .child
                .as_mut()
                .expect("helper child")
                .try_wait()?
                .is_some()
            {
                return Err(helper_error(&pending.stage.as_ref().expect("stage").job));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    tr!(
                        "更新ヘルパーの準備がタイムアウトしました。",
                        "The update helper readiness handshake timed out."
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
        let ready: ProcessRecord = read_json(
            &pending
                .stage
                .as_ref()
                .expect("stage")
                .job
                .join("ready.json"),
        )?;
        let child = pending.child.as_ref().expect("helper child");
        if ready.schema != SCHEMA
            || ready.pid != child.id()
            || ready.started != process_started(child.as_raw_handle())?
        {
            return Err(invalid(tr!(
                "更新ヘルパーの応答が不正です。",
                "Invalid update helper handshake."
            )));
        }
        Ok(pending)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.cleanup {
            self._stage_directories.pop();
            cleanup_job(&self.job);
        }
    }
}

/// An acknowledged helper that is still forbidden to change the executable.
pub struct Pending {
    stage: Option<Stage>,
    child: Option<Child>,
    events: Events,
    committed: bool,
    _candidate: Option<File>,
}

impl Pending {
    pub fn commit(mut self) -> io::Result<()> {
        if self
            .child
            .as_mut()
            .expect("helper child")
            .try_wait()?
            .is_some()
        {
            return Err(helper_error(&self.stage.as_ref().expect("stage").job));
        }
        win(unsafe { SetEvent(raw(&self.events.commit)) })?;
        self.committed = true;
        if let Some(stage) = self.stage.as_mut() {
            stage.cleanup = false;
        }
        Ok(())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                SetEvent(raw(&self.events.abort));
            }
            if let Some(child) = self.child.as_mut() {
                // This is only our own, uncommitted helper. No PID enumeration,
                // process-name matching or termination of another app is used.
                if unsafe { WaitForSingleObject(child.as_raw_handle(), 5_000) } != WAIT_OBJECT_0 {
                    stop_owned_child(child);
                }
            }
            drop(self._candidate.take());
        }
    }
}

/// Recognizes only the private helper entry point, before normal app startup.
pub fn run_helper_if_requested() -> Option<i32> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(OsStr::new("--rfm-apply-update")) {
        return None;
    }
    let job = match (arguments.next(), arguments.next()) {
        (Some(job), None) => PathBuf::from(job),
        _ => {
            show_error(tr!(
                "更新ヘルパーの引数が不正です。",
                "Invalid update helper arguments."
            ));
            return Some(1);
        }
    };
    let mut committed = false;
    Some(match helper(&job, &mut committed) {
        Ok(()) => 0,
        Err(error) => {
            report_helper_error(&job, &error, committed);
            1
        }
    })
}

/// Restores UTF-16 paths only for an authenticated restart from our own helper.
pub fn resume_from_args() -> io::Result<Option<Restart>> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(OsStr::new("--rfm-update-resume")) {
        return Ok(None);
    }
    let job = match (arguments.next(), arguments.next()) {
        (Some(job), None) => PathBuf::from(job),
        _ => {
            return Err(invalid(tr!(
                "更新後の再開引数が不正です。",
                "Invalid update restart arguments."
            )));
        }
    };
    let security = PrivateSecurity::new()?;
    let (manifest, directories) = validate_job(&job, &security)?;
    let target = from_wide(&manifest.target);
    let mut current = open_image(&target, false)?;
    let own = open_image(&fs::canonicalize(std::env::current_exe()?)?, false)?;
    if identity(&current)? != identity(&own)? {
        return Err(invalid(tr!(
            "更新後の実行ファイルの場所が一致しません。",
            "The restarted executable location does not match."
        )));
    }
    let launch: Launch = read_json(&job.join("launch.json"))?;
    if launch.schema != SCHEMA
        || launch.expected_hash
            != if launch.rollback {
                manifest.old_hash
            } else {
                manifest.new_hash
            }
        || launch
            .reason
            .as_ref()
            .is_some_and(|value| value.len() > 16_384)
    {
        return Err(invalid(tr!(
            "更新後の起動情報が不正です。",
            "Invalid update launch information."
        )));
    }
    verify_image(&mut current, launch.expected_hash)?;
    let ready: ProcessRecord = read_json(&job.join("ready.json"))?;
    let helper = validated_process(&ready, &job.join("helper.exe"))?;
    let events = Events::open(&manifest.job)?;
    let restart = manifest.restart.restart();
    let replacement_id = if job.join("replacement.json").try_exists()? {
        let replacement: Replacement = read_json(&job.join("replacement.json"))?;
        if replacement.schema != SCHEMA {
            return Err(invalid(tr!(
                "置換ファイルの情報が不正です。",
                "Invalid replacement file information."
            )));
        }
        Some(replacement.identity)
    } else {
        None
    };
    let target_directory = open_directory(
        target
            .parent()
            .ok_or_else(|| invalid(tr!("更新先が不正です。", "Invalid update target.")))?,
    )?;
    security.validate_directory(&target_directory, false)?;
    if identity(&target_directory)? != manifest.directory_id {
        return Err(unsafe_directory());
    }
    let state = ResumeState {
        backup: backup_path(&target, &manifest.job),
        partial: sibling_path(&target, &manifest.job, "new"),
        failed: sibling_path(&target, &manifest.job, "failed"),
        job,
        old_hash: manifest.old_hash,
        old_id: manifest.target_id,
        replacement_id,
        helper,
        resumed: events.resumed,
        directories,
        target_directory,
        reason: launch.reason,
    };
    *RESUME.lock().map_err(|_| {
        io::Error::other(tr!(
            "再開情報のロックを取得できません。",
            "Cannot lock the update restart information."
        ))
    })? = Some(state);
    Ok(Some(restart))
}

/// Called only after the restarted main window is visible and initialized.
pub fn acknowledge_restart() -> io::Result<()> {
    let state = RESUME
        .lock()
        .map_err(|_| {
            io::Error::other(tr!(
                "再開情報のロックを取得できません。",
                "Cannot lock the update restart information."
            ))
        })?
        .take();
    let Some(state) = state else {
        return Ok(());
    };
    let ResumeState {
        job,
        backup,
        partial,
        failed,
        old_hash,
        old_id,
        replacement_id,
        helper,
        resumed,
        mut directories,
        target_directory,
        reason,
    } = state;
    // Do not acknowledge until cleanup is scheduled. A failed startup keeps the
    // backup intact; the helper stops only its own unacknowledged child and rolls
    // back. Cleanup starts after acknowledgment and waits for the helper to exit.
    thread::Builder::new()
        .name("rfm-update-cleanup".into())
        .spawn(move || {
            if unsafe { WaitForSingleObject(raw(&helper), CLEAN_TIMEOUT.as_millis() as u32) }
                == WAIT_OBJECT_0
            {
                remove_owned_image(&backup, old_hash, old_id);
                if let Some(id) = replacement_id {
                    remove_owned_partial(&partial, id);
                    remove_owned_partial(&failed, id);
                }
                drop(target_directory);
                directories.pop();
                cleanup_job(&job);
            }
        })?;
    win(unsafe { SetEvent(raw(&resumed)) })?;
    if let Some(reason) = reason {
        show_error(&reason);
    }
    Ok(())
}

fn helper(job: &Path, committed: &mut bool) -> io::Result<()> {
    let security = PrivateSecurity::new()?;
    let (manifest, mut directories) = validate_job(job, &security)?;
    set_language(manifest.restart.japanese);
    let target = from_wide(&manifest.target);
    let own_path = fs::canonicalize(std::env::current_exe()?)?;
    let own = open_image(&own_path, false)?;
    let mut target_file = open_image(&target, true)?;
    let target_directory = open_directory(
        target
            .parent()
            .ok_or_else(|| invalid(tr!("更新先が不正です。", "Invalid update target.")))?,
    )?;
    security.validate_directory(&target_directory, false)?;
    if identity(&target_directory)? != manifest.directory_id {
        return Err(unsafe_directory());
    }
    // A short-lived cleanup invocation runs from the unchanged installed exe,
    // never from the staging helper that it will eventually delete.
    if job.join("cleanup.json").try_exists()? && identity(&own)? == identity(&target_file)? {
        if identity(&target_file)? != manifest.target_id
            || hash_file(&mut target_file)? != manifest.old_hash
        {
            return Err(invalid(tr!(
                "中止した更新の元の実行ファイルが一致しません。",
                "The aborted update's original executable does not match."
            )));
        }
        let record: ProcessRecord = read_json(&job.join("cleanup.json"))?;
        let helper = validated_process(&record, &job.join("helper.exe"))?;
        let ready_name = wide(OsStr::new(&event_name(&manifest.job, "cleanup-ready")));
        let ready = owned(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, ready_name.as_ptr()) })?;
        win(unsafe { SetEvent(raw(&ready)) })?;
        if unsafe { WaitForSingleObject(raw(&helper), CLEAN_TIMEOUT.as_millis() as u32) }
            != WAIT_OBJECT_0
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                tr!(
                    "更新ヘルパーの終了待機がタイムアウトしました。",
                    "Waiting for the update helper to exit timed out."
                ),
            ));
        }
        if job.join("replacement.json").try_exists()? {
            let replacement: Replacement = read_json(&job.join("replacement.json"))?;
            if replacement.schema != SCHEMA {
                return Err(invalid(tr!(
                    "置換ファイルの情報が不正です。",
                    "Invalid replacement file information."
                )));
            }
            remove_owned_partial(
                &sibling_path(&target, &manifest.job, "new"),
                replacement.identity,
            );
            remove_owned_partial(
                &sibling_path(&target, &manifest.job, "failed"),
                replacement.identity,
            );
        }
        directories.pop();
        cleanup_job(job);
        return Ok(());
    }
    if fs::canonicalize(job.join("helper.exe"))? != own_path
        || identity(&target_file)? != manifest.target_id
        || hash_file(&mut target_file)? != manifest.old_hash
    {
        return Err(invalid(tr!(
            "更新ヘルパーまたは更新先が元の実行ファイルと一致しません。",
            "The update helper or target does not match the original executable."
        )));
    }
    let mut own_hash = own.try_clone()?;
    verify_image(&mut own_hash, manifest.old_hash)?;
    let helper_record = ProcessRecord {
        schema: SCHEMA,
        pid: std::process::id(),
        started: process_started(unsafe { GetCurrentProcess() })?,
    };
    write_json_new(&job.join("ready.json"), &helper_record)?;
    let parent_record = ProcessRecord {
        schema: SCHEMA,
        pid: manifest.parent_pid,
        started: manifest.parent_started,
    };
    let parent = match validated_process(&parent_record, &target) {
        Ok(parent) if !signaled(&parent)? => parent,
        _ => {
            arrange_aborted_cleanup(job, &target, &helper_record, &security)?;
            return Ok(());
        }
    };
    let lease_name = wide(OsStr::new(&lease_name(&target)?));
    let _lease = owned(unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, lease_name.as_ptr()) })?;
    let events = Events::open(&manifest.job)?;
    let mut candidate = open_image(&job.join("candidate.exe"), false)?;
    verify_image(&mut candidate, manifest.new_hash)?;
    win(unsafe { SetEvent(raw(&events.ready)) })?;
    let deadline = Instant::now() + COMMIT_TIMEOUT;
    loop {
        if signaled(&events.abort)? {
            return Ok(());
        }
        if signaled(&events.commit)? {
            break;
        }
        if signaled(&parent)? {
            arrange_aborted_cleanup(job, &target, &helper_record, &security)?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    *committed = true;
    if !wait_parent_exit(&parent, &events.abort, EXIT_TIMEOUT)? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            tr!(
                "rfm が終了しなかったため、実行ファイルを変更せず更新を中止しました。",
                "rfm did not exit in time. The update was canceled without changing the executable."
            ),
        ));
    }
    // Parent identity is pinned by its process HANDLE, not by a reusable PID.
    // Candidate and target handles deny concurrent writers throughout cutover.
    let still_at_target = open_image(&target, true)?;
    if identity(&still_at_target)? != manifest.target_id
        || hash_file(&mut target_file)? != manifest.old_hash
    {
        return Err(invalid(tr!(
            "更新前に実行ファイルが変更されたため中止しました。",
            "The executable changed before installation. The update was canceled."
        )));
    }
    if let Err(error) = verify_image(&mut candidate, manifest.new_hash) {
        return restart_unchanged(job, &manifest, &events, &error);
    }
    let result = install_and_restart(job, &manifest, &mut target_file, &mut candidate, &events);
    drop(directories);
    result
}

fn install_and_restart(
    job: &Path,
    manifest: &Manifest,
    old: &mut File,
    candidate: &mut File,
    events: &Events,
) -> io::Result<()> {
    let target = from_wide(&manifest.target);
    let temp_path = sibling_path(&target, &manifest.job, "new");
    let backup = backup_path(&target, &manifest.job);
    let security = PrivateSecurity::new()?;
    let mut writer = match create_private_file(&temp_path, &security) {
        Ok(file) => file,
        Err(error) => return restart_unchanged(job, manifest, events, &error),
    };
    let prepare = (|| {
        let expected_id = identity(&writer)?;
        write_json_new(
            &job.join("replacement.json"),
            &Replacement {
                schema: SCHEMA,
                identity: expected_id,
            },
        )?;
        candidate.seek(SeekFrom::Start(0))?;
        io::copy(candidate, &mut writer)?;
        writer.sync_all()?;
        verify_image(&mut writer, manifest.new_hash)?;
        Ok(expected_id)
    })();
    let expected_id = match prepare {
        Ok(id) => id,
        Err(error) => {
            let _ = delete_handle(&writer);
            return restart_unchanged(job, manifest, events, &error);
        }
    };
    // A writable file handle prevents Windows from creating an executable image
    // section. Close it and pin the same file read-only before rename/launch.
    drop(writer);
    let mut temp = match open_image(&temp_path, true) {
        Ok(file) => file,
        Err(error) => return restart_unchanged(job, manifest, events, &error),
    };
    if identity(&temp)? != expected_id {
        return Err(unsafe_directory());
    }
    if let Err(error) = verify_image(&mut temp, manifest.new_hash) {
        let _ = delete_handle(&temp);
        return restart_unchanged(job, manifest, events, &error);
    }
    // Handle-based, non-overwriting renames preserve unrelated existing files.
    if let Err(error) = rename_handle(old, &backup) {
        let _ = delete_handle(&temp);
        return restart_unchanged(job, manifest, events, &error);
    }
    if let Err(error) = rename_handle(&temp, &target) {
        let _ = delete_handle(&temp);
        return rollback(job, manifest, old, None, events, error);
    }
    match launch_and_ack(job, &target, manifest.new_hash, false, None, events) {
        Ok(()) => Ok(()),
        Err(error) => rollback(job, manifest, old, Some(&temp), events, error),
    }
}

fn rollback(
    job: &Path,
    manifest: &Manifest,
    old: &File,
    new: Option<&File>,
    events: &Events,
    error: io::Error,
) -> io::Result<()> {
    let target = from_wide(&manifest.target);
    if let Some(new) = new {
        let failed = sibling_path(&target, &manifest.job, "failed");
        rename_handle(new, &failed).map_err(|restore| rollback_error(&error, &restore))?;
    }
    if let Err(restore) = rename_handle(old, &target) {
        // Never overwrite a file that appeared at the target during rollback.
        // The verified original remains at the unique backup path for recovery.
        return Err(rollback_error(&error, &restore));
    }
    if let Some(new) = new {
        let _ = delete_handle(new);
    }
    restart_unchanged(job, manifest, events, &error)
}

fn restart_unchanged(
    job: &Path,
    manifest: &Manifest,
    events: &Events,
    error: &io::Error,
) -> io::Result<()> {
    let message = tr_format!(
        "更新できなかったため、元のバージョンを再起動しました。\n\n{}",
        "The update failed. The previous version has been restarted.\n\n{}",
        error
    );
    launch_and_ack(
        job,
        &from_wide(&manifest.target),
        manifest.old_hash,
        true,
        Some(message),
        events,
    )
}

fn launch_and_ack(
    job: &Path,
    target: &Path,
    expected_hash: [u8; 32],
    rollback: bool,
    reason: Option<String>,
    events: &Events,
) -> io::Result<()> {
    let launch = Launch {
        schema: SCHEMA,
        expected_hash,
        rollback,
        reason,
    };
    // The failed child has stopped before a rollback launch is attempted.
    if rollback && job.join("launch.json").try_exists()? {
        fs::remove_file(job.join("launch.json"))?;
    }
    write_json_new(&job.join("launch.json"), &launch)?;
    win(unsafe { ResetEvent(raw(&events.resumed)) })?;
    let mut child = spawn(target, "--rfm-update-resume", job)?;
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let result = (|| {
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(tr_format!(
                    "再起動した rfm が開始前に終了しました ({status})。",
                    "The restarted rfm exited before startup ({status})."
                )));
            }
            if signaled(&events.resumed)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    tr!(
                        "再起動した rfm の応答がありません。",
                        "The restarted rfm did not acknowledge startup."
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    })();
    if result.is_err() {
        stop_owned_child(&mut child);
    }
    result
}

fn stop_owned_child(child: &mut Child) {
    // Child::kill targets the retained CreateProcess HANDLE, never a PID lookup.
    let _ = child.kill();
    unsafe {
        WaitForSingleObject(child.as_raw_handle(), 5_000);
    }
    let _ = child.try_wait();
}

fn arrange_aborted_cleanup(
    job: &Path,
    target: &Path,
    helper: &ProcessRecord,
    security: &PrivateSecurity,
) -> io::Result<()> {
    // Keep this helper alive until the native cleaner pins our process HANDLE.
    // Otherwise a fast exit could destroy the process object before OpenProcess.
    let job_id = job
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(unsafe_directory)?;
    let name = wide(OsStr::new(&event_name(job_id, "cleanup-ready")));
    let attributes = security.attributes();
    let ready = owned(unsafe { CreateEventW(&attributes, 1, 0, name.as_ptr()) })?;
    write_json_new(&job.join("cleanup.json"), helper)?;
    let mut child = spawn(target, "--rfm-apply-update", job)?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if signaled(&ready)? {
            return Ok(());
        }
        if child.try_wait()?.is_some() || Instant::now() >= deadline {
            stop_owned_child(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                tr!(
                    "中止した更新の後片付けを開始できません。",
                    "Cannot start cleanup of the canceled update."
                ),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn set_language(japanese: bool) {
    crate::i18n::initialize(if japanese {
        crate::i18n::Language::Japanese
    } else {
        crate::i18n::Language::English
    });
}

fn validated_process(record: &ProcessRecord, expected_image: &Path) -> io::Result<OwnedHandle> {
    if record.schema != SCHEMA || record.pid == 0 || record.started == 0 {
        return Err(invalid(tr!(
            "更新プロセスの情報が不正です。",
            "Invalid update process information."
        )));
    }
    let handle = owned(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            record.pid,
        )
    })?;
    if process_started(raw(&handle))? != record.started {
        return Err(invalid(tr!(
            "更新プロセスが既に入れ替わっています。",
            "The update process identity has changed."
        )));
    }
    let mut image = vec![0u16; 32_768];
    let mut length = image.len() as u32;
    win(unsafe { QueryFullProcessImageNameW(raw(&handle), 0, image.as_mut_ptr(), &mut length) })?;
    image.truncate(length as usize);
    let actual = open_image(&from_wide(&image), false)?;
    let expected = open_image(expected_image, false)?;
    if identity(&actual)? != identity(&expected)? {
        return Err(invalid(tr!(
            "更新プロセスの実行ファイルが一致しません。",
            "The update process image does not match."
        )));
    }
    Ok(handle)
}

fn process_started(handle: HANDLE) -> io::Result<u64> {
    let mut created = Default::default();
    let mut exited = Default::default();
    let mut kernel = Default::default();
    let mut user = Default::default();
    win(unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) })?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn wait_parent_exit(
    parent: &OwnedHandle,
    abort: &OwnedHandle,
    timeout: Duration,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if signaled(abort)? {
            return Ok(false);
        }
        if signaled(parent)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn signaled(handle: &OwnedHandle) -> io::Result<bool> {
    match unsafe { WaitForSingleObject(raw(handle), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn stage_base(security: &PrivateSecurity, create: bool) -> io::Result<(PathBuf, Vec<File>)> {
    let mut raw_path = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, null_mut(), &mut raw_path) };
    if result < 0 {
        return Err(io::Error::other(tr!(
            "ユーザー専用の保存場所を取得できません。",
            "Cannot locate the per-user application directory."
        )));
    }
    let local = unsafe { from_wide(wide_slice(raw_path)) };
    unsafe {
        CoTaskMemFree(raw_path.cast());
    }
    let local = fs::canonicalize(local)?;
    let mut path = local;
    let mut directories = Vec::new();
    // Pin the known folder and each component against rename/junction exchange.
    let root = open_directory(&path)?;
    security.validate_directory(&root, false)?;
    directories.push(root);
    for component in ["rfm", "updates"] {
        path.push(component);
        if create {
            create_private_directory(&path, security, true)?;
        }
        let directory = open_directory(&path)?;
        security.validate_directory(&directory, false)?;
        directories.push(directory);
    }
    Ok((path, directories))
}

fn validate_job(job: &Path, security: &PrivateSecurity) -> io::Result<(Manifest, Vec<File>)> {
    validate_absolute(job)?;
    let (base, mut directories) = stage_base(security, false)?;
    let id = job
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| valid_job_id(name))
        .ok_or_else(unsafe_directory)?;
    if job != base.join(id) {
        return Err(unsafe_directory());
    }
    let directory = open_directory(job)?;
    security.validate_directory(&directory, true)?;
    directories.push(directory);
    let manifest: Manifest = read_json(&job.join("manifest.json"))?;
    if manifest.schema != SCHEMA
        || manifest.job != id
        || manifest.parent_pid == 0
        || manifest.parent_started == 0
    {
        return Err(invalid(tr!(
            "更新情報の形式が不正です。",
            "Invalid update manifest schema."
        )));
    }
    manifest.restart.validate()?;
    let target = from_wide(&manifest.target);
    validate_absolute(&target)?;
    if fs::canonicalize(&target)? != target {
        return Err(unsafe_directory());
    }
    Ok((manifest, directories))
}

fn valid_job_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn random_id() -> io::Result<String> {
    let mut random = [0u8; 16];
    if unsafe { BCryptGenRandom(null_mut(), random.as_mut_ptr(), random.len() as u32, 2) } < 0 {
        return Err(io::Error::other(tr!(
            "安全な更新識別子を生成できません。",
            "Cannot generate a secure update identifier."
        )));
    }
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        random[0],
        random[1],
        random[2],
        random[3],
        random[4],
        random[5],
        random[6],
        random[7],
        random[8],
        random[9],
        random[10],
        random[11],
        random[12],
        random[13],
        random[14],
        random[15]
    ))
}

fn create_private_directory(
    path: &Path,
    security: &PrivateSecurity,
    existing_ok: bool,
) -> io::Result<()> {
    let path = wide(path.as_os_str());
    let attributes = security.attributes();
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if !existing_ok || error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
    }
    Ok(())
}

fn create_lease(target: &Path, security: &PrivateSecurity) -> io::Result<OwnedHandle> {
    let name = wide(OsStr::new(&lease_name(target)?));
    let attributes = security.attributes();
    // Existence is the lease; mutex ownership is deliberately never acquired,
    // so Stage/Pending can move between the download worker and UI thread.
    let handle = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
    let error = unsafe { GetLastError() };
    let handle = owned(handle)?;
    if error == ERROR_ALREADY_EXISTS {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            tr!(
                "この実行ファイルの更新は既に進行中です。",
                "An update for this executable is already in progress."
            ),
        ));
    }
    Ok(handle)
}

fn lease_name(target: &Path) -> io::Result<String> {
    let source: Vec<u16> = target.as_os_str().encode_wide().collect();
    let length = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source.len() as i32,
            null_mut(),
            0,
            null(),
            null(),
            0,
        )
    };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut mapped = vec![0u16; length as usize];
    if unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            source.as_ptr(),
            source.len() as i32,
            mapped.as_mut_ptr(),
            length,
            null(),
            null(),
            0,
        )
    } != length
    {
        return Err(io::Error::last_os_error());
    }
    let mut hash = Sha256::new();
    for unit in mapped {
        hash.update(unit.to_le_bytes());
    }
    Ok(format!("Local\\rfm-update-{:x}", hash.finalize()))
}

fn event_name(job: &str, kind: &str) -> String {
    format!("Local\\rfm-update-{job}-{kind}")
}

fn open_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let attributes = file.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(unsafe_directory());
    }
    Ok(file)
}

fn open_image(path: &Path, deletable: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | if deletable { DELETE } else { 0 })
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(unsafe_directory());
    }
    Ok(file)
}

fn create_new(path: &Path, deletable: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | if deletable { DELETE } else { 0 })
        .share_mode(FILE_SHARE_READ | if deletable { FILE_SHARE_DELETE } else { 0 })
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn create_private_file(path: &Path, security: &PrivateSecurity) -> io::Result<File> {
    let path = wide(path.as_os_str());
    let attributes = security.attributes();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | DELETE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn probe_target_directory(target: &Path, job: &str) -> io::Result<()> {
    let first = sibling_path(target, job, "probe");
    let second = sibling_path(target, job, "probe-renamed");
    let mut file = create_new(&first, true)?;
    let result = (|| {
        file.write_all(b"rfm update permission probe\n")?;
        file.sync_all()?;
        rename_handle(&file, &second)
    })();
    let cleanup = delete_handle(&file);
    result.and(cleanup)
}

fn sibling_path(target: &Path, job: &str, suffix: &str) -> PathBuf {
    target
        .parent()
        .expect("validated target parent")
        .join(format!(".rfm-update-{job}.{suffix}"))
}

fn backup_path(target: &Path, job: &str) -> PathBuf {
    sibling_path(target, job, "backup")
}

fn rename_handle(file: &File, destination: &Path) -> io::Result<()> {
    let name = wide(destination.as_os_str());
    let bytes = offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let mut buffer = vec![0u64; bytes.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = ((name.len() - 1) * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        win(SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfo,
            info.cast(),
            bytes as u32,
        ))
    }
}

fn delete_handle(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    win(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    })
}

fn identity(file: &File) -> io::Result<Identity> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    win(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    Ok(Identity {
        volume: info.dwVolumeSerialNumber,
        high: info.nFileIndexHigh,
        low: info.nFileIndexLow,
    })
}

fn hash_file(file: &mut File) -> io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    if file.metadata()?.len() > MAX_EXE {
        return Err(invalid(tr!(
            "更新ファイルが大きすぎます。",
            "The update executable is too large."
        )));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize().into())
}

fn verify_image(file: &mut File, expected: [u8; 32]) -> io::Result<()> {
    if hash_file(file)? != expected {
        return Err(invalid(tr!(
            "更新ファイルの SHA-256 が一致しません。",
            "The update executable SHA-256 does not match."
        )));
    }
    let length = file.metadata()?.len();
    validate_pe(file, length)
}

fn validate_pe(reader: &mut (impl Read + Seek), length: u64) -> io::Result<()> {
    let bad = || {
        invalid(tr!(
            "更新ファイルは有効な x64 Windows 実行ファイルではありません。",
            "The update is not a valid x64 Windows executable."
        ))
    };
    if !(512..=MAX_EXE).contains(&length) {
        return Err(bad());
    }
    reader.seek(SeekFrom::Start(0))?;
    let mut dos = [0u8; 64];
    reader.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(bad());
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().expect("four bytes")) as u64;
    if offset < 64 || offset + 24 > length {
        return Err(bad());
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut coff = [0u8; 24];
    reader.read_exact(&mut coff)?;
    let short = |bytes: &[u8]| u16::from_le_bytes(bytes.try_into().expect("two bytes"));
    let word = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().expect("four bytes"));
    let sections = short(&coff[6..8]) as usize;
    let optional_length = short(&coff[20..22]) as usize;
    let flags = short(&coff[22..24]);
    if &coff[..4] != b"PE\0\0"
        || short(&coff[4..6]) != 0x8664
        || !(1..=96).contains(&sections)
        || flags & 2 == 0
        || flags & 0x2000 != 0
        || !(240..=4096).contains(&optional_length)
        || offset + 24 + optional_length as u64 + sections as u64 * 40 > length
    {
        return Err(bad());
    }
    let mut optional = vec![0u8; optional_length];
    reader.read_exact(&mut optional)?;
    let entry = word(&optional[16..20]);
    let image_size = word(&optional[56..60]);
    let header_size = word(&optional[60..64]);
    if short(&optional[..2]) != 0x20b
        || entry == 0
        || image_size == 0
        || image_size > 1024 * 1024 * 1024
        || u64::from(header_size) > length
        || u64::from(header_size) < offset + 24 + optional_length as u64 + sections as u64 * 40
        || ![2, 3].contains(&short(&optional[68..70]))
    {
        return Err(bad());
    }
    let mut executable_entry = false;
    for _ in 0..sections {
        let mut section = [0u8; 40];
        reader.read_exact(&mut section)?;
        let virtual_size = u64::from(word(&section[8..12]));
        let address = u64::from(word(&section[12..16]));
        let raw_size = u64::from(word(&section[16..20]));
        let raw_offset = u64::from(word(&section[20..24]));
        if raw_offset + raw_size > length
            || address + virtual_size.max(raw_size) > u64::from(image_size)
        {
            return Err(bad());
        }
        if u64::from(entry) >= address
            && u64::from(entry) < address + virtual_size.max(raw_size)
            && word(&section[36..40]) & 0x2000_0000 != 0
        {
            executable_entry = true;
        }
    }
    if !executable_entry {
        return Err(bad());
    }
    Ok(())
}

fn validate_absolute(path: &Path) -> io::Result<()> {
    let value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.is_empty()
        || value.len() > 32_760
        || value.contains(&0)
        || !path.is_absolute()
        || path.components().any(|part| match part {
            Component::ParentDir | Component::CurDir => true,
            Component::Prefix(prefix) => matches!(prefix.kind(), Prefix::DeviceNS(_)),
            _ => false,
        })
    {
        return Err(invalid(tr!(
            "更新または再開するパスが不正です。",
            "Invalid update or restart path."
        )));
    }
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid(tr!(
            "更新情報が大きすぎます。",
            "The update manifest is too large."
        )));
    }
    let mut file = create_new(path, false)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = open_image(path, false)?;
    if file.metadata()?.len() > MAX_MANIFEST {
        return Err(invalid(tr!(
            "更新情報が大きすぎます。",
            "The update manifest is too large."
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid(tr!(
            "更新情報が大きすぎます。",
            "The update manifest is too large."
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        invalid(&tr_format!(
            "更新情報を読み取れません: {error}",
            "Cannot read the update manifest: {error}"
        ))
    })
}

fn spawn(executable: &Path, argument: &str, job: &Path) -> io::Result<Child> {
    Command::new(executable)
        .arg(argument)
        .arg(job)
        .current_dir(executable.parent().unwrap_or(job))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

fn cleanup_job(job: &Path) {
    // Callers retain validated parent-directory handles until immediately before
    // this call. Never recurse or interpret a filename supplied by a manifest.
    for name in JOB_FILES {
        let _ = fs::remove_file(job.join(name));
    }
    let _ = fs::remove_dir(job);
}

fn remove_owned_image(path: &Path, hash: [u8; 32], expected_id: Identity) {
    if let Ok(mut file) = open_image(path, true)
        && identity(&file).is_ok_and(|actual| actual == expected_id)
        && hash_file(&mut file).is_ok_and(|actual| actual == hash)
    {
        let _ = delete_handle(&file);
    }
}

fn remove_owned_partial(path: &Path, expected_id: Identity) {
    if let Ok(file) = open_image(path, true)
        && identity(&file).is_ok_and(|actual| actual == expected_id)
    {
        let _ = delete_handle(&file);
    }
}

fn helper_error(job: &Path) -> io::Error {
    let reason = open_image(&job.join("error.txt"), false)
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(16_385).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 16_384 {
                return None;
            }
            String::from_utf8(bytes).ok()
        });
    io::Error::other(reason.unwrap_or_else(|| {
        tr!(
            "更新ヘルパーが準備を完了できませんでした。",
            "The update helper could not finish preparing."
        )
        .to_string()
    }))
}

fn report_helper_error(job: &Path, error: &io::Error, committed: bool) {
    let parent_alive = (|| -> io::Result<bool> {
        let security = PrivateSecurity::new()?;
        let (manifest, _directories) = validate_job(job, &security)?;
        set_language(manifest.restart.japanese);
        let message = error.to_string();
        if message.len() <= 16_384
            && let Ok(mut file) = create_new(&job.join("error.txt"), false)
        {
            let _ = file.write_all(message.as_bytes());
        }
        let record = ProcessRecord {
            schema: SCHEMA,
            pid: manifest.parent_pid,
            started: manifest.parent_started,
        };
        match validated_process(&record, &from_wide(&manifest.target)) {
            Ok(parent) => Ok(!signaled(&parent)?),
            Err(_) => Ok(false),
        }
    })()
    .unwrap_or(false);
    // Before commit, the waiting parent owns error presentation. Afterwards the
    // helper is the only remaining UI and must explain an unrecoverable failure.
    if committed || !parent_alive {
        show_error(&error.to_string());
    }
    if committed || !parent_alive {
        let _ = (|| -> io::Result<()> {
            let security = PrivateSecurity::new()?;
            let (manifest, _directories) = validate_job(job, &security)?;
            let own_path = fs::canonicalize(std::env::current_exe()?)?;
            if own_path != fs::canonicalize(job.join("helper.exe"))?
                || job.join("cleanup.json").try_exists()?
            {
                return Ok(());
            }
            let target = from_wide(&manifest.target);
            let mut original = open_image(&target, true)?;
            if identity(&original)? != manifest.target_id
                || hash_file(&mut original)? != manifest.old_hash
            {
                return Ok(());
            }
            let record = ProcessRecord {
                schema: SCHEMA,
                pid: std::process::id(),
                started: process_started(unsafe { GetCurrentProcess() })?,
            };
            arrange_aborted_cleanup(job, &target, &record, &security)
        })();
    }
}

fn rollback_error(original: &io::Error, restore: &io::Error) -> io::Error {
    io::Error::other(tr_format!(
        "更新に失敗し、元のファイルも戻せませんでした。バックアップは同じフォルダーの .rfm-update-*.backup に保持されています。\n\n更新: {original}\n復元: {restore}",
        "The update failed and the original file could not be restored. Its backup is retained beside the executable as .rfm-update-*.backup.\n\nUpdate: {original}\nRestore: {restore}"
    ))
}

fn show_error(message: &str) {
    let message = wide(OsStr::new(message));
    unsafe {
        MessageBoxW(
            null_mut(),
            message.as_ptr(),
            tr_w!("rfm — 更新エラー", "rfm — Update error"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn from_wide(value: &[u16]) -> PathBuf {
    PathBuf::from(OsString::from_wide(value))
}
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
unsafe fn wide_slice<'a>(value: *const u16) -> &'a [u16] {
    let mut length = 0;
    unsafe {
        while *value.add(length) != 0 {
            length += 1;
        }
        std::slice::from_raw_parts(value, length)
    }
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
fn win(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn unsafe_directory() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        tr!(
            "更新用のパスまたはアクセス権が安全ではありません。",
            "The update path or its access permissions are unsafe."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pe() -> Vec<u8> {
        let mut bytes = vec![0u8; 1024];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&240u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[104..108].copy_from_slice(&4096u32.to_le_bytes());
        bytes[144..148].copy_from_slice(&8192u32.to_le_bytes());
        bytes[148..152].copy_from_slice(&512u32.to_le_bytes());
        bytes[156..158].copy_from_slice(&2u16.to_le_bytes());
        bytes[336..340].copy_from_slice(&512u32.to_le_bytes());
        bytes[340..344].copy_from_slice(&4096u32.to_le_bytes());
        bytes[344..348].copy_from_slice(&512u32.to_le_bytes());
        bytes[348..352].copy_from_slice(&512u32.to_le_bytes());
        bytes[364..368].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_wrong_architecture_dll_truncated_sections_and_nonexecutable_entry() {
        let valid = pe();
        assert!(validate_pe(&mut Cursor::new(&valid), valid.len() as u64).is_ok());
        for (offset, replacement) in [
            (68, 0x014cu16.to_le_bytes().to_vec()),
            (86, 0x2002u16.to_le_bytes().to_vec()),
            (348, 1000u32.to_le_bytes().to_vec()),
            (364, 0x4000_0040u32.to_le_bytes().to_vec()),
        ] {
            let mut bytes = valid.clone();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(validate_pe(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
    }

    #[test]
    fn restart_round_trips_non_unicode_utf16_without_reordering_tabs() {
        let path = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0x65e5,
            0xd800,
        ]));
        let original = Restart {
            folders: vec![path.clone(), PathBuf::from("C:\\日本語\\資料")],
            active: 1,
            japanese: true,
        };
        let wire = WireRestart::from_restart(original).unwrap();
        let bytes = serde_json::to_vec(&wire).unwrap();
        let decoded: WireRestart = serde_json::from_slice(&bytes).unwrap();
        decoded.validate().unwrap();
        let restored = decoded.restart();
        assert_eq!(restored.folders[0], path);
        assert_eq!(restored.folders[1], PathBuf::from("C:\\日本語\\資料"));
        assert_eq!(restored.active, 1);
    }

    #[test]
    fn rejects_invalid_active_tab_and_path_traversal_job_identifiers() {
        assert!(
            WireRestart::from_restart(Restart {
                folders: vec![PathBuf::from("C:\\")],
                active: 1,
                japanese: false
            })
            .is_err()
        );
        assert!(
            WireRestart::from_restart(Restart {
                folders: vec![PathBuf::from("..\\rfm.exe")],
                active: 0,
                japanese: false
            })
            .is_err()
        );
        assert!(valid_job_id("12345678-1234-4234-8234-123456789abc"));
        for value in [
            "..",
            "../12345678-1234-4234-8234-123456789abc",
            "12345678-1234-4234-8234-123456789abg",
        ] {
            assert!(!valid_job_id(value));
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rfm-更新-{}", random_id().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            let mut file = create_new(&path, true).unwrap();
            file.write_all(contents).unwrap();
            file.sync_all().unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for name in [
                "old.exe",
                "new.exe",
                "backup",
                "failed",
                "unrelated",
                "candidate.exe",
            ] {
                let _ = fs::remove_file(self.0.join(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn handle_cutover_and_rollback_preserve_original_identity_and_unrelated_files() {
        let scratch = Scratch::new();
        let target = scratch.file("old.exe", b"original");
        let new_path = scratch.file("new.exe", b"replacement");
        let unrelated = scratch.file("unrelated", b"untouched");
        let old = open_image(&target, true).unwrap();
        let new = open_image(&new_path, true).unwrap();
        let old_id = identity(&old).unwrap();
        assert!(rename_handle(&new, &unrelated).is_err());
        assert_eq!(fs::read(&unrelated).unwrap(), b"untouched");
        rename_handle(&old, &scratch.0.join("backup")).unwrap();
        rename_handle(&new, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"replacement");
        rename_handle(&new, &scratch.0.join("failed")).unwrap();
        rename_handle(&old, &target).unwrap();
        assert_eq!(
            identity(&open_image(&target, false).unwrap()).unwrap(),
            old_id
        );
        assert_eq!(fs::read(&target).unwrap(), b"original");
        delete_handle(&new).unwrap();
    }

    #[test]
    fn checksum_mismatch_and_locked_target_leave_original_unchanged() {
        let scratch = Scratch::new();
        let target = scratch.file("old.exe", b"original");
        let candidate = scratch.file("candidate.exe", &pe());
        let mut candidate = open_image(&candidate, false).unwrap();
        assert_eq!(
            verify_image(&mut candidate, [0; 32]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&target)
            .unwrap();
        assert!(open_image(&target, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        drop(lock);
    }

    #[test]
    fn operation_lease_is_kept_alive_without_thread_affine_mutex_ownership() {
        let scratch = Scratch::new();
        let target = scratch.file("old.exe", b"original");
        let security = PrivateSecurity::from_token(&current_token().unwrap()).unwrap();
        let parent = create_lease(&target, &security).unwrap();
        let name = wide(OsStr::new(&lease_name(&target).unwrap()));
        let helper =
            owned(unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) }).unwrap();
        drop(parent);
        assert_eq!(
            create_lease(&target, &security).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        thread::spawn(move || drop(helper)).join().unwrap();
        assert!(create_lease(&target, &security).is_ok());
    }
}
