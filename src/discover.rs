use std::path::{Path, PathBuf};

use include_dir::Dir;

use crate::error::{Error, Result};

/// A file pair from the migrations directory, named the way the ledger has always
/// named it: the slug written as a class, with its timestamp behind.
#[derive(Clone)]
pub struct Migration {
    pub stamp: i64,
    pub name: String,
    pub up: String,
    pub down: Option<String>,
}

/// Where the files come from. A service carries them inside itself; the image
/// that runs nothing but the migrator reads them off a mount, because it is one
/// image for every schema and cannot have been built with any of them.
pub(crate) enum Files {
    Embedded(&'static Dir<'static>),
    OnDisk(PathBuf),
}

impl Migration {
    /// `CREATE INDEX CONCURRENTLY` and friends refuse to run inside a transaction,
    /// so a file may ask to be run on its own by opening with the directive.
    pub fn transactional(&self) -> bool {
        !self
            .up
            .lines()
            .take_while(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with("--")
            })
            .any(|line| line.trim().trim_start_matches("--").trim() == DIRECTIVE)
    }

    pub fn checksum(&self) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(self.up.as_bytes());

        format!("{:x}", hasher.finalize())
    }
}

pub(crate) const DIRECTIVE: &str = "migrator:no-transaction";

/// Reads the directory the binary carries. Files that are not `.up.sql` are passed
/// over, so a README next to the migrations is no trouble, but a `.down.sql` with
/// nothing in front of it is: it means the pair was split.
pub(crate) fn all(files: &Files) -> Result<Vec<Migration>> {
    let mut found = match files {
        Files::Embedded(dir) => embedded(dir)?,
        Files::OnDisk(path) => on_disk(path)?,
    };

    // typeorm ordered by the stamp alone and let the directory listing settle the
    // ties, which happened to be by name; two files that resolve to one name are
    // the real trouble, because the ledger cannot tell them apart
    found.sort_by(|left, right| left.stamp.cmp(&right.stamp).then_with(|| left.name.cmp(&right.name)));

    for pair in found.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(Error::Duplicate {
                name: pair[0].name.clone(),
            });
        }
    }

    Ok(found)
}

fn embedded(dir: &'static Dir<'static>) -> Result<Vec<Migration>> {
    let mut found = Vec::new();

    for file in dir.files() {
        let Some(file_name) = file.path().file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = pair_of(file_name, |down| dir.get_file(dir.path().join(down)).is_some())? else {
            continue;
        };

        found.push(read(
            file_name,
            stem,
            || {
                file.contents_utf8().map(str::to_owned).ok_or(Error::Encoding {
                    file: file_name.to_owned(),
                })
            },
            || {
                dir.get_file(dir.path().join(format!("{stem}.down.sql")))
                    .and_then(|file| file.contents_utf8())
                    .map(str::to_owned)
            },
        )?);
    }

    Ok(found)
}

fn on_disk(directory: &Path) -> Result<Vec<Migration>> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(directory).map_err(|_| Error::NoDirectory {
        path: directory.display().to_string(),
    })?;

    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = pair_of(&file_name, |down| directory.join(down).is_file())? else {
            continue;
        };

        let stem = stem.to_owned();
        found.push(read(
            &file_name,
            &stem,
            || {
                std::fs::read_to_string(entry.path()).map_err(|_| Error::Encoding {
                    file: file_name.clone(),
                })
            },
            || std::fs::read_to_string(directory.join(format!("{stem}.down.sql"))).ok(),
        )?);
    }

    Ok(found)
}

/// Files that are not `.up.sql` are passed over, so a README next to the
/// migrations is no trouble. A `.down.sql` with nothing in front of it is: it
/// means the pair was split.
fn pair_of<'a>(file_name: &'a str, has: impl Fn(&str) -> bool) -> Result<Option<&'a str>> {
    match file_name.strip_suffix(".up.sql") {
        Some(stem) => Ok(Some(stem)),
        None => {
            if file_name.ends_with(".down.sql") {
                let up = file_name.replace(".down.sql", ".up.sql");
                if !has(&up) {
                    return Err(Error::Naming { file: up });
                }
            }

            Ok(None)
        }
    }
}

fn read(
    file_name: &str,
    stem: &str,
    up: impl FnOnce() -> Result<String>,
    down: impl FnOnce() -> Option<String>,
) -> Result<Migration> {
    let (stamp, slug) = stem.split_once('-').ok_or_else(|| Error::Naming {
        file: file_name.to_owned(),
    })?;
    let stamp: i64 = stamp.parse().map_err(|_| Error::Naming {
        file: file_name.to_owned(),
    })?;

    Ok(Migration {
        stamp,
        name: format!("{}{stamp}", class_of(slug)),
        up: up()?,
        down: down(),
    })
}

/// The ledger holds the name of the class typeorm generated, so the slug is read
/// back into one: every dash-separated part gets a capital, and nothing else is
/// touched - a slug already written as a class comes back as it was.
fn class_of(slug: &str) -> String {
    let mut class = String::with_capacity(slug.len());

    for part in slug.split(['-', '_']) {
        let mut characters = part.chars();
        if let Some(first) = characters.next() {
            class.extend(first.to_uppercase());
            class.push_str(characters.as_str());
        }
    }

    class
}

#[cfg(test)]
mod tests {
    use super::class_of;

    #[test]
    fn a_kebab_slug_is_read_as_the_class_typeorm_wrote() {
        assert_eq!(class_of("init"), "Init");
        assert_eq!(class_of("workspace-member"), "WorkspaceMember");
        assert_eq!(class_of("plan-external-price-ids"), "PlanExternalPriceIds");
    }

    #[test]
    fn a_slug_already_written_as_a_class_is_left_alone() {
        assert_eq!(class_of("AddEventStore"), "AddEventStore");
        assert_eq!(class_of("AlignGuildIdsWithProduction"), "AlignGuildIdsWithProduction");
    }
}
