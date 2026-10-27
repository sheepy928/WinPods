//! Start with Windows via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! CONTRACT.
//!
//! Value name `WinPods`, data `"<exe>" --minimized`. `StartupApproved\Run\WinPods`
//! (first byte 0x03 = disabled by the user in Task Manager) is read for status only.
//! Reconcile only after taking the single-instance mutex; never write a Run value for
//! a temp copy. Only rewrite silently when the old target no longer exists.
//!
//! [`reconcile`] in detail (`config_enabled` is the config flag):
//!
//! | Run value                         | flag on                       | flag off |
//! |-----------------------------------|-------------------------------|----------|
//! | none                              | write (not for a temp copy)   | -        |
//! | this exe                          | normalise the command line    | remove   |
//! | another exe that no longer exists | rewrite (not for a temp copy) | remove   |
//! | another exe that exists           | keep ("Starts from <path>")   | keep     |
//!
//! `StartupApproved` is never written: "disabled in Task Manager" is the user's call.

use std::io;
use std::path::{Path, PathBuf};

use windows_registry::{CURRENT_USER, Key};

use crate::{paths, win};

/// `HKCU` sub key holding the Run values.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// `HKCU` sub key where Task Manager records enabled/disabled startup entries.
pub const STARTUP_APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// Name of our Run value.
pub const VALUE_NAME: &str = "WinPods";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartStatus {
    Disabled,
    Enabled {
        /// Executable the Run value points to.
        path: PathBuf,
        /// False when disabled in Task Manager (StartupApproved).
        approved: bool,
        /// True when `path` is this executable.
        is_current_exe: bool,
    },
}

/// Registry locations (under `HKCU`). Injectable so tests use throw-away keys and
/// never touch the real Run key.
#[derive(Debug, Clone)]
pub(crate) struct Keys {
    pub(crate) run: String,
    pub(crate) approved: String,
    pub(crate) value: String,
}

impl Keys {
    fn real() -> Self {
        Self {
            run: RUN_KEY.into(),
            approved: STARTUP_APPROVED_KEY.into(),
            value: VALUE_NAME.into(),
        }
    }
}

pub fn status() -> AutostartStatus {
    let current = paths::exe_path().ok();
    status_with(&Keys::real(), current.as_deref())
}

/// Write the Run value for `exe` (`"<exe>" --minimized`).
///
/// Refuses (`InvalidInput`) when `exe` lies in the temp folder: a copy opened from a
/// zip disappears once the zip viewer closes.
pub fn enable(exe: &Path) -> std::io::Result<()> {
    enable_guarded(&Keys::real(), exe)
}

/// The temp-copy guard of [`enable`], separated so tests can exercise it against
/// throw-away keys and never the real Run key.
fn enable_guarded(keys: &Keys, exe: &Path) -> std::io::Result<()> {
    if paths::is_in_temp_dir(exe) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to start a temporary copy with Windows (extract the zip first)",
        ));
    }
    enable_with(keys, exe)
}

/// Remove the Run value (no error if absent).
pub fn disable() -> std::io::Result<()> {
    disable_with(&Keys::real())
}

/// Make the registry agree with the config flag where safe (see module docs) and
/// return the resulting status.
pub fn reconcile(config_enabled: bool) -> AutostartStatus {
    let current = paths::exe_path().ok();
    let temp = current.as_deref().is_some_and(paths::is_in_temp_dir);
    reconcile_with(&Keys::real(), config_enabled, current.as_deref(), temp)
}

/// The Run value data for `exe`.
pub fn command_line(exe: &Path) -> io::Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable path is not valid Unicode",
        )
    })?;
    Ok(format!("\"{exe}\" --minimized"))
}

/// The executable named by a Run command line: the quoted part, or for an unquoted
/// line everything up to `.exe` (or the first space).
pub(crate) fn parse_command(data: &str) -> PathBuf {
    let s = data.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        return PathBuf::from(&rest[..end]);
    }
    let lower = s.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find(".exe") {
        let end = search + found + 4;
        if lower[end..].is_empty() || lower[end..].starts_with(char::is_whitespace) {
            return PathBuf::from(&s[..end]);
        }
        search = end;
    }
    PathBuf::from(s.split_whitespace().next().unwrap_or(""))
}

/// Map a registry error to `io::Error` (keeping Win32 codes such as NotFound).
fn reg_err(e: windows::core::Error) -> io::Error {
    win::to_io(e)
}

