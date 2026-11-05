//! Per-user Windows registration as a magnet and `.torrent` handler, as
//! `packaging/windows/README.md` asks: HKCU only (no administrator), explicit
//! (`--register` / `--unregister`), the executable path and the activation
//! argument both quoted, and uninstall removing only what is still ours.
//!
//! What is written, under `HKCU\Software`:
//! - `Classes\TemplarTorrent.Magnet` and `Classes\TemplarTorrent.Torrent`:
//!   ProgIDs whose `shell\open\command` is `"<exe>" "%1"`.
//! - `Classes\.torrent\OpenWithProgids\TemplarTorrent.Torrent`: offers the
//!   ProgID without taking `.torrent` from another program.
//! - `TemplarTorrent\Capabilities` plus `RegisteredApplications`: makes the
//!   app selectable in Settings > Apps > Default apps for `magnet` and
//!   `.torrent`.
//! - `Classes\magnet`: the protocol class itself, only when no other program
//!   owns it. Windows 10/11 protect the user's chosen default with a hashed
//!   `UserChoice` that no program may write, so when another handler is the
//!   default the user confirms TemplarTorrent in Settings; `--register` opens
//!   that page.
//!
//! The argument reaches `main` as one string and goes through
//! `activate::valid_activation`; it is never handed to a shell.
#![allow(unsafe_code)]

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};

pub const APP_NAME: &str = "TemplarTorrent";
pub const MAGNET_PROGID: &str = "TemplarTorrent.Magnet";
pub const TORRENT_PROGID: &str = "TemplarTorrent.Torrent";
const EXE_NAME: &str = "templartorrent.exe";

/// The real per-user root. Tests pass a throwaway key instead.
pub const USER_SOFTWARE: &str = "Software";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// `Classes\magnet` now runs this executable.
    pub owns_magnet_class: bool,
    /// The ProgID of the user's chosen magnet default, when one is set.
    pub magnet_user_choice: Option<String>,
}

impl Registration {
    /// True when clicking a magnet link will start TemplarTorrent.
    pub fn is_magnet_default(&self) -> bool {
        match &self.magnet_user_choice {
            Some(progid) => progid.eq_ignore_ascii_case(MAGNET_PROGID),
            None => self.owns_magnet_class,
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn status(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Create `subkey` under HKCU and set one string value (`None`: the default).
fn set_string(subkey: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let key_name = wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated name, out pointer to a local HKEY.
    status(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    })?;
    let value_name = name.map(wide);
    let data = wide(value);
    // SAFETY: `key` is open with KEY_SET_VALUE; `data` is REG_SZ including
    // its terminator, and the byte count matches the buffer.
    let result = status(unsafe {
        RegSetValueExW(
            key,
            value_name
                .as_ref()
                .map_or(std::ptr::null(), |name| name.as_ptr()),
            0,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    });
    // SAFETY: closes the key opened above exactly once.
    unsafe { RegCloseKey(key) };
    result
}

/// Read one string value under HKCU; `None` when the key or value is absent.
pub(crate) fn get_string(subkey: &str, name: Option<&str>) -> Option<String> {
    let key_name = wide(subkey);
    let value_name = name.map(wide);
    let name_ptr = value_name
        .as_ref()
        .map_or(std::ptr::null(), |name| name.as_ptr());
    let mut bytes = 0u32;
    // SAFETY: a size query with a null buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if code != ERROR_SUCCESS || bytes == 0 || bytes > 64 * 1024 {
        return None;
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut len = (buffer.len() * 2) as u32;
    // SAFETY: the buffer holds `len` bytes.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key_name.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if code != ERROR_SUCCESS {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..end]).ok()
}

/// Delete `subkey` itself and everything under it. With a non-null subkey
/// name, `RegDeleteTreeW` removes that key too; never call it with a parent.
fn delete_tree(subkey: &str) -> io::Result<()> {
    // A bad format string must never reach `Software`, `Software\Classes`
    // or `Software\Microsoft`: refuse any key shallower than two levels or
    // whose last component is one of those containers.
    let parts: Vec<&str> = subkey.split('\\').filter(|part| !part.is_empty()).collect();
    let container = parts.last().is_some_and(|last| {
        ["Software", "Classes", "Microsoft"]
            .iter()
            .any(|name| last.eq_ignore_ascii_case(name))
    });
    if parts.len() < 2 || container {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::text::t("register.shallow_key"),
        ));
    }
    let key_name = wide(subkey);
    // SAFETY: valid NUL-terminated name relative to HKCU.
    let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_name.as_ptr()) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status(code)
}

fn delete_value(subkey: &str, name: &str) -> io::Result<()> {
    let key_name = wide(subkey);
    let value_name = wide(name);
    // SAFETY: valid NUL-terminated names.
    let code =
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key_name.as_ptr(), value_name.as_ptr()) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status(code)
}

