//! Console path helpers. Console paths look like `HDD:\Content\file.xex`.

use crate::error::{Error, Result};

/// Normalizes a user-supplied path: `/` becomes `\`, and a bare drive (`HDD:`) gets a
/// trailing separator. Rejects characters that would break the quoted command syntax.
pub fn normalize(path: &str) -> Result<String> {
    if path.contains(['"', '\r', '\n', '\0']) {
        return Err(Error::InvalidArgument(format!("path contains a forbidden character: {path:?}")));
    }
    if !path.contains(':') {
        return Err(Error::InvalidArgument(format!(
            "console paths must start with a drive, e.g. HDD:\\ (got {path:?})"
        )));
    }
    let mut p = path.replace('/', "\\");
    if p.ends_with(':') {
        p.push('\\');
    }
    Ok(p)
}

/// Joins a directory and a file name with exactly one separator.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Splits `HDD:\a\b.xex` into (`HDD:\a\`, `b.xex`).
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('\\') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Wraps a value in quotes after checking it cannot escape them.
pub(crate) fn quote(value: &str) -> Result<String> {
    if value.contains(['"', '\r', '\n', '\0']) {
        return Err(Error::InvalidArgument(format!("value contains a forbidden character: {value:?}")));
    }
    Ok(format!("\"{value}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("HDD:").unwrap(), "HDD:\\");
        assert_eq!(normalize("HDD:/Content/a.xex").unwrap(), "HDD:\\Content\\a.xex");
        assert!(normalize("HDD:\\a\"\r\nreboot").is_err());
        assert!(normalize("relative/path").is_err());
    }

    #[test]
    fn join_and_split() {
        assert_eq!(join("HDD:\\", "a"), "HDD:\\a");
        assert_eq!(join("HDD:\\dir", "a"), "HDD:\\dir\\a");
        assert_eq!(split("HDD:\\dir\\default.xex"), ("HDD:\\dir\\", "default.xex"));
    }
}
