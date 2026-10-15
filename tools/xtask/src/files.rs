//! Reading files with the path in the error.

use std::fs;
use std::path::Path;

use crate::R;

pub(crate) fn read_text(path: &Path) -> R<String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn a_read_error_names_the_file() {
        let dir = env::temp_dir().join(format!("xtask-read-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.rs");
        fs::write(&bad, b"\xff\xfe").unwrap();
        for path in [bad.clone(), dir.join("missing.rs")] {
            let err = read_text(&path).unwrap_err().to_string();
            assert!(err.contains(&path.display().to_string()), "{err}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
