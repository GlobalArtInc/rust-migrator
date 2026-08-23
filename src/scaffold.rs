use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const UP: &str = "-- write the change here\n";
const DOWN: &str = "-- write what takes the change back here\n";

/// A new pair, stamped in milliseconds the way typeorm stamped them, so the two
/// histories sort into one.
pub(crate) fn create(directory: &Path, slug: &str, stamp: i64) -> Result<Vec<PathBuf>> {
    if !directory.is_dir() {
        return Err(Error::NotACheckout {
            path: directory.display().to_string(),
        });
    }

    let slug = slug.trim().replace([' ', '_'], "-").trim_matches('-').to_lowercase();
    if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::usage("a slug is letters, digits and dashes"));
    }

    let mut written = Vec::new();
    for (suffix, body) in [("up", UP), ("down", DOWN)] {
        let path = directory.join(format!("{stamp}-{slug}.{suffix}.sql"));
        if path.exists() {
            return Err(Error::Exists {
                path: path.display().to_string(),
            });
        }

        std::fs::write(&path, body).map_err(|source| Error::Write {
            path: path.display().to_string(),
            source,
        })?;
        written.push(path);
    }

    Ok(written)
}
