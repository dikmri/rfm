//! Official GitHub release discovery and bounded, consent-triggered downloads.
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem::{size_of, size_of_val},
    path::Path,
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::{Win32::Networking::WinHttp::*, core::w};

const LATEST_URL: &str = "https://api.github.com/repos/dikmri/rfm/releases/latest";
const ASSET_NAME: &str = "rfm-windows-x86_64.exe";
const CHECKSUM_NAME: &str = "rfm-windows-x86_64.exe.sha256";
const MAX_METADATA: usize = 256 * 1024;
const MAX_EXE: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_URL: usize = 16 * 1024;
const CHECKSUM_LENGTH: usize = 64 + 2 + ASSET_NAME.len() + 1;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub tag: String,
    pub notes_url: String,
    pub asset_url: String,
    pub checksum_url: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_cancel(cancel: Option<&AtomicBool>) -> io::Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            tr!("更新をキャンセルしました。", "The update was cancelled."),
        ));
    }
    Ok(())
}

struct Budget<'a> {
    cancel: Option<&'a AtomicBool>,
    start: Instant,
    limit: Duration,
}

impl Budget<'_> {
    fn check(&self) -> io::Result<()> {
        check_cancel(self.cancel)?;
        if self.start.elapsed() >= self.limit {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                tr!(
                    "更新サーバーとの通信がタイムアウトしました。",
                    "The update request timed out."
                ),
            ));
        }
        Ok(())
    }
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> io::Result<Self> {
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

struct Session(Handle);

impl Session {
    fn new() -> io::Result<Self> {
        // Automatic proxy discovery and TLS certificate validation are supplied by Windows.
        let handle = Handle::new(unsafe {
            WinHttpOpen(
                w!("rfm-updater/0.2"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            )
        })?;
        if unsafe { WinHttpSetTimeouts(handle.0, 10_000, 15_000, 15_000, 15_000) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn get(&self, url: &str, budget: &Budget<'_>) -> io::Result<Response> {
        let mut url = url.to_owned();
        for hop in 0..=MAX_REDIRECTS {
            budget.check()?;
            let parsed = parse_url(&url)?;
            let host: Vec<u16> = parsed.host.encode_utf16().chain(Some(0)).collect();
            let target: Vec<u16> = parsed.target.encode_utf16().chain(Some(0)).collect();
            let connection =
                Handle::new(unsafe { WinHttpConnect(self.0.0, host.as_ptr(), 443, 0) })?;
            let request = Handle::new(unsafe {
                WinHttpOpenRequest(
                    connection.0,
                    w!("GET"),
                    target.as_ptr(),
                    null(),
                    null(),
                    null(),
                    WINHTTP_FLAG_SECURE,
                )
            })?;
            // Redirects are followed here, never inside WinHTTP: validate every destination first.
            let redirect_policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            if unsafe {
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_REDIRECT_POLICY,
                    (&redirect_policy as *const u32).cast(),
                    size_of::<u32>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let headers = w!("Accept: application/octet-stream\r\nAccept-Encoding: identity\r\n");
            let api_headers = w!(
                "Accept: application/vnd.github+json\r\nAccept-Encoding: identity\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
            );
            if unsafe {
                WinHttpSendRequest(
                    request.0,
                    if parsed.host == "api.github.com" {
                        api_headers
                    } else {
                        headers
                    },
                    u32::MAX,
                    null(),
                    0,
                    0,
                    0,
                )
            } == 0
            {
                budget.check()?;
                return Err(io::Error::last_os_error());
            }
            budget.check()?;
            if unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0 {
                budget.check()?;
                return Err(io::Error::last_os_error());
            }
            budget.check()?;
            let mut status = 0u32;
            let mut length = size_of::<u32>() as u32;
            if unsafe {
                WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    null(),
                    (&mut status as *mut u32).cast(),
                    &mut length,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err(invalid(tr!(
                        "更新サーバーの転送回数が多すぎます。",
                        "Too many update server redirects."
                    )));
                }
                let location = location_header(&request)?;
                url = redirect_url(&url, &location)?;
                continue;
            }
            return Ok(Response {
                request,
                _connection: connection,
                status,
            });
        }
        unreachable!()
    }
}

struct Response {
    // Close the request before its connection; the session remains owned by the caller.
    request: Handle,
    _connection: Handle,
    status: u32,
}

impl Response {
    fn require_ok(&self) -> io::Result<()> {
        if self.status == 200 {
            Ok(())
        } else {
            Err(io::Error::other(tr_format!(
                "更新サーバーが HTTP {} を返しました。",
                "The update server returned HTTP {}.",
                self.status
            )))
        }
    }

    fn read(&self, buffer: &mut [u8], budget: &Budget<'_>) -> io::Result<usize> {
        budget.check()?;
        let mut count = 0u32;
        if unsafe {
            WinHttpReadData(
                self.request.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut count,
            )
        } == 0
        {
            budget.check()?;
            return Err(io::Error::last_os_error());
        }
        budget.check()?;
        Ok(count as usize)
    }

    fn bounded_body(&self, limit: usize, budget: &Budget<'_>) -> io::Result<Vec<u8>> {
        self.require_ok()?;
        let mut body = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let count = self.read(&mut buffer, budget)?;
            if count == 0 {
                return Ok(body);
            }
            if count > limit.saturating_sub(body.len()) {
                return Err(invalid(tr!(
                    "更新サーバーの応答が大きすぎます。",
                    "The update server response is too large."
                )));
            }
            body.extend_from_slice(&buffer[..count]);
        }
    }
}

struct Url<'a> {
    host: &'a str,
    target: &'a str,
}

fn parse_url(url: &str) -> io::Result<Url<'_>> {
    let fail = || {
        invalid(tr!(
            "更新 URL が許可されていません。",
            "The update URL is not allowed."
        ))
    };
    if url.len() > MAX_URL
        || !url.is_ascii()
        || url
            .bytes()
            .any(|c| c <= b' ' || c == 127 || c == b'\\' || c == b'#')
    {
        return Err(fail());
    }
    let rest = url.strip_prefix("https://").ok_or_else(fail)?;
    let slash = rest.find('/').ok_or_else(fail)?;
    let (host, target) = rest.split_at(slash);
    if !matches!(
        host,
        "github.com"
            | "api.github.com"
            | "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com"
    ) {
        return Err(fail());
    }
    Ok(Url { host, target })
}

fn redirect_url(previous: &str, location: &str) -> io::Result<String> {
    let resolved = if location.starts_with('/') && !location.starts_with("//") {
        let previous = parse_url(previous)?;
        format!("https://{}{}", previous.host, location)
    } else {
        location.to_owned()
    };
    parse_url(&resolved)?;
    Ok(resolved)
}

fn location_header(request: &Handle) -> io::Result<String> {
    // A fixed bound also limits redirects with adversarially long signed URLs.
    let mut buffer = [0u16; MAX_URL + 1];
    let mut bytes = size_of_val(&buffer) as u32;
    if unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            null(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let count = bytes as usize / size_of::<u16>();
    let value = buffer.get(..count).ok_or_else(|| {
        invalid(tr!(
            "更新先 URL が長すぎます。",
            "The redirect URL is too long."
        ))
    })?;
    let value = value.strip_suffix(&[0]).unwrap_or(value);
    String::from_utf16(value).map_err(|_| {
        invalid(tr!(
            "更新先 URL が不正です。",
            "The redirect URL is invalid."
        ))
    })
}

fn stable_version(tag: &str) -> io::Result<Version> {
    let fail = || {
        invalid(tr!(
            "リリースのバージョンが不正です。",
            "The release version is invalid."
        ))
    };
    if tag.len() > 128 {
        return Err(fail());
    }
    let text = tag.strip_prefix('v').ok_or_else(fail)?;
    let version = Version::parse(text).map_err(|_| fail())?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(fail());
    }
    Ok(version)
}

fn asset_url(tag: &str, name: &str) -> String {
    format!("https://github.com/dikmri/rfm/releases/download/{tag}/{name}")
}

fn release_from_metadata(metadata: Metadata, installed: &Version) -> io::Result<Option<Release>> {
    if metadata.draft || metadata.prerelease {
        return Ok(None);
    }
    let version = stable_version(&metadata.tag_name)?;
    let notes_url = format!(
        "https://github.com/dikmri/rfm/releases/tag/{}",
        metadata.tag_name
    );
    if metadata.html_url != notes_url {
        return Err(invalid(tr!(
            "リリースの公開元が一致しません。",
            "The release source does not match the official repository."
        )));
    }
    if version <= *installed {
        return Ok(None);
    }
    let expected_exe = asset_url(&metadata.tag_name, ASSET_NAME);
    let expected_checksum = asset_url(&metadata.tag_name, CHECKSUM_NAME);
    let find_asset = |name: &str, expected_url: &str| -> io::Result<&Asset> {
        let mut assets = metadata.assets.iter().filter(|asset| asset.name == name);
        let asset = assets.next().ok_or_else(|| {
            invalid(tr!(
                "必要な更新ファイルがありません。",
                "A required release asset is missing."
            ))
        })?;
        if assets.next().is_some() || asset.browser_download_url != expected_url {
            return Err(invalid(tr!(
                "更新ファイルの名前または URL が不正です。",
                "The release asset name or URL is invalid."
            )));
        }
        Ok(asset)
    };
    let exe = find_asset(ASSET_NAME, &expected_exe)?;
    let checksum = find_asset(CHECKSUM_NAME, &expected_checksum)?;
    if exe.size == 0 || exe.size > MAX_EXE || checksum.size != CHECKSUM_LENGTH as u64 {
        return Err(invalid(tr!(
            "更新ファイルのサイズが不正です。",
            "The release asset size is invalid."
        )));
    }
    Ok(Some(Release {
        version: version.to_string(),
        tag: metadata.tag_name,
        notes_url,
        asset_url: expected_exe,
        checksum_url: expected_checksum,
        size: exe.size,
    }))
}

/// Only metadata is fetched here; asset access is deferred until user consent.
pub fn check_latest() -> io::Result<Option<Release>> {
    let budget = Budget {
        cancel: None,
        start: Instant::now(),
        limit: Duration::from_secs(120),
    };
    let session = Session::new()?;
    let response = session.get(LATEST_URL, &budget)?;
    if response.status == 404 {
        return Ok(None);
    }
    let body = response.bounded_body(MAX_METADATA, &budget)?;
    let metadata: Metadata = serde_json::from_slice(&body).map_err(|_| {
        invalid(tr!(
            "リリース情報の JSON が不正です。",
            "The release metadata JSON is invalid."
        ))
    })?;
    let installed = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| {
        invalid(tr!(
            "現在のバージョンが不正です。",
            "The installed version is invalid."
        ))
    })?;
    release_from_metadata(metadata, &installed)
}

