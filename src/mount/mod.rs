//! What the remote machine mounts from this one: the project's remote folder and its path mapping,
//! the SFTP servers behind the mounts, and the local sessions folder they serve.

pub mod sftp;
pub mod store;
pub mod transcripts;
pub mod warm;
pub mod workdir;
mod visible;
