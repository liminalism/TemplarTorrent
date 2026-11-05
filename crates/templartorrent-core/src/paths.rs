//! Lexical checks only: NOT a filesystem sandbox and NOT a safe deletion implementation.
//! The storage adapter must also perform handle-relative no-follow/reparse-point checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    UnsafeComponent,
    TooLong,
    Duplicate,
}
impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PathError {}

pub fn validate_component(part: &str) -> Result<(), PathError> {
    if part.is_empty() {
        return Err(PathError::Empty);
    }
    if part.len() > 255 {
        return Err(PathError::TooLong);
    }
    if part == "."
        || part == ".."
        || part.ends_with('.')
        || part.ends_with(' ')
        || part.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return Err(PathError::UnsafeComponent);
    }
    let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem
                .chars()
                .last()
                .is_some_and(|c| matches!(c, '1'..='9' | '¹' | '²' | '³')));
    if reserved {
        return Err(PathError::UnsafeComponent);
    }
    Ok(())
}
#[derive(Debug, Clone)]
pub struct RelativeFile(Vec<String>);
impl RelativeFile {
    pub fn components(&self) -> &[String] {
        &self.0
    }
    /// Use the original bencoded component list; do not flatten then split a hostile path.
    pub fn new(parts: Vec<String>) -> Result<Self, PathError> {
        if parts.is_empty() {
            return Err(PathError::Empty);
        }
        if parts.len() > 64 || parts.iter().map(|s| s.len() + 1).sum::<usize>() > 4096 {
            return Err(PathError::TooLong);
        }
        for part in &parts {
            validate_component(part)?;
        }
        Ok(Self(parts))
    }
}
#[derive(Debug)]
pub struct RemovalIntent {
    /// Unchecked by default. Execute only after explicit confirmation and quiescence.
    pub delete_data: bool,
    pub manifest: Vec<RelativeFile>,
}
impl RemovalIntent {
    pub fn keep_data() -> Self {
        Self {
            delete_data: false,
            manifest: Vec::new(),
        }
    }
    /// No destructive filesystem methods are deliberately provided in this policy crate.
    pub fn confirmed(manifest: Vec<RelativeFile>) -> Result<Self, PathError> {
        let mut names = std::collections::HashSet::with_capacity(manifest.len());
        for file in &manifest {
            // Conservative ASCII-insensitive check. Production Windows must also detect
            // Unicode/NTFS-normalized aliases, file-vs-directory conflicts and hardlinks.
            let canonical = file.0.join("/").to_ascii_lowercase();
            if !names.insert(canonical) {
                return Err(PathError::Duplicate);
            }
        }
        Ok(Self {
            delete_data: true,
            manifest,
        })
    }
}

/// Windows verbatim path prefixes, spelled with escapes so no tool in the
/// chain can collapse the doubled backslashes.
const VERBATIM_UNC: &str = "\\\\?\\UNC\\";
const VERBATIM: &str = "\\\\?\\";

/// `std::fs::canonicalize`, minus the Windows verbatim prefix where dropping
/// it names the same file: a verbatim drive path becomes `C:` form and a
/// verbatim UNC path becomes a plain UNC path, when the result is short
/// enough for the legacy path limit and every component is one the Win32
/// path parser keeps unchanged. The plain form compares equal to the paths
/// users type and the UI shows. Unix output is untouched.
pub fn canonical_dir(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    Ok(strip_verbatim(canonical))
}

fn strip_verbatim(path: std::path::PathBuf) -> std::path::PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let Some(text) = path.to_str() else {
        return path;
    };
    // `plain` is the legacy form; `body` is the part made of components.
    let (plain, body) = if let Some(rest) = text.strip_prefix(VERBATIM_UNC) {
        (format!("\\\\{rest}"), rest)
    } else if let Some(rest) = text.strip_prefix(VERBATIM) {
        let drive = rest.as_bytes();
        if drive.len() < 3 || !drive[0].is_ascii_alphabetic() || &drive[1..3] != b":\\" {
            return path;
        }
        (rest.to_string(), &rest[3..])
    } else {
        return path;
    };
    // MAX_PATH less room for a file name below the directory.
    if plain.len() >= 248 {
        return path;
    }
    let safe = body
        .split('\\')
        .filter(|part| !part.is_empty())
        .all(|part| validate_component(part).is_ok());
    if safe {
        std::path::PathBuf::from(plain)
    } else {
        path
    }
}

#[cfg(test)]
mod canonical_tests {
    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_dropped_only_when_equivalent() {
        use super::strip_verbatim;
        use std::path::PathBuf;
        let strip = |text: &str| strip_verbatim(PathBuf::from(text));
        let same = |text: &str| assert_eq!(strip(text), PathBuf::from(text), "{text}");
        assert_eq!(
            strip("\\\\?\\C:\\Users\\me\\Down"),
            PathBuf::from("C:\\Users\\me\\Down")
        );
        assert_eq!(
            strip("\\\\?\\UNC\\nas\\share\\x"),
            PathBuf::from("\\\\nas\\share\\x")
        );
        // A trailing dot or a reserved name would change meaning without it.
        same("\\\\?\\C:\\a.\\b");
        same("\\\\?\\C:\\x\\NUL");
        same("\\\\?\\Volume{1}\\x");
        same("C:\\plain");
    }

    #[test]
    fn canonical_dir_resolves_an_existing_directory() {
        let dir = std::env::temp_dir();
        let canonical = super::canonical_dir(&dir).unwrap();
        assert!(canonical.is_absolute());
        assert!(canonical.is_dir());
        if cfg!(windows) {
            let text = canonical.to_string_lossy();
            assert!(!text.starts_with(super::VERBATIM), "{text}");
        }
    }
}