fn parse_checksum(body: &[u8]) -> io::Result<[u8; 32]> {
    let fail = || {
        invalid(tr!(
            "更新ファイルの SHA-256 情報が不正です。",
            "The release SHA-256 checksum format is invalid."
        ))
    };
    if body.len() != CHECKSUM_LENGTH
        || &body[64..66] != b"  "
        || &body[66..body.len() - 1] != ASSET_NAME.as_bytes()
        || body.last() != Some(&b'\n')
    {
        return Err(fail());
    }
    let mut digest = [0u8; 32];
    let hex = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(fail()),
    };
    for (out, pair) in digest.iter_mut().zip(body[..64].as_chunks::<2>().0) {
        *out = (hex(pair[0])? << 4) | hex(pair[1])?;
    }
    Ok(digest)
}

struct Candidate<'a> {
    path: &'a Path,
    file: Option<File>,
    keep: bool,
}

impl Drop for Candidate<'_> {
    fn drop(&mut self) {
        // Windows requires closing our write handle before attempting deletion.
        drop(self.file.take());
        if !self.keep {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// Download into the caller's staging path only, never a path obtained from the server.
pub fn download(
    release: &Release,
    destination: &Path,
    cancel: &AtomicBool,
) -> io::Result<[u8; 32]> {
    let budget = Budget {
        cancel: Some(cancel),
        start: Instant::now(),
        limit: Duration::from_secs(600),
    };
    budget.check()?;
    stable_version(&release.tag)?;
    if release.tag.strip_prefix('v') != Some(release.version.as_str())
        || release.notes_url
            != format!("https://github.com/dikmri/rfm/releases/tag/{}", release.tag)
        || release.asset_url != asset_url(&release.tag, ASSET_NAME)
        || release.checksum_url != asset_url(&release.tag, CHECKSUM_NAME)
        || release.size == 0
        || release.size > MAX_EXE
    {
        return Err(invalid(tr!(
            "更新ファイルの情報が不正です。",
            "The release download information is invalid."
        )));
    }
    let session = Session::new()?;
    let checksum = session
        .get(&release.checksum_url, &budget)?
        .bounded_body(CHECKSUM_LENGTH, &budget)?;
    let expected = parse_checksum(&checksum)?;
    let response = session.get(&release.asset_url, &budget)?;
    response.require_ok()?;
    budget.check()?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut candidate = Candidate {
        path: destination,
        file: Some(file),
        keep: false,
    };
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = response.read(&mut buffer, &budget)?;
        if count == 0 {
            break;
        }
        if count as u64 > release.size - total {
            return Err(invalid(tr!(
                "更新ファイルが指定サイズを超えています。",
                "The update file exceeds its declared size."
            )));
        }
        candidate
            .file
            .as_mut()
            .unwrap()
            .write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        total += count as u64;
    }
    if total != release.size {
        return Err(invalid(tr!(
            "更新ファイルのサイズが一致しません。",
            "The update file size does not match."
        )));
    }
    let actual: [u8; 32] = hash.finalize().into();
    if actual != expected {
        return Err(invalid(tr!(
            "更新ファイルの SHA-256 が一致しません。",
            "The update file SHA-256 checksum does not match."
        )));
    }
    candidate.file.as_mut().unwrap().sync_all()?;
    budget.check()?;
    candidate.keep = true;
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> Metadata {
        Metadata {
            tag_name: "v0.3.0".to_owned(),
            html_url: "https://github.com/dikmri/rfm/releases/tag/v0.3.0".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                Asset {
                    name: ASSET_NAME.to_owned(),
                    browser_download_url: asset_url("v0.3.0", ASSET_NAME),
                    size: 1234,
                },
                Asset {
                    name: CHECKSUM_NAME.to_owned(),
                    browser_download_url: asset_url("v0.3.0", CHECKSUM_NAME),
                    size: CHECKSUM_LENGTH as u64,
                },
            ],
        }
    }

    #[test]
    fn latest_requires_strictly_newer_stable_version() {
        assert!(
            release_from_metadata(metadata(), &Version::new(0, 2, 0))
                .unwrap()
                .is_some()
        );
        for installed in [Version::new(0, 3, 0), Version::new(0, 4, 0)] {
            assert!(
                release_from_metadata(metadata(), &installed)
                    .unwrap()
                    .is_none()
            );
        }
        // Existing/older releases need no downloadable assets: source-only
        // historical releases must not produce a false update failure.
        let mut legacy = metadata();
        legacy.assets.clear();
        assert!(
            release_from_metadata(legacy, &Version::new(0, 3, 0))
                .unwrap()
                .is_none()
        );
        let mut preview = metadata();
        preview.prerelease = true;
        assert!(
            release_from_metadata(preview, &Version::new(0, 2, 0))
                .unwrap()
                .is_none()
        );
        let mut draft = metadata();
        draft.draft = true;
        assert!(
            release_from_metadata(draft, &Version::new(0, 2, 0))
                .unwrap()
                .is_none()
        );
        for tag in [
            "0.3.0",
            "v01.3.0",
            "v0.3.0-rc.1",
            "v0.3.0+build",
            "v0.3.0/evil",
        ] {
            assert!(stable_version(tag).is_err(), "{tag}");
        }
    }

    #[test]
    fn metadata_rejects_missing_duplicate_foreign_or_unbounded_assets() {
        let installed = Version::new(0, 2, 0);
        let mut missing = metadata();
        missing.assets.pop();
        assert!(release_from_metadata(missing, &installed).is_err());
        let mut duplicate = metadata();
        duplicate.assets.push(Asset {
            name: ASSET_NAME.to_owned(),
            browser_download_url: asset_url("v0.3.0", ASSET_NAME),
            size: 1,
        });
        assert!(release_from_metadata(duplicate, &installed).is_err());
        for url in [
            asset_url("v0.2.0", ASSET_NAME),
            format!("https://github.com/other/rfm/releases/download/v0.3.0/{ASSET_NAME}"),
            format!("{}?extra", asset_url("v0.3.0", ASSET_NAME)),
        ] {
            let mut wrong = metadata();
            wrong.assets[0].browser_download_url = url;
            assert!(release_from_metadata(wrong, &installed).is_err());
        }
        for size in [0, MAX_EXE + 1] {
            let mut wrong = metadata();
            wrong.assets[0].size = size;
            assert!(release_from_metadata(wrong, &installed).is_err());
        }
        let mut wrong = metadata();
        wrong.assets[1].size += 1;
        assert!(release_from_metadata(wrong, &installed).is_err());
    }

    #[test]
    fn redirect_policy_rejects_downgrades_and_authority_confusion() {
        for url in [
            "http://github.com/file",
            "https://github.com.evil.test/file",
            "https://github.com@evil.test/file",
            "https://evil.test@github.com/file",
            "https://github.com:443/file",
            "https://github.com\\evil/file",
            "https://github.com/file#fragment",
            "https://github.com/file\r\nInjected: true",
            "//evil.test/file",
            "https://%67ithub.com/file",
        ] {
            assert!(redirect_url(LATEST_URL, url).is_err(), "{url}");
        }
        for host in [
            "github.com",
            "api.github.com",
            "release-assets.githubusercontent.com",
            "objects.githubusercontent.com",
        ] {
            assert!(parse_url(&format!("https://{host}/file?signature=abc%2Fdef")).is_ok());
        }
        assert_eq!(
            redirect_url(LATEST_URL, "/safe/path").unwrap(),
            "https://api.github.com/safe/path"
        );
    }

    #[test]
    fn checksum_requires_exact_lowercase_digest_and_asset_filename() {
        let digest = "0123456789abcdef".repeat(4);
        let valid = format!("{digest}  {ASSET_NAME}\n");
        let parsed = parse_checksum(valid.as_bytes()).unwrap();
        assert_eq!(
            &parsed[..8],
            &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
        );
        for invalid in [
            valid.replace("abcdef", "ABCDEF"),
            valid.replace("  ", " *"),
            valid.replace(ASSET_NAME, "../rfm-windows-x86_64.exe"),
            valid.replace('\n', "\r\n"),
            valid.trim_end().to_owned(),
            format!("{valid}{valid}"),
            valid.replacen('0', "g", 1),
        ] {
            assert!(parse_checksum(invalid.as_bytes()).is_err());
        }
        assert!(parse_checksum(b"").is_err());
    }

    #[test]
    fn cancellation_precedes_network_and_file_creation() {
        let cancel = AtomicBool::new(true);
        let release = release_from_metadata(metadata(), &Version::new(0, 2, 0))
            .unwrap()
            .unwrap();
        let error =
            download(&release, Path::new("unused-update-candidate.exe"), &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
