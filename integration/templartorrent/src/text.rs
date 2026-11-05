//! Every piece of text the program shows, by key, from one JSON file.
//!
//! The single source is `integration/templartorrent/assets/messages.json`: a
//! flat object, one `"key": "text"` pair per line, keys sorted. It is
//! compiled into the binary, so the program always has every message. To
//! change wording without rebuilding, put an edited copy named
//! `messages.json` next to `templartorrent.exe` (or set
//! `TEMPLARTORRENT_MESSAGES` to a path). Each key it defines replaces the
//! built-in text; keys it leaves out keep the built-in text. A file that is
//! not valid JSON is ignored, and the problem is reported once on stderr.
//!
//! Placeholders are `{name}`. [`tf`] fills them; a placeholder with no
//! value is left as written, so a typo shows up on screen instead of
//! vanishing. `{{` and `}}` are literal braces.
//!
//! Code refers to a key with [`t`] or [`tf`] and a string literal (the
//! literal may start on the next line), which
//! `tests::every_key_in_the_source_exists_and_none_is_orphaned` checks against
//! the file in both directions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The built-in messages, the same file the override copies.
pub const BUILT_IN: &str = include_str!("../assets/messages.json");

/// Environment override for the messages file path.
pub const MESSAGES_ENV_VAR: &str = "TEMPLARTORRENT_MESSAGES";

static TABLE: OnceLock<HashMap<String, &'static str>> = OnceLock::new();

/// Why an override file was ignored: its path and the reason.
static OVERRIDE_PROBLEM: OnceLock<Option<(PathBuf, String)>> = OnceLock::new();

fn table() -> &'static HashMap<String, &'static str> {
    TABLE.get_or_init(|| {
        let mut table = parse(BUILT_IN).unwrap_or_else(|error| {
            // Caught by the unit tests; never ships.
            panic!("built-in messages.json is invalid: {error}")
        });
        let mut problem = None;
        if let Some(path) = override_path() {
            match std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse(&text))
            {
                Ok(overrides) => table.extend(overrides),
                Err(error) => {
                    eprintln!("templartorrent: ignoring {}: {error}", path.display());
                    problem = Some((path, error));
                }
            }
        }
        let _ = OVERRIDE_PROBLEM.set(problem);
        table
            .into_iter()
            .map(|(key, value)| (key, &*Box::leak(value.into_boxed_str())))
            .collect()
    })
}

fn override_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(MESSAGES_ENV_VAR).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("messages.json");
    path.is_file().then_some(path)
}

/// When an override `messages.json` exists but could not be used: its path
/// and why. The window shows it in the status bar at startup, since a GUI
/// launch has no console for the stderr line.
pub fn override_problem() -> Option<(&'static std::path::Path, &'static str)> {
    table();
    let (path, error) = OVERRIDE_PROBLEM.get()?.as_ref()?;
    Some((path.as_path(), error.as_str()))
}

