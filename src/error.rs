use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the database refused {action}: {source}")]
    Database {
        action: &'static str,
        #[source]
        source: sea_orm::DbErr,
    },

    #[error("{file} is not named <timestamp>-<slug>.up.sql")]
    Naming { file: String },

    #[error("{file} is not utf-8")]
    Encoding { file: String },

    #[error("two files resolve to {name}, so the ledger could not tell them apart")]
    Duplicate { name: String },

    #[error("{name} carries no down.sql, so it cannot be taken back")]
    Irreversible { name: String },

    #[error("{name} is recorded as applied but is not part of this build")]
    Unknown { name: String },

    #[error("{name} failed: {source}")]
    Failed {
        name: String,
        #[source]
        source: sea_orm::DbErr,
    },

    #[error("the migrations directory is not on disk at {path}; create only works in a checkout")]
    NotACheckout { path: String },

    #[error("{path} already exists")]
    Exists { path: String },

    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Usage(String),
}

impl Error {
    pub(crate) fn database(action: &'static str) -> impl FnOnce(sea_orm::DbErr) -> Self {
        move |source| Error::Database { action, source }
    }

    pub(crate) fn usage(message: impl fmt::Display) -> Self {
        Error::Usage(message.to_string())
    }
}
