pub mod accounts;
pub mod auth;
pub mod client;
pub mod database;
pub(crate) mod process_lock;
pub mod protocol;
pub mod server;
pub mod terminal;
pub mod worker;

pub const ALPN: &[u8] = b"astra/1";
pub const PROTOCOL_VERSION: u32 = 1;
pub const SSHSIG_NAMESPACE: &str = "astra-shell-auth-v1";
