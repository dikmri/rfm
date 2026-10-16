use crate::settings::Terminal;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::process::Command;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

const PLACEHOLDER: [u16; 8] = [
    b'{' as u16,
    b'f' as u16,
    b'o' as u16,
    b'l' as u16,
    b'd' as u16,
    b'e' as u16,
    b'r' as u16,
    b'}' as u16,
];

pub fn validate(config: &Terminal) -> io::Result<()> {
    if config.executable.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "ターミナルの実行ファイルを指定してください。",
                "Specify a terminal executable."
            ),
        ));
    }
    for (name, value) in [
        (
            tr!("実行ファイル", "executable"),
            config.executable.as_os_str(),
        ),
        (tr!("引数", "arguments"), config.arguments.as_os_str()),
    ] {
        if value.encode_wide().any(|unit| matches!(unit, 0 | 10 | 13)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr_format!(
                    "ターミナルの{name}に NUL 文字や改行は指定できません。",
                    "The terminal {name} cannot contain NUL characters or line breaks."
                ),
            ));
        }
    }
    if let Some(name) = config.executable.file_name() {
        // Inspect the last dot directly so names such as `.cmd` cannot bypass validation.
        let name = name.as_encoded_bytes();
        if let Some(dot) = name.iter().rposition(|&byte| byte == b'.')
            && !name[dot + 1..].eq_ignore_ascii_case(b"exe")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!(
                    "ターミナルには .exe 実行ファイルを指定してください。.bat、.cmd、.lnk などは使用できません。",
                    "Specify an .exe terminal executable. Files such as .bat, .cmd, and .lnk are not supported."
                ),
            ));
        }
    }
    Ok(())
}

struct NativeArguments(*mut *mut u16);

impl Drop for NativeArguments {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0.cast());
        }
    }
}

fn replace_folder(argument: &[u16], folder: &[u16]) -> OsString {
    let Some(mut index) = argument
        .windows(PLACEHOLDER.len())
        .position(|part| part == PLACEHOLDER)
    else {
        return OsString::from_wide(argument);
    };
    let mut expanded = Vec::with_capacity(argument.len() - PLACEHOLDER.len() + folder.len());
    let mut remaining = argument;
    loop {
        expanded.extend_from_slice(&remaining[..index]);
        expanded.extend_from_slice(folder);
        remaining = &remaining[index + PLACEHOLDER.len()..];
        match remaining
            .windows(PLACEHOLDER.len())
            .position(|part| part == PLACEHOLDER)
        {
            Some(next) => index = next,
            None => break,
        }
    }
    expanded.extend_from_slice(remaining);
    OsString::from_wide(&expanded)
}

fn parse_arguments(arguments: &OsStr, folder: &[u16]) -> io::Result<Vec<OsString>> {
    // argv[0] has different quoting rules; only the dummy executable occupies it.
    let command_line: Vec<u16> = "rfm.exe "
        .encode_utf16()
        .chain(arguments.encode_wide())
        .chain(Some(0))
        .collect();
    let mut count = 0;
    let raw = unsafe { CommandLineToArgvW(command_line.as_ptr(), &mut count) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let owned = NativeArguments(raw);
    let mut parsed = Vec::with_capacity(count.saturating_sub(1) as usize);
    for index in 1..count as usize {
        // Each native argument is NUL-terminated and lives until LocalFree.
        let argument = unsafe { *owned.0.add(index) };
        let mut length = 0;
        unsafe {
            while *argument.add(length) != 0 {
                length += 1;
            }
            let argument = std::slice::from_raw_parts(argument, length);
            // Never parse again after inserting a folder containing spaces or shell metacharacters.
            parsed.push(replace_folder(argument, folder));
        }
    }
    Ok(parsed)
}

pub fn open(config: &Terminal, folder: &Path) -> io::Result<()> {
    validate(config)
        .and_then(|()| {
            if !folder.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    tr!("ターミナルで開くフォルダーには絶対パスが必要です。", "The folder to open in the terminal must be an absolute path."),
                ));
            }
            let folder_wide: Vec<u16> = folder.as_os_str().encode_wide().collect();
            if folder_wide.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    tr!("ターミナルで開くフォルダーに NUL 文字は指定できません。", "The folder to open in the terminal cannot contain NUL characters."),
                ));
            }
            let arguments = parse_arguments(&config.arguments, &folder_wide)?;
            // Command starts the configured executable directly, without an implicit shell.
            Command::new(&config.executable)
                .args(arguments)
                .current_dir(folder)
                .spawn()
                .map(|_| ())
        })
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                tr_format!(
                    "ターミナル「{}」を起動できませんでした。{error}\n［ファイル → 設定］のターミナル欄で実行ファイルと引数を確認してください。",
                    "Could not start terminal \"{}\". {error}\nCheck the executable and arguments under Terminal in File → Settings.",
                    config.executable.display()
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn windows_quotes_preserve_argument_boundaries_and_empty_arguments() {
        let parsed = parse_arguments(
            OsStr::new(r#""quote\"inside" plain "two words" "" "C:\tail\\""#),
            &[],
        )
        .unwrap();
        assert_eq!(
            parsed,
            [r#"quote"inside"#, "plain", "two words", "", "C:\\tail\\"].map(OsString::from)
        );
        assert!(parse_arguments(OsStr::new(""), &[]).unwrap().is_empty());
    }

    #[test]
    fn folder_expansion_preserves_utf16_and_cannot_create_arguments() {
        let mut folder: Vec<u16> = "C:\\日本語 & notes\\".encode_utf16().collect();
        folder.extend_from_slice(&[0xD800, b'\\' as u16]);
        let mut arguments: Vec<u16> =
            "--working-directory \"{folder}\" \"\" --label=pre{folder}{folder}post "
                .encode_utf16()
                .collect();
        arguments.push(0xDC00);
        let parsed = parse_arguments(&OsString::from_wide(&arguments), &folder).unwrap();
        let mut label: Vec<u16> = "--label=pre".encode_utf16().collect();
        label.extend_from_slice(&folder);
        label.extend_from_slice(&folder);
        label.extend("post".encode_utf16());
        assert_eq!(
            parsed,
            [
                OsString::from("--working-directory"),
                OsString::from_wide(&folder),
                OsString::new(),
                OsString::from_wide(&label),
                OsString::from_wide(&[0xDC00]),
            ]
        );
    }

    #[test]
    fn validation_rejects_scripts_and_control_characters_but_allows_exe_resolution() {
        for executable in [
            "",
            "terminal.bat",
            "terminal.CMD",
            ".cmd",
            "terminal.lnk",
            "terminal.exe.",
            "terminal.exe\n",
            "terminal\0.exe",
        ] {
            let config = Terminal {
                executable: PathBuf::from(executable),
                arguments: OsString::new(),
            };
            assert_eq!(
                validate(&config).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        for arguments in ["--flag\0other", "--flag\rnext", "--flag\nnext"] {
            let config = Terminal {
                executable: PathBuf::from("alacritty.exe"),
                arguments: OsString::from(arguments),
            };
            assert_eq!(
                validate(&config).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        for executable in [
            "alacritty",
            "terminal.EXE",
            "C:\\Program Files\\Alacritty\\alacritty.exe",
        ] {
            let config = Terminal {
                executable: PathBuf::from(executable),
                arguments: OsString::new(),
            };
            assert!(validate(&config).is_ok());
        }
    }
}