fn quoted_command(exe: &Path) -> io::Result<String> {
    let text = exe.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::text::t("register.exe_not_unicode"),
        )
    })?;
    if text.contains('"') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::text::t("register.exe_has_quote"),
        ));
    }
    Ok(format!("\"{text}\" \"%1\""))
}

/// A command line launches this application when its program is a file
/// named `templartorrent.exe`, wherever it was installed before.
fn is_ours(command: &str) -> bool {
    let program = command
        .trim()
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| command.split_whitespace().next().unwrap_or(""));
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(EXE_NAME))
}

fn write_progid(base: &str, progid: &str, label: &str, url: bool, exe: &Path) -> io::Result<()> {
    let key = format!("{base}\\Classes\\{progid}");
    set_string(&key, None, label)?;
    if url {
        set_string(&key, Some("URL Protocol"), "")?;
    }
    let exe_text = exe.to_string_lossy();
    set_string(
        &format!("{key}\\DefaultIcon"),
        None,
        &format!("\"{exe_text}\",0"),
    )?;
    set_string(
        &format!("{key}\\shell\\open\\command"),
        None,
        &quoted_command(exe)?,
    )
}

fn magnet_user_choice(base: &str) -> Option<String> {
    get_string(
        &format!(
            "{base}\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\magnet\\UserChoice"
        ),
        Some("ProgId"),
    )
}

/// Register `exe` under `base` (normally [`USER_SOFTWARE`]).
pub fn register_under(base: &str, exe: &Path) -> io::Result<Registration> {
    if !exe.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::text::t("register.exe_not_absolute"),
        ));
    }
    write_progid(base, MAGNET_PROGID, "URL:Magnet link", true, exe)?;
    write_progid(base, TORRENT_PROGID, "Torrent file", false, exe)?;
    set_string(
        &format!("{base}\\Classes\\.torrent\\OpenWithProgids"),
        Some(TORRENT_PROGID),
        "",
    )?;
    let app = format!("{base}\\Classes\\Applications\\{EXE_NAME}");
    set_string(&app, Some("FriendlyAppName"), APP_NAME)?;
    set_string(&format!("{app}\\SupportedTypes"), Some(".torrent"), "")?;
    set_string(
        &format!("{app}\\shell\\open\\command"),
        None,
        &quoted_command(exe)?,
    )?;
    let capabilities = format!("{base}\\{APP_NAME}\\Capabilities");
    set_string(&capabilities, Some("ApplicationName"), APP_NAME)?;
    set_string(
        &capabilities,
        Some("ApplicationDescription"),
        "BitTorrent client for magnet links and .torrent files",
    )?;
    set_string(
        &capabilities,
        Some("ApplicationIcon"),
        &format!("\"{}\",0", exe.to_string_lossy()),
    )?;
    set_string(
        &format!("{capabilities}\\URLAssociations"),
        Some("magnet"),
        MAGNET_PROGID,
    )?;
    set_string(
        &format!("{capabilities}\\FileAssociations"),
        Some(".torrent"),
        TORRENT_PROGID,
    )?;
    set_string(
        &format!("{base}\\RegisteredApplications"),
        Some(APP_NAME),
        &format!("Software\\{APP_NAME}\\Capabilities"),
    )?;
    // The protocol class itself: only when nobody else owns it.
    let magnet = format!("{base}\\Classes\\magnet");
    let current = get_string(&format!("{magnet}\\shell\\open\\command"), None);
    let owns_magnet_class = match current {
        Some(command) if !is_ours(&command) => false,
        _ => {
            write_progid(base, "magnet", "URL:Magnet link", true, exe)?;
            true
        }
    };
    notify_shell();
    Ok(Registration {
        owns_magnet_class,
        magnet_user_choice: magnet_user_choice(base),
    })
}

