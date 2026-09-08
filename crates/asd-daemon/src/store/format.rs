use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{SessionState, StoreError};

pub(crate) const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreDocument {
    version: u32,
    sessions: Vec<SessionState>,
}

pub(crate) fn encode_document(states: &[SessionState]) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec_pretty(&StoreDocument {
        version: STORE_VERSION,
        sessions: states.to_vec(),
    })
    .map_err(|error| StoreError::ReadDocument {
        path: PathBuf::new(),
        detail: error.to_string(),
    })
}

pub(crate) fn decode_document(bytes: &[u8]) -> Result<Vec<SessionState>, StoreError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| StoreError::ReadDocument {
            path: PathBuf::new(),
            detail: error.to_string(),
        })?;
    let object = value.as_object().ok_or_else(|| StoreError::ReadDocument {
        path: PathBuf::new(),
        detail: "document root must be an object".into(),
    })?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| StoreError::ReadDocument {
            path: PathBuf::new(),
            detail: "document version must be an unsigned integer".into(),
        })?;
    let version = u32::try_from(version).map_err(|_| StoreError::FutureVersion(u32::MAX))?;
    if version > STORE_VERSION {
        return Err(StoreError::FutureVersion(version));
    }
    if version != STORE_VERSION {
        return Err(StoreError::ReadDocument {
            path: PathBuf::new(),
            detail: format!("unsupported session-store version {version}"),
        });
    }
    let sessions = object
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| StoreError::ReadDocument {
            path: PathBuf::new(),
            detail: "document sessions must be an array".into(),
        })?;
    let mut names = HashSet::with_capacity(sessions.len());
    let mut decoded = Vec::with_capacity(sessions.len());
    for (index, value) in sessions.iter().enumerate() {
        let state: SessionState =
            serde_json::from_value(value.clone()).map_err(|error| StoreError::InvalidSession {
                path: PathBuf::new(),
                index,
                detail: error.to_string(),
            })?;
        if !asd_proto::paths::is_valid_session_name(&state.name) {
            return Err(StoreError::InvalidSession {
                path: PathBuf::new(),
                index,
                detail: format!("invalid session name '{}'", state.name),
            });
        }
        if !names.insert(state.name.clone()) {
            return Err(StoreError::InvalidSession {
                path: PathBuf::new(),
                index,
                detail: format!("duplicate session name '{}'", state.name),
            });
        }
        decoded.push(state);
    }
    Ok(decoded)
}
