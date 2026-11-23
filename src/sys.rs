//! Elevation, external tools (DISM) and other Windows odds and ends.

use crate::error::{Error, IoContext, Result};
use crate::model::Cancel;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_CANCELLED, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_HANDLE,
    STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// `CREATE_NO_WINDOW`: the child gets no console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// NUL-terminated UTF-16 copy of a string, for the Win32 `...W` functions.
pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

// --------------------------------------------------------------------------------------
// Elevation
// --------------------------------------------------------------------------------------

/// True when the process runs with full administrator rights (an elevated
/// token). Any failure to find out counts as "not elevated".
pub fn is_elevated() -> bool {
    // SAFETY: plain Win32 calls with valid out-pointers; the token handle is
    // closed before returning.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Quotes one argument so that the standard Windows command-line parser
/// (`CommandLineToArgvW`, the C runtime and Rust's `std::env::args`) gives it
/// back unchanged.
pub fn quote_arg(arg: &str) -> String {
    let plain = !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0B' | '"'));
    if plain {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote must be doubled, and the quote escaped.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes would otherwise escape the closing quote.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Joins arguments into one command-line tail, each quoted with [`quote_arg`].
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Starts a second, elevated copy of the running executable with `args`
/// (shows the UAC prompt). On success the caller should exit; the new
/// process is not waited for.
///
/// Returns `Error::Cancelled` when the user declines the prompt.
pub fn relaunch_elevated(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().ctx("Could not find the program's own path")?;
    let exe_w = wide(exe.as_os_str());
    let params_w = wide(join_args(args));
    // SAFETY: all strings are NUL-terminated and outlive the call.
    let (code, last_error) = unsafe {
        let instance = ShellExecuteW(
            HWND(std::ptr::null_mut()),
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(params_w.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        (instance.0 as isize, GetLastError())
    };
    // ShellExecute reports success with a value greater than 32.
    if code > 32 {
        Ok(())
    } else if last_error == ERROR_CANCELLED {
        Err(Error::Cancelled)
    } else {
        Err(Error::msg(format!(
            "Could not restart BootForge as administrator (ShellExecute code {code})."
        )))
    }
}

// --------------------------------------------------------------------------------------
// DISM
// --------------------------------------------------------------------------------------

/// `%SystemRoot%\System32\Dism.exe`, when it exists.
pub fn find_dism() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let dism = root.join("System32").join("Dism.exe");
    dism.is_file().then_some(dism)
}

/// The DISM arguments that split `wim` into `<out_dir>\install.swm`,
/// `install2.swm`, ... of at most `part_mb` MiB each.
///
/// Each element is already in command-line form (paths are wrapped in quotes
/// after the colon, the way the DISM documentation writes them) and must be
/// passed raw, separated by spaces. Fails when a path contains a quote.
pub fn dism_split_args(wim: &Path, out_dir: &Path, part_mb: u32) -> Result<Vec<String>> {
    let swm = out_dir.join("install.swm");
    let wim = wim.to_string_lossy().into_owned();
    let swm = swm.to_string_lossy().into_owned();
    if wim.contains('"') || swm.contains('"') {
        return Err(Error::invalid(
            "The path of the Windows image contains a quote character.",
        ));
    }
    if part_mb == 0 {
        return Err(Error::invalid("The split size must be at least 1 MB."));
    }
    Ok(vec![
        "/English".to_string(),
        "/Split-Image".to_string(),
        format!("/ImageFile:\"{wim}\""),
        format!("/SWMFile:\"{swm}\""),
        format!("/FileSize:{part_mb}"),
    ])
}

/// The percentage in a DISM progress line such as
/// `[===========               20.0%                          ]`.
pub fn parse_dism_percent(line: &str) -> Option<f32> {
    let end = line.find('%')?;
    let head = &line[..end];
    let start = head
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let value: f32 = head[start..].parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value)
}

/// Incremental parser for DISM's console output: feeds on raw bytes as they
/// arrive, reports progress percentages and remembers the last text lines
/// for error messages.
#[derive(Default)]
pub struct DismOutput {
    partial: Vec<u8>,
    tail: VecDeque<String>,
}

impl DismOutput {
    /// Number of trailing output lines kept for error reports.
    const KEEP: usize = 8;

    pub fn new() -> DismOutput {
        DismOutput::default()
    }

    /// Consumes more output. Returns the most recent percentage completed by
    /// these bytes, if any.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<f32> {
        let mut latest = None;
        for &b in bytes {
            // The progress bar is redrawn with bare carriage returns.
            if b == b'\r' || b == b'\n' {
                if let Some(p) = self.end_line() {
                    latest = Some(p);
                }
            } else {
                self.partial.push(b);
            }
        }
        latest
    }

    fn end_line(&mut self) -> Option<f32> {
        let raw = std::mem::take(&mut self.partial);
        let text = String::from_utf8_lossy(&raw);
        let line = text.trim();
        if line.is_empty() {
            return None;
        }
        if line.starts_with('[') {
            if let Some(p) = parse_dism_percent(line) {
                return Some(p);
            }
        }
        if self.tail.len() == Self::KEEP {
            self.tail.pop_front();
        }
        self.tail.push_back(line.to_string());
        None
    }

    /// The last few non-progress lines, oldest first, one per line.
    pub fn last_lines(&mut self) -> String {
        self.end_line();
        self.tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Orders `install.swm`, `install2.swm`, `install3.swm`, ... and drops
/// anything else. Pure helper for [`split_wim`].
pub fn sort_swm_parts(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut parts: Vec<(u32, String)> = names
        .into_iter()
        .filter_map(|name| {
            let lower = name.to_ascii_lowercase();
            let middle = lower.strip_prefix("install")?.strip_suffix(".swm")?;
            let index = if middle.is_empty() {
                1
            } else {
                middle.parse::<u32>().ok().filter(|n| *n >= 2)?
            };
            Some((index, name))
        })
        .collect();
    parts.sort();
    parts.into_iter().map(|(_, name)| name).collect()
}

fn swm_parts_in(dir: &Path) -> Vec<PathBuf> {
    let names = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    sort_swm_parts(names)
        .into_iter()
        .map(|n| dir.join(n))
        .collect()
}

fn remove_swm_parts(dir: &Path) {
    for part in swm_parts_in(dir) {
        let _ = std::fs::remove_file(part);
    }
}

/// Splits a Windows `install.wim` into `.swm` parts of at most `part_mb` MiB
/// with DISM, so that each part fits on FAT32.
///
/// Runs `Dism /English /Split-Image /ImageFile:<wim>
/// /SWMFile:<out_dir>\install.swm /FileSize:<part_mb>` without a console
/// window. `on_percent` receives DISM's progress (0 to 100). When `cancel` is
/// set the DISM process is killed and `Error::Cancelled` returned.
///
/// Returns the parts in order (`install.swm`, `install2.swm`, ...). On
/// failure or cancellation the parts written so far are deleted and the error
/// is an `Error::Msg` quoting DISM's last output lines. Existing
/// `install*.swm` files in `out_dir` are replaced.
///
/// DISM requires an elevated process.
pub fn split_wim(
    dism: &Path,
    wim: &Path,
    out_dir: &Path,
    part_mb: u32,
    cancel: &Cancel,
    on_percent: &mut dyn FnMut(f32),
) -> Result<Vec<PathBuf>> {
    let args = dism_split_args(wim, out_dir, part_mb)?;
    cancel.check()?;
    std::fs::create_dir_all(out_dir).ctx("Could not create the working folder")?;
    remove_swm_parts(out_dir);

    let mut command = Command::new(dism);
    for a in &args {
        command.raw_arg(a);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ctx("Could not start DISM")?;

    // Both pipes are drained by small threads so that DISM can never block on
    // a full pipe; the chunks arrive here through one channel.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let spawn_reader = |pipe: Option<Box<dyn Read + Send>>| {
        if let Some(mut pipe) = pipe {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
        }
    };
    spawn_reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    spawn_reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    drop(tx);

    let mut output = DismOutput::new();
    let mut exited_at: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                if let Some(p) = output.feed(&bytes) {
                    on_percent(p);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // Both pipes are closed: DISM is done.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            remove_swm_parts(out_dir);
            return Err(Error::Cancelled);
        }
        match exited_at {
            // A helper process may keep the pipes open a little longer than
            // DISM itself lives; do not wait for it indefinitely.
            Some(t) if t.elapsed() > Duration::from_secs(2) => break,
            Some(_) => {}
            None => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    exited_at = Some(Instant::now());
                }
            }
        }
    }
    let status = child.wait().ctx("Could not wait for DISM")?;
    if !status.success() {
        remove_swm_parts(out_dir);
        return Err(Error::msg(dism_failure_message(
            status.code(),
            &output.last_lines(),
        )));
    }
    let parts = swm_parts_in(out_dir);
    if parts.is_empty() {
        return Err(Error::msg(dism_failure_message(None, &output.last_lines())));
    }
    on_percent(100.0);
    Ok(parts)
}

/// The text shown when DISM fails. `code` is its exit code (`None` when it
/// reported success but produced no files).
pub fn dism_failure_message(code: Option<i32>, last_lines: &str) -> String {
    let mut s = match code {
        // ERROR_ELEVATION_REQUIRED
        Some(740) => "DISM needs administrator rights to split the Windows image.".to_string(),
        Some(c) => format!("DISM could not split the Windows image (exit code {c})."),
        None => "DISM finished but produced no install.swm files.".to_string(),
    };
    if !last_lines.trim().is_empty() {
        s.push_str("\nDISM said:\n");
        s.push_str(last_lines.trim());
    }
    s
}

// --------------------------------------------------------------------------------------
// Odds and ends
// --------------------------------------------------------------------------------------

/// Keeps Windows from putting the PC to sleep for as long as it lives (the
/// screen may still switch off). It belongs to the thread that created it.
pub struct KeepAwake {
    /// False when Windows refused the request.
    active: bool,
}

impl KeepAwake {
    pub fn new() -> KeepAwake {
        // SAFETY: plain flag-setting call without pointers.
        let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        KeepAwake {
            active: previous != EXECUTION_STATE(0),
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        KeepAwake::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: as above; back to the normal power behaviour.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

/// Free bytes available to the user on the volume holding `dir`. When `dir`
/// does not exist yet, its nearest existing parent is asked.
pub fn free_space(dir: &Path) -> Option<u64> {
    for candidate in dir.ancestors() {
        if candidate.as_os_str().is_empty() {
            continue;
        }
        let path = wide(candidate.as_os_str());
        let mut free = 0u64;
        // SAFETY: NUL-terminated path, valid out-pointer.
        let ok = unsafe { GetDiskFreeSpaceExW(PCWSTR(path.as_ptr()), Some(&mut free), None, None) };
        if ok.is_ok() {
            return Some(free);
        }
    }
    None
}

/// Attaches the process to the console of the program that started it, so
/// that a GUI-subsystem executable can print (used by `--selftest`). Output
/// that was redirected to a file or pipe is left alone. Does nothing when
/// there is no parent console.
pub fn attach_parent_console() {
    // SAFETY: console API calls with constant arguments; a handle opened here
    // is handed over to the process's standard handle table.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if std_handle_usable(which) {
                continue;
            }
            if let Ok(console) = CreateFileW(
                w!("CONOUT$"),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                HANDLE::default(),
            ) {
                let _ = SetStdHandle(which, console);
            }
        }
    }
}

unsafe fn std_handle_usable(which: STD_HANDLE) -> bool {
    match GetStdHandle(which) {
        Ok(h) => !h.0.is_null() && !h.is_invalid(),
        Err(_) => false,
    }
}

/// Opens a file or folder with the program Windows associates with it
/// (a folder opens in Explorer). Failures are ignored.
pub fn open_path(path: &Path) {
    let path = wide(path.as_os_str());
    // SAFETY: NUL-terminated strings that outlive the call.
    unsafe {
        ShellExecuteW(
            HWND(std::ptr::null_mut()),
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// The folder for BootForge's temporary files (`BootForge` under the user's
/// temp directory), created when missing.
pub fn work_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("BootForge");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits a command-line tail the way `CommandLineToArgvW` does for
    /// arguments after the program name (the documented 2N / 2N+1 backslash
    /// rules), to prove that quoting round-trips.
    fn parse_args(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut args = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let mut arg = String::new();
            let mut quoted = false;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' {
                    let mut n = 0;
                    while i < chars.len() && chars[i] == '\\' {
                        n += 1;
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == '"' {
                        arg.extend(std::iter::repeat_n('\\', n / 2));
                        if n % 2 == 1 {
                            arg.push('"');
                            i += 1;
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', n));
                    }
                } else if c == '"' {
                    quoted = !quoted;
                    i += 1;
                } else if !quoted && (c == ' ' || c == '\t') {
                    break;
                } else {
                    arg.push(c);
                    i += 1;
                }
            }
            args.push(arg);
        }
        args
    }

    #[test]
    fn quoting_basics() {
        assert_eq!(quote_arg("--selftest"), "--selftest");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("D:\\isos\\x.iso"), "D:\\isos\\x.iso");
        assert_eq!(quote_arg("D:\\my isos\\"), "\"D:\\my isos\\\\\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(join_args(&[]), "");
    }

    #[test]
    fn quoting_round_trips_through_the_windows_parser() {
        let cases: Vec<Vec<String>> = vec![
            vec!["--write".into(), "D:\\my isos\\win 11.iso".into()],
            vec!["".into(), "x".into(), "".into()],
            vec![
                "trailing\\".into(),
                "trailing space\\".into(),
                "a\\\\b c\\\\".into(),
            ],
            vec![
                "quote\"inside".into(),
                "\"".into(),
                "\\\"".into(),
                "\\\\\"x y".into(),
            ],
            vec!["tab\there".into(), "plain".into(), "ünï cødé ✓".into()],
        ];
        for args in cases {
            let line = join_args(&args);
            assert_eq!(parse_args(&line), args, "line: {line}");
        }
    }

    #[test]
    fn dism_arguments() {
        let args = dism_split_args(
            Path::new("E:\\sources\\install.wim"),
            Path::new("D:\\Temp Files\\BootForge"),
            3800,
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "/English",
                "/Split-Image",
                "/ImageFile:\"E:\\sources\\install.wim\"",
                "/SWMFile:\"D:\\Temp Files\\BootForge\\install.swm\"",
                "/FileSize:3800",
            ]
        );
        // Joined raw, the line parses back into exactly five arguments.
        let parsed = parse_args(&args.join(" "));
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[3], "/SWMFile:D:\\Temp Files\\BootForge\\install.swm");
        assert!(dism_split_args(Path::new("a\"b.wim"), Path::new("D:\\t"), 100).is_err());
        assert!(dism_split_args(Path::new("a.wim"), Path::new("D:\\t"), 0).is_err());
    }

    #[test]
    fn dism_percent_lines() {
        assert_eq!(
            parse_dism_percent("[===========               20.0%                          ]"),
            Some(20.0)
        );
        assert_eq!(
            parse_dism_percent("[==========================100.0%==========================]"),
            Some(100.0)
        );
        assert_eq!(
            parse_dism_percent("[                           0.5%"),
            Some(0.5)
        );
        assert_eq!(parse_dism_percent("7%"), Some(7.0));
        assert_eq!(parse_dism_percent("no percentage here"), None);
        assert_eq!(parse_dism_percent("100% sure about 250%"), Some(100.0));
        assert_eq!(parse_dism_percent("[ 250.0% ]"), None);
        assert_eq!(parse_dism_percent("[ % ]"), None);
        assert_eq!(parse_dism_percent("[ ..% ]"), None);
    }

    #[test]
    fn dism_output_stream() {
        let mut out = DismOutput::new();
        assert_eq!(
            out.feed(
                b"\r\nDeployment Image Servicing and Management tool\r\nVersion: 10.0\r\n\r\n"
            ),
            None
        );
        // A progress line split across two reads, redrawn with carriage returns.
        assert_eq!(out.feed(b"[=====        10."), None);
        assert_eq!(
            out.feed(b"0%            ]\r[======     12.5%     ]\r"),
            Some(12.5)
        );
        assert_eq!(out.feed(b"[====100.0%====]\r\n"), Some(100.0));
        assert_eq!(out.feed(b"The operation completed successfully.\r\n"), None);
        assert_eq!(
            out.last_lines(),
            "Deployment Image Servicing and Management tool\nVersion: 10.0\nThe operation completed successfully."
        );

        // Only the last lines are kept, and an unterminated final line counts.
        let mut out = DismOutput::new();
        for i in 0..20 {
            out.feed(format!("line {i}\n").as_bytes());
        }
        out.feed(b"Error: 740");
        let tail = out.last_lines();
        assert_eq!(tail.lines().count(), DismOutput::KEEP);
        assert!(tail.starts_with("line 13\n"), "{tail}");
        assert!(tail.ends_with("Error: 740"), "{tail}");
    }

    #[test]
    fn dism_failure_text() {
        let m = dism_failure_message(
            Some(740),
            "Error: 740\nElevated permissions are required to run DISM.",
        );
        assert!(m.contains("administrator rights"));
        assert!(m.ends_with("Elevated permissions are required to run DISM."));
        let m = dism_failure_message(Some(2), "");
        assert_eq!(m, "DISM could not split the Windows image (exit code 2).");
        assert!(dism_failure_message(None, " ").contains("no install.swm"));
    }

    #[test]
    fn swm_parts_are_ordered_numerically() {
        let names = [
            "install10.swm",
            "install2.swm",
            "INSTALL.SWM",
            "install3.swm",
            "install.wim",
            "installer.swm",
            "install1.swm",
            "boot.wim",
            "install02x.swm",
        ]
        .map(String::from);
        assert_eq!(
            sort_swm_parts(names),
            vec![
                "INSTALL.SWM",
                "install2.swm",
                "install3.swm",
                "install10.swm"
            ]
        );
    }

    #[test]
    fn split_wim_reports_a_missing_dism_and_honours_cancel() {
        let dir = std::env::temp_dir().join(format!("bootforge-sys-{}", std::process::id()));
        let missing = dir.join("no-such-dism.exe");
        let err = split_wim(
            &missing,
            Path::new("X:\\none.wim"),
            &dir,
            100,
            &Cancel::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io(..)), "{err:?}");
        let cancel = Cancel::new();
        cancel.cancel();
        let err = split_wim(
            &missing,
            Path::new("X:\\none.wim"),
            &dir,
            100,
            &cancel,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.is_cancelled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn environment_queries_do_not_fail() {
        // Not elevated in the test environment, but either answer is valid.
        let _ = is_elevated();
        let dir = work_dir();
        assert!(dir.is_dir());
        assert!(dir.ends_with("BootForge"));
        let free = free_space(&dir.join("not").join("created").join("yet"));
        assert!(free.is_some());
        assert_eq!(free_space(Path::new("")), None);
        if let Some(dism) = find_dism() {
            assert!(dism.is_file());
        }
    }

    #[test]
    fn keep_awake_is_accepted_and_released() {
        std::thread::spawn(|| {
            let awake = KeepAwake::new();
            assert!(awake.active());
            drop(awake);
            // Asking again after the release works the same way.
            assert!(KeepAwake::default().active());
        })
        .join()
        .unwrap();
    }
}