/// Remove what [`register_under`] wrote, leaving another program's magnet
/// class and the user's `UserChoice` untouched.
pub fn unregister_under(base: &str) -> io::Result<()> {
    delete_tree(&format!("{base}\\Classes\\{MAGNET_PROGID}"))?;
    delete_tree(&format!("{base}\\Classes\\{TORRENT_PROGID}"))?;
    delete_value(
        &format!("{base}\\Classes\\.torrent\\OpenWithProgids"),
        TORRENT_PROGID,
    )?;
    delete_tree(&format!("{base}\\Classes\\Applications\\{EXE_NAME}"))?;
    delete_tree(&format!("{base}\\{APP_NAME}"))?;
    delete_value(&format!("{base}\\RegisteredApplications"), APP_NAME)?;
    let magnet = format!("{base}\\Classes\\magnet");
    if get_string(&format!("{magnet}\\shell\\open\\command"), None).is_some_and(|c| is_ours(&c)) {
        delete_tree(&magnet)?;
    }
    notify_shell();
    Ok(())
}

pub fn register(exe: &Path) -> io::Result<Registration> {
    register_under(USER_SOFTWARE, exe)
}

pub fn unregister() -> io::Result<()> {
    unregister_under(USER_SOFTWARE)
}

/// Open Settings at this app's default-apps page, where the user can make
/// it the magnet and `.torrent` handler. No shell: explorer gets one URI.
pub fn open_default_apps_settings() -> io::Result<()> {
    std::process::Command::new("explorer.exe")
        .arg(format!(
            "ms-settings:defaultapps?registeredAppUser={APP_NAME}"
        ))
        .spawn()
        .map(|_| ())
}

fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    // SAFETY: the documented association-changed broadcast, no items.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway HKCU key: nothing under the user's real associations is
    /// read or written by these tests.
    struct Scratch(String);
    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(format!(
                "Software\\TemplarTorrentTest\\{tag}-{}",
                std::process::id()
            ))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = delete_tree(&self.0);
            // The shared parent goes only once empty: RegDeleteKeyW refuses
            // a key that still has subkeys (another test's scratch).
            let parent = wide("Software\\TemplarTorrentTest");
            // SAFETY: valid NUL-terminated name relative to HKCU.
            unsafe {
                windows_sys::Win32::System::Registry::RegDeleteKeyW(
                    HKEY_CURRENT_USER,
                    parent.as_ptr(),
                )
            };
        }
    }

    #[test]
    fn shallow_or_container_keys_are_never_deleted() {
        for key in [
            "Software",
            "Software\\Classes",
            "Software\\Microsoft",
            "Software\\TemplarTorrentTest\\x\\Classes",
            "",
        ] {
            assert!(delete_tree(key).is_err(), "{key}");
        }
    }

    #[test]
    fn command_lines_quote_the_program_and_the_argument() {
        let exe = Path::new("C:\\Program Files\\Templar Törrent\\templartorrent.exe");
        assert_eq!(
            quoted_command(exe).unwrap(),
            "\"C:\\Program Files\\Templar Törrent\\templartorrent.exe\" \"%1\""
        );
        assert!(is_ours(&quoted_command(exe).unwrap()));
        assert!(is_ours("\"D:\\old\\TemplarTorrent.EXE\" \"%1\""));
        assert!(!is_ours(
            "\"C:\\Program Files\\qBittorrent\\qbittorrent.exe\" \"%1\""
        ));
        assert!(!is_ours("notepad.exe %1"));
    }

    #[test]
    fn register_then_unregister_round_trips_under_a_scratch_key() {
        let scratch = Scratch::new("roundtrip");
        let exe = Path::new("C:\\Tools\\TT\\templartorrent.exe");
        let registration = register_under(&scratch.0, exe).unwrap();
        assert!(registration.owns_magnet_class);
        assert_eq!(registration.magnet_user_choice, None);
        assert!(registration.is_magnet_default());
        let command = |progid: &str| {
            get_string(
                &format!("{}\\Classes\\{progid}\\shell\\open\\command", scratch.0),
                None,
            )
        };
        let expected = "\"C:\\Tools\\TT\\templartorrent.exe\" \"%1\"";
        assert_eq!(command(MAGNET_PROGID).as_deref(), Some(expected));
        assert_eq!(command(TORRENT_PROGID).as_deref(), Some(expected));
        assert_eq!(command("magnet").as_deref(), Some(expected));
        assert_eq!(
            get_string(
                &format!("{}\\Classes\\magnet", scratch.0),
                Some("URL Protocol")
            )
            .as_deref(),
            Some("")
        );
        assert_eq!(
            get_string(
                &format!("{}\\{APP_NAME}\\Capabilities\\URLAssociations", scratch.0),
                Some("magnet")
            )
            .as_deref(),
            Some(MAGNET_PROGID)
        );
        assert_eq!(
            get_string(
                &format!("{}\\RegisteredApplications", scratch.0),
                Some(APP_NAME)
            )
            .as_deref(),
            Some("Software\\TemplarTorrent\\Capabilities")
        );
        // Registering again (a moved executable) updates in place.
        let moved = Path::new("D:\\Portable\\templartorrent.exe");
        assert!(register_under(&scratch.0, moved).unwrap().owns_magnet_class);
        assert_eq!(
            command("magnet").as_deref(),
            Some("\"D:\\Portable\\templartorrent.exe\" \"%1\"")
        );
        unregister_under(&scratch.0).unwrap();
        assert_eq!(command(MAGNET_PROGID), None);
        assert_eq!(command(TORRENT_PROGID), None);
        assert_eq!(command("magnet"), None);
        assert_eq!(
            get_string(
                &format!("{}\\RegisteredApplications", scratch.0),
                Some(APP_NAME)
            ),
            None
        );
    }

    #[test]
    fn another_programs_magnet_class_is_neither_taken_nor_removed() {
        let scratch = Scratch::new("foreign");
        let foreign = "\"C:\\Program Files\\Other\\other.exe\" \"%1\"";
        set_string(
            &format!("{}\\Classes\\magnet\\shell\\open\\command", scratch.0),
            None,
            foreign,
        )
        .unwrap();
        let exe = Path::new("C:\\Tools\\templartorrent.exe");
        let registration = register_under(&scratch.0, exe).unwrap();
        assert!(!registration.owns_magnet_class);
        assert!(!registration.is_magnet_default());
        unregister_under(&scratch.0).unwrap();
        assert_eq!(
            get_string(
                &format!("{}\\Classes\\magnet\\shell\\open\\command", scratch.0),
                None
            )
            .as_deref(),
            Some(foreign)
        );
    }

    #[test]
    fn a_user_choice_decides_the_default() {
        let user = |progid: Option<&str>| Registration {
            owns_magnet_class: true,
            magnet_user_choice: progid.map(str::to_string),
        };
        assert!(user(None).is_magnet_default());
        assert!(user(Some(MAGNET_PROGID)).is_magnet_default());
        assert!(!user(Some("qBittorrent.magnet")).is_magnet_default());
    }
}
