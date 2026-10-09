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