/// The text for `key`. An unknown key returns the key itself, visibly.
pub fn t(key: &'static str) -> &'static str {
    table().get(key).copied().unwrap_or(key)
}

/// The text for `key` with each `{name}` replaced by its value.
pub fn tf(key: &'static str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
    fill(t(key), values)
}

/// Replace `{name}` placeholders; `{{` and `}}` are literal braces.
pub fn fill(template: &str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(index) = rest.find(['{', '}']) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if tail.starts_with('{')
            && let Some(end) = tail.find('}')
        {
            let name = &tail[1..end];
            match values.iter().find(|(key, _)| *key == name) {
                Some((_, value)) => {
                    use std::fmt::Write;
                    let _ = write!(out, "{value}");
                }
                None => out.push_str(&tail[..=end]),
            }
            rest = &tail[end + 1..];
        } else {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Parse a flat JSON object of string values. Hand-written so the client
/// needs no JSON dependency; it accepts exactly what the file needs: one
/// object, string keys, string values, standard escapes.
pub fn parse(text: &str) -> Result<HashMap<String, String>, String> {
    // Windows editors (Notepad, PowerShell 5's Set-Content) often save
    // UTF-8 with a byte-order mark; it is not part of the JSON.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    parser.skip_space();
    parser.expect(b'{')?;
    let mut table = HashMap::new();
    parser.skip_space();
    if parser.peek() == Some(b'}') {
        parser.at += 1;
    } else {
        loop {
            parser.skip_space();
            let key = parser.string()?;
            parser.skip_space();
            parser.expect(b':')?;
            parser.skip_space();
            let value = parser.string()?;
            if table.insert(key.clone(), value).is_some() {
                return Err(format!("duplicate key \"{key}\""));
            }
            parser.skip_space();
            match parser.next() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(parser.error("expected , or }")),
            }
        }
    }
    parser.skip_space();
    if parser.at != parser.bytes.len() {
        return Err(parser.error("text after the closing }"));
    }
    Ok(table)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    fn error(&self, what: &str) -> String {
        let line = self.bytes[..self.at.min(self.bytes.len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1;
        format!("line {line}: {what}")
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.next() == Some(byte) {
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", byte as char)))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            match self.next() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => match self.next() {
                    Some(b'"') => out.push(b'"'),
                    Some(b'\\') => out.push(b'\\'),
                    Some(b'/') => out.push(b'/'),
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'r') => out.push(b'\r'),
                    Some(b'b') => out.push(8),
                    Some(b'f') => out.push(12),
                    Some(b'u') => {
                        let unit = self.hex4()?;
                        let code = if (0xD800..0xDC00).contains(&unit) {
                            if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                                return Err(self.error("lone surrogate"));
                            }
                            let low = self.hex4()?;
                            if !(0xDC00..0xE000).contains(&low) {
                                return Err(self.error("bad surrogate pair"));
                            }
                            0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                        } else {
                            unit
                        };
                        let character =
                            char::from_u32(code).ok_or_else(|| self.error("bad \\u escape"))?;
                        let mut buffer = [0u8; 4];
                        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                    }
                    _ => return Err(self.error("bad escape")),
                },
                Some(byte) if byte < 0x20 => {
                    return Err(self.error("control character in a string"));
                }
                Some(byte) => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("short \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("bad \\u escape"))?;
        let value = u32::from_str_radix(text, 16).map_err(|_| self.error("bad \\u escape"))?;
        self.at += 4;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_file_parses_with_sorted_unique_keys_one_per_line() {
        let table = parse(BUILT_IN).expect("valid JSON");
        assert!(!table.is_empty());
        let keys: Vec<&str> = BUILT_IN
            .lines()
            .filter_map(|line| line.trim().strip_prefix('"'))
            .filter_map(|line| line.split_once("\": "))
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys.len(), table.len(), "one key per line");
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "keys are sorted so edits stay easy to find");
    }

    #[test]
    fn placeholders_fill_and_unknown_ones_stay_visible() {
        let count = 3;
        assert_eq!(
            fill("{count} peers, {missing} {{literal}}", &[("count", &count)]),
            "3 peers, {missing} {literal}"
        );
        assert_eq!(fill("no braces", &[]), "no braces");
        assert_eq!(fill("lone } and {", &[]), "lone } and {");
    }

    #[test]
    fn the_parser_handles_escapes_and_rejects_bad_files() {
        let table = parse("{\"a\": \"x\\\"y\\n\\u00e9\\ud83d\\ude00\", \"b\": \"\"}").unwrap();
        assert_eq!(table["a"], "x\"y\né😀");
        assert_eq!(table["b"], "");
        let with_bom = parse("\u{feff}{\"a\": \"x\"}").expect("a UTF-8 BOM is accepted");
        assert_eq!(with_bom["a"], "x");
        for bad in [
            "",
            "[]",
            "{\"a\": 1}",
            "{\"a\": \"x\",}",
            "{\"a\": \"x\"} trailing",
            "{\"a\": \"x\", \"a\": \"y\"}",
            "{\"a\": \"\\ud83d\"}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    /// Every `t("…")` / `tf("…")` key in the client source exists in the
    /// file, and every key in the file is used somewhere: the file is the
    /// whole set of messages, nothing more.
    #[test]
    fn every_key_in_the_source_exists_and_none_is_orphaned() {
        let table = parse(BUILT_IN).unwrap();
        let mut used = std::collections::BTreeSet::new();
        for source in crate::SOURCES {
            for call in ["t(", "tf("] {
                for (index, _) in source.match_indices(call) {
                    // `t(` must not be the tail of a longer identifier.
                    let before = source[..index].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    // rustfmt may put the key on the line after the `(`.
                    let rest = source[index + call.len()..].trim_start();
                    let Some(rest) = rest.strip_prefix('"') else {
                        continue;
                    };
                    if let Some(end) = rest.find('"') {
                        used.insert(rest[..end].to_string());
                    }
                }
            }
        }
        let missing: Vec<_> = used
            .iter()
            .filter(|key| !table.contains_key(*key))
            .collect();
        assert!(
            missing.is_empty(),
            "keys used in code but not in messages.json: {missing:?}"
        );
        let orphaned: Vec<_> = table.keys().filter(|key| !used.contains(*key)).collect();
        assert!(
            orphaned.is_empty(),
            "keys in messages.json no code uses: {orphaned:?}"
        );
    }
}