/// Open `HKCU\<path>` with `write` access when asked; `Ok(None)` when it is missing.
fn open_key(path: &str, write: bool) -> io::Result<Option<Key>> {
    let mut options = CURRENT_USER.options();
    options.read();
    if write {
        options.write();
    }
    match options.open(path).map_err(reg_err) {
        Ok(key) => Ok(Some(key)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The raw Run value data, `None` when absent.
fn read_run_value(keys: &Keys) -> io::Result<Option<String>> {
    let Some(key) = open_key(&keys.run, false)? else {
        return Ok(None);
    };
    match key.get_string(&keys.value).map_err(reg_err) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// False when Task Manager marked the entry disabled (odd first byte: 0x03 / 0x07).
fn read_approved(keys: &Keys) -> bool {
    let Ok(Some(key)) = open_key(&keys.approved, false) else {
        return true;
    };
    match key.get_value(&keys.value) {
        Ok(value) => value.first().is_none_or(|b| b & 0x01 == 0),
        Err(_) => true,
    }
}

pub(crate) fn status_with(keys: &Keys, current: Option<&Path>) -> AutostartStatus {
    let data = match read_run_value(keys) {
        Ok(Some(data)) => data,
        Ok(None) => return AutostartStatus::Disabled,
        Err(e) => {
            tracing::warn!("could not read the Run value: {e}");
            return AutostartStatus::Disabled;
        }
    };
    let path = parse_command(&data);
    if path.as_os_str().is_empty() {
        return AutostartStatus::Disabled;
    }
    let is_current_exe = current.is_some_and(|c| win::same_path(c, &path));
    AutostartStatus::Enabled {
        approved: read_approved(keys),
        is_current_exe,
        path,
    }
}

pub(crate) fn enable_with(keys: &Keys, exe: &Path) -> io::Result<()> {
    let data = command_line(exe)?;
    let key = CURRENT_USER.create(&keys.run).map_err(reg_err)?;
    key.set_string(&keys.value, &data).map_err(reg_err)?;
    tracing::info!("start with Windows enabled: {data}");
    Ok(())
}

pub(crate) fn disable_with(keys: &Keys) -> io::Result<()> {
    let Some(key) = open_key(&keys.run, true)? else {
        return Ok(());
    };
    match key.remove_value(&keys.value).map_err(reg_err) {
        Ok(()) => {
            tracing::info!("start with Windows disabled");
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// What [`reconcile`] should do to the Run value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Keep,
    Write,
    Remove,
}

/// Pure decision table of [`reconcile`] (see the module docs).
fn decide(
    status: &AutostartStatus,
    config_enabled: bool,
    command_is_canonical: bool,
    target_exists: bool,
    can_write: bool,
) -> Action {
    match (status, config_enabled) {
        (AutostartStatus::Disabled, true) if can_write => Action::Write,
        (AutostartStatus::Disabled, _) => Action::Keep,
        (AutostartStatus::Enabled { is_current_exe, .. }, true) => {
            let stale = !is_current_exe && !target_exists;
            let normalise = *is_current_exe && !command_is_canonical;
            if can_write && (stale || normalise) {
                Action::Write
            } else {
                Action::Keep
            }
        }
        (AutostartStatus::Enabled { is_current_exe, .. }, false) => {
            if *is_current_exe || !target_exists {
                Action::Remove
            } else {
                Action::Keep
            }
        }
    }
}

pub(crate) fn reconcile_with(
    keys: &Keys,
    config_enabled: bool,
    current: Option<&Path>,
    is_temp_copy: bool,
) -> AutostartStatus {
    let status = status_with(keys, current);
    let (target_exists, canonical) = match &status {
        AutostartStatus::Enabled { path, .. } => {
            let data = read_run_value(keys).ok().flatten();
            let expected = current.and_then(|c| command_line(c).ok());
            (path.exists(), data.is_some() && data == expected)
        }
        AutostartStatus::Disabled => (false, false),
    };
    let can_write = current.is_some() && !is_temp_copy;
    let action = decide(&status, config_enabled, canonical, target_exists, can_write);
    let result = match (action, current) {
        (Action::Write, Some(exe)) => enable_with(keys, exe),
        (Action::Remove, _) => disable_with(keys),
        _ => return status,
    };
    if let Err(e) = result {
        tracing::warn!("could not update start with Windows ({action:?}): {e}");
    }
    status_with(keys, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_suffix;
    use windows_registry::Type;

    /// Throw-away `HKCU\Software\WinPods-Test-<random>` keys, deleted on drop.
    struct TestKeys {
        root: String,
        keys: Keys,
    }

    impl TestKeys {
        fn new() -> Self {
            let root = format!(r"Software\WinPods-Test-{}", unique_suffix());
            let keys = Keys {
                run: format!(r"{root}\Run"),
                approved: format!(r"{root}\StartupApproved\Run"),
                value: VALUE_NAME.into(),
            };
            Self { root, keys }
        }

        fn set_raw(&self, data: &str) {
            let key = CURRENT_USER.create(&self.keys.run).unwrap();
            key.set_string(&self.keys.value, data).unwrap();
        }

        fn raw(&self) -> Option<String> {
            read_run_value(&self.keys).unwrap()
        }

        fn set_approved(&self, first: u8) {
            let key = CURRENT_USER.create(&self.keys.approved).unwrap();
            let mut data = [0u8; 12];
            data[0] = first;
            key.set_bytes(&self.keys.value, Type::Bytes, &data).unwrap();
        }
    }

    impl Drop for TestKeys {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.root);
        }
    }

    fn fake_exe(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"MZ").unwrap();
        path
    }

    #[test]
    fn real_keys_are_the_documented_ones() {
        let keys = Keys::real();
        assert_eq!(keys.run, r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert_eq!(
            keys.approved,
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
        );
        assert_eq!(keys.value, "WinPods");
    }

    #[test]
    fn command_lines_round_trip() {
        let exe = Path::new(r"C:\Program Files\WinPods\WinPods.exe");
        let cmd = command_line(exe).unwrap();
        assert_eq!(cmd, r#""C:\Program Files\WinPods\WinPods.exe" --minimized"#);
        assert_eq!(parse_command(&cmd), exe);
        assert_eq!(
            parse_command(r"C:\Program Files\WinPods\WinPods.exe --minimized"),
            exe
        );
        assert_eq!(
            parse_command(r"C:\Apps\WinPods.EXE"),
            Path::new(r"C:\Apps\WinPods.EXE")
        );
        assert_eq!(
            parse_command(r"C:\my.exe.files\WinPods.exe -x"),
            Path::new(r"C:\my.exe.files\WinPods.exe")
        );
        assert_eq!(
            parse_command(r#""C:\unterminated"#),
            Path::new(r"C:\unterminated")
        );
        assert_eq!(parse_command("  "), Path::new(""));
    }

    #[test]
    fn decision_table() {
        use Action::*;
        let disabled = AutostartStatus::Disabled;
        let current = AutostartStatus::Enabled {
            path: PathBuf::from(r"C:\a.exe"),
            approved: true,
            is_current_exe: true,
        };
        let other = AutostartStatus::Enabled {
            path: PathBuf::from(r"C:\b.exe"),
            approved: true,
            is_current_exe: false,
        };
        // (status, flag, canonical, target exists, can write) -> action
        assert_eq!(decide(&disabled, true, false, false, true), Write);
        assert_eq!(decide(&disabled, true, false, false, false), Keep);
        assert_eq!(decide(&disabled, false, false, false, true), Keep);
        assert_eq!(decide(&current, true, true, true, true), Keep);
        assert_eq!(decide(&current, true, false, true, true), Write);
        assert_eq!(decide(&current, true, false, true, false), Keep);
        assert_eq!(decide(&current, false, true, true, true), Remove);
        assert_eq!(decide(&other, true, false, true, true), Keep);
        assert_eq!(decide(&other, true, false, false, true), Write);
        assert_eq!(decide(&other, true, false, false, false), Keep);
        assert_eq!(decide(&other, false, false, true, true), Keep);
        assert_eq!(decide(&other, false, false, false, true), Remove);
    }

    #[test]
    fn enable_status_disable() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");

        assert_eq!(status_with(&t.keys, Some(&exe)), AutostartStatus::Disabled);
        // disabling with no key / no value is fine
        disable_with(&t.keys).unwrap();

        enable_with(&t.keys, &exe).unwrap();
        assert_eq!(
            t.raw().unwrap(),
            format!("\"{}\" --minimized", exe.display())
        );
        assert_eq!(
            status_with(&t.keys, Some(&exe)),
            AutostartStatus::Enabled {
                path: exe.clone(),
                approved: true,
                is_current_exe: true
            }
        );
        let other = fake_exe(dir.path(), "Other.exe");
        assert!(matches!(
            status_with(&t.keys, Some(&other)),
            AutostartStatus::Enabled {
                is_current_exe: false,
                ..
            }
        ));

        disable_with(&t.keys).unwrap();
        disable_with(&t.keys).unwrap();
        assert_eq!(t.raw(), None);
        assert_eq!(status_with(&t.keys, Some(&exe)), AutostartStatus::Disabled);
    }

    #[test]
    fn startup_approved_first_byte() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");
        enable_with(&t.keys, &exe).unwrap();
        let approved = |t: &TestKeys| match status_with(&t.keys, Some(&exe)) {
            AutostartStatus::Enabled { approved, .. } => approved,
            AutostartStatus::Disabled => panic!("expected enabled"),
        };
        assert!(approved(&t));
        t.set_approved(0x03);
        assert!(!approved(&t));
        t.set_approved(0x02);
        assert!(approved(&t));
        t.set_approved(0x07);
        assert!(!approved(&t));
    }

    #[test]
    fn test_keys_are_removed_on_drop() {
        let root;
        {
            let t = TestKeys::new();
            root = t.root.clone();
            t.set_raw("x");
            assert!(CURRENT_USER.open(&root).is_ok());
        }
        assert!(CURRENT_USER.open(&root).is_err());
    }

    #[test]
    fn reconcile_writes_when_enabled_and_missing() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");
        // temp copies never get a Run value
        assert_eq!(
            reconcile_with(&t.keys, true, Some(&exe), true),
            AutostartStatus::Disabled
        );
        assert_eq!(t.raw(), None);
        let status = reconcile_with(&t.keys, true, Some(&exe), false);
        assert!(matches!(
            status,
            AutostartStatus::Enabled {
                is_current_exe: true,
                ..
            }
        ));
        assert_eq!(t.raw(), Some(command_line(&exe).unwrap()));
    }

    #[test]
    fn reconcile_normalises_our_own_command_line() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");
        t.set_raw(&exe.display().to_string());
        reconcile_with(&t.keys, true, Some(&exe), false);
        assert_eq!(t.raw(), Some(command_line(&exe).unwrap()));
    }

    #[test]
    fn reconcile_rewrites_only_a_stale_target() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");

        // another copy that still exists: keep it
        let other = fake_exe(dir.path(), "Copy of WinPods.exe");
        t.set_raw(&command_line(&other).unwrap());
        let status = reconcile_with(&t.keys, true, Some(&exe), false);
        assert_eq!(
            status,
            AutostartStatus::Enabled {
                path: other.clone(),
                approved: true,
                is_current_exe: false
            }
        );
        assert_eq!(t.raw(), Some(command_line(&other).unwrap()));

        // a copy that is gone: rewrite to this exe, but not from a temp copy
        let gone = dir.path().join("Deleted").join("WinPods.exe");
        t.set_raw(&command_line(&gone).unwrap());
        reconcile_with(&t.keys, true, Some(&exe), true);
        assert_eq!(t.raw(), Some(command_line(&gone).unwrap()));
        reconcile_with(&t.keys, true, Some(&exe), false);
        assert_eq!(t.raw(), Some(command_line(&exe).unwrap()));
    }

    #[test]
    fn reconcile_removes_when_disabled_in_config() {
        let t = TestKeys::new();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path(), "WinPods.exe");

        enable_with(&t.keys, &exe).unwrap();
        assert_eq!(
            reconcile_with(&t.keys, false, Some(&exe), false),
            AutostartStatus::Disabled
        );
        assert_eq!(t.raw(), None);

        // stale entries are removed, entries for another existing copy are kept
        let gone = dir.path().join("gone.exe");
        t.set_raw(&command_line(&gone).unwrap());
        reconcile_with(&t.keys, false, Some(&exe), false);
        assert_eq!(t.raw(), None);
        let other = fake_exe(dir.path(), "Other.exe");
        t.set_raw(&command_line(&other).unwrap());
        reconcile_with(&t.keys, false, Some(&exe), false);
        assert_eq!(t.raw(), Some(command_line(&other).unwrap()));
    }

    #[test]
    fn enable_refuses_temp_copies() {
        // Uses throw-away keys: even if the guard regressed, the real Run key is safe.
        let t = TestKeys::new();
        let exe = std::env::temp_dir()
            .join("Temp1_WinPods.zip")
            .join("WinPods.exe");
        let e = enable_guarded(&t.keys, &exe).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(t.raw(), None, "nothing may be written for a temp copy");
    }
}
