//! Shared data types — session info, host state. Kept minimal; the
//! full app state lives in Dioxus signals inside [`crate::app`].



#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub command: String,
    pub created_ms: u64,
    pub attached_clients: u32,
}

impl From<asd_proto::SessionInfo> for Session {
    fn from(s: asd_proto::SessionInfo) -> Self {
        Self {
            name: s.name,
            command: s.command,
            created_ms: s.created_ms,
            attached_clients: s.attached_clients,
        }
    }
}
