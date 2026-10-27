//! Small Win32 helpers shared by the modules of this crate (private).
//!
//! Everything here is a thin, documented wrapper: wide-string conversion, error
//! mapping, long-path normalisation, `MoveFileExW`, retrying on sharing violations,
//! case-insensitive path comparison and atomic file replacement.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    GetLongPathNameW, MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    MoveFileExW,
};
use windows::core::PCWSTR;

/// A kernel handle closed on drop.
#[derive(Debug)]
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

// SAFETY: a kernel object handle is just an index into the process handle table; the
// Win32 functions used on these handles (events, mutexes) are thread-safe.
unsafe impl Send for OwnedHandle {}
// SAFETY: see above; shared references only pass the handle value to thread-safe APIs.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle and close it exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// NUL-terminated UTF-16 copy of `s` for `PCWSTR` parameters.
pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Map a `windows` error to an `io::Error`, keeping the Win32 code when the HRESULT
/// wraps one (`0x8007xxxx`) so `io::ErrorKind` matching keeps working.
pub(crate) fn to_io(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hr & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

/// True for the transient errors another process holding a file causes (antivirus,
/// indexer, a still-exiting process): access denied, sharing and lock violations.
pub(crate) fn is_sharing_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error().map(|c| c as u32),
        Some(c) if c == ERROR_ACCESS_DENIED.0
            || c == ERROR_SHARING_VIOLATION.0
            || c == ERROR_LOCK_VIOLATION.0
    )
}

/// Run `op` until it succeeds, fails with a non-sharing error, or `timeout` elapses.
pub(crate) fn retry_sharing<T>(
    timeout: Duration,
    mut op: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        match op() {
            Err(e) if is_sharing_error(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// `MoveFileExW(from, to)` with write-through; `replace` allows overwriting `to`.
pub(crate) fn move_file(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    let from_w = wide(from.as_os_str());
    let to_w = wide(to.as_os_str());
    let mut flags: MOVE_FILE_FLAGS = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe { MoveFileExW(PCWSTR(from_w.as_ptr()), PCWSTR(to_w.as_ptr()), flags) }.map_err(to_io)
}

/// Strip a `\\?\` (or `\\?\UNC\`) verbatim prefix so paths stay usable in registry
/// values, messages and comparisons.
pub(crate) fn strip_verbatim(path: &Path) -> PathBuf {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let s = String::from_utf16_lossy(&wide);
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Only drive-letter paths are safe to un-verbatim this way.
        if rest.as_bytes().get(1) == Some(&b':') {
            PathBuf::from(rest)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    }
}

/// Expand 8.3 short names (`HUANG~1`) to long names with `GetLongPathNameW` and drop
/// any verbatim prefix. When the path does not exist (yet), the longest existing
/// ancestor is expanded and the remaining components are appended unchanged.
pub(crate) fn long_path(path: &Path) -> PathBuf {
    if let Some(long) = existing_long_path(path) {
        return strip_verbatim(&long);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            long_path(parent).join(name)
        }
        _ => strip_verbatim(path),
    }
}

/// `GetLongPathNameW` for an existing path; `None` when it fails.
fn existing_long_path(path: &Path) -> Option<PathBuf> {
    let input = wide(path.as_os_str());
    // SAFETY: `input` is NUL-terminated; a `None` buffer asks for the required size.
    let needed = unsafe { GetLongPathNameW(PCWSTR(input.as_ptr()), None) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    // SAFETY: `buf` has room for `needed` UTF-16 units including the terminator.
    let written = unsafe { GetLongPathNameW(PCWSTR(input.as_ptr()), Some(&mut buf)) };
    if written == 0 || written as usize >= buf.len() {
        return None;
    }
    buf.truncate(written as usize);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

/// Normal path components, lower-cased, for case-insensitive comparison.
fn folded_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::CurDir => None,
            Component::RootDir => Some(String::from("\\")),
            Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().to_lowercase()),
            other => Some(other.as_os_str().to_string_lossy().to_lowercase()),
        })
        .collect()
}

/// Case-insensitive, component-wise "`path` is `dir` or inside it" (no I/O).
pub(crate) fn starts_with_ci(path: &Path, dir: &Path) -> bool {
    let p = folded_components(path);
    let d = folded_components(dir);
    !d.is_empty() && p.len() >= d.len() && p[..d.len()] == d[..]
}

/// Case-insensitive comparison of two paths after long-path normalisation.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    folded_components(&long_path(a)) == folded_components(&long_path(b))
}

/// `path` with `suffix` appended to the file name (`WinPods.exe` → `WinPods.exe.new`).
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// A process-unique file name next to `path` for temporary files.
pub(crate) fn unique_sibling(path: &Path, tag: &str) -> PathBuf {
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = format!(".{name}.{}.{n}.{tag}", std::process::id());
    match path.parent() {
        Some(dir) => dir.join(file),
        None => PathBuf::from(file),
    }
}

/// Write `data` to `path` atomically: a temp file in the same folder, flushed to disk,
/// then renamed over the original (`MoveFileExW` with replace + write-through,
/// retried briefly on sharing violations). The temp file is removed on failure.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = unique_sibling(path, "tmp");
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        retry_sharing(Duration::from_secs(2), || move_file(&tmp, path, true))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// True when a new file can be created (and removed again) in `dir`.
pub(crate) fn dir_writable(dir: &Path) -> bool {
    let probe = unique_sibling(&dir.join("write-test"), "probe");
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_io_keeps_win32_codes() {
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(2));
        assert_eq!(to_io(e).kind(), io::ErrorKind::NotFound);
        let e = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(32));
        assert!(is_sharing_error(&to_io(e)));
    }

    #[test]
    fn strip_verbatim_prefixes() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Apps\WinPods.exe")),
            PathBuf::from(r"C:\Apps\WinPods.exe")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\x.exe")),
            PathBuf::from(r"\\server\share\x.exe")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
    }

    #[test]
    fn prefix_and_equality_ignore_case() {
        let tmp = Path::new(r"C:\Users\Me\AppData\Local\Temp");
        assert!(starts_with_ci(
            Path::new(r"c:\users\me\appdata\local\temp\Temp1_x.zip\WinPods.exe"),
            tmp
        ));
        assert!(starts_with_ci(tmp, tmp));
        // A sibling folder that merely shares a string prefix is not inside.
        assert!(!starts_with_ci(
            Path::new(r"C:\Users\Me\AppData\Local\Temporary\WinPods.exe"),
            tmp
        ));
        assert!(!starts_with_ci(Path::new(r"D:\x"), Path::new("")));
        assert!(same_path(
            Path::new(r"C:\Nope\Missing\WinPods.EXE"),
            Path::new(r"c:\nope\missing\winpods.exe")
        ));
    }

    #[test]
    fn long_path_expands_short_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a long file name.txt");
        fs::write(&file, b"x").unwrap();
        let long = long_path(&file);
        assert!(same_path(&long, &file));
        assert!(long.ends_with("a long file name.txt"));
        // Missing tails keep their names below the expanded existing ancestor.
        let missing = dir.path().join("no such dir").join("missing.bin");
        let long_missing = long_path(&missing);
        assert!(long_missing.ends_with(Path::new("no such dir").join("missing.bin")));
        assert_eq!(
            long_missing.parent().unwrap().parent().unwrap(),
            long_path(dir.path())
        );
        assert_eq!(
            long_path(Path::new(r"Q:\definitely\not\here")),
            PathBuf::from(r"Q:\definitely\not\here")
        );
    }

    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("f.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn move_file_respects_replace_flag() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert!(move_file(&a, &b, false).is_err());
        move_file(&a, &b, true).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"a");
        assert!(!a.exists());
    }

    #[test]
    fn retry_sharing_gives_up_after_timeout() {
        let start = Instant::now();
        let mut calls = 0;
        let r: io::Result<()> = retry_sharing(Duration::from_millis(120), || {
            calls += 1;
            Err(io::Error::from_raw_os_error(32))
        });
        assert!(r.is_err());
        assert!(calls >= 2);
        assert!(start.elapsed() >= Duration::from_millis(120));
        // Non-sharing errors fail at once.
        let mut calls = 0;
        let r: io::Result<()> = retry_sharing(Duration::from_secs(5), || {
            calls += 1;
            Err(io::Error::from_raw_os_error(2))
        });
        assert!(r.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn suffix_and_writable_probe() {
        assert_eq!(
            with_suffix(Path::new(r"C:\a\WinPods.exe"), ".new"),
            PathBuf::from(r"C:\a\WinPods.exe.new")
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(dir_writable(dir.path()));
        assert!(!dir_writable(&dir.path().join("does-not-exist")));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
