//! Versioned, durable session persistence and one-way legacy migration.

mod format;
mod legacy;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) use format::{decode_document, encode_document};
#[cfg(test)]
pub(crate) use legacy::{parse, serialize};

/// One session's entry in durable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_resume: Option<crate::agent_resume::AgentResumeRecord>,
}

/// The cwd of a live process, shared with the card command.
pub fn read_cwd(pid: u32) -> Option<PathBuf> {
    crate::platform::read_cwd(pid)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("session store uses unsupported future version {0}")]
    FutureVersion(u32),
    #[error("could not read session store {path}: {detail}")]
    ReadDocument { path: PathBuf, detail: String },
    #[error("invalid session entry {index} in session store {path}: {detail}")]
    InvalidSession {
        path: PathBuf,
        index: usize,
        detail: String,
    },
    #[error("failed to {operation} session store {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl StoreError {
    fn with_path(self, path: &Path) -> Self {
        match self {
            Self::ReadDocument { detail, .. } => Self::ReadDocument {
                path: path.to_path_buf(),
                detail,
            },
            Self::InvalidSession { index, detail, .. } => Self::InvalidSession {
                path: path.to_path_buf(),
                index,
                detail,
            },
            other => other,
        }
    }
}

/// A durable writer; the Registry owns the accepted session state.
pub struct SessionStore {
    json_path: PathBuf,
    legacy_path: PathBuf,
    generation: u64,
    dirty: bool,
}

/// The store and all records to attempt on daemon startup.
pub struct StoreLoad {
    pub store: SessionStore,
    pub sessions: Vec<SessionState>,
    pub diagnostics: Vec<String>,
}

impl SessionStore {
    /// Open JSON if present, otherwise migrate legacy TSV exactly once.
    pub fn open(json_path: PathBuf, legacy_path: PathBuf) -> Result<StoreLoad, StoreError> {
        let json_present = match fs::symlink_metadata(&json_path) {
            Ok(_) => true,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "inspect session store",
                    path: json_path,
                    source,
                });
            }
        };
        if json_present {
            let sessions = read_authoritative(&json_path)?;
            return Ok(StoreLoad {
                store: Self {
                    json_path,
                    legacy_path,
                    generation: 0,
                    dirty: false,
                },
                sessions,
                diagnostics: Vec::new(),
            });
        }
        let legacy_present = match fs::symlink_metadata(&legacy_path) {
            Ok(_) => true,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(StoreError::Io {
                    operation: "inspect legacy session list",
                    path: legacy_path,
                    source,
                });
            }
        };
        if !legacy_present {
            return Ok(StoreLoad {
                store: Self {
                    json_path,
                    legacy_path,
                    generation: 0,
                    dirty: false,
                },
                sessions: Vec::new(),
                diagnostics: Vec::new(),
            });
        }
        let legacy_text = fs::read_to_string(&legacy_path).map_err(|source| StoreError::Io {
            operation: "read legacy session list",
            path: legacy_path.clone(),
            source,
        })?;
        let (sessions, mut diagnostics) = legacy::parse_with_diagnostics(&legacy_text);
        let mut store = Self {
            json_path,
            legacy_path,
            generation: 0,
            dirty: false,
        };
        store.commit(&sessions)?;
        read_authoritative(&store.json_path)?;
        let migrated = store.legacy_path.with_extension("tsv.migrated");
        if let Err(error) = fs::rename(&store.legacy_path, &migrated) {
            diagnostics.push(format!(
                "failed to rename legacy session list {} to {}: {error}",
                store.legacy_path.display(),
                migrated.display()
            ));
        }
        Ok(StoreLoad {
            store,
            sessions,
            diagnostics,
        })
    }

    /// Commit a snapshot. Generation advances only after durable replacement.
    pub fn commit(&mut self, states: &[SessionState]) -> Result<u64, StoreError> {
        let candidate = encode_document(states)?;
        decode_document(&candidate)?;
        let (temporary_path, mut temporary) = crate::platform::create_private_temp(&self.json_path)
            .map_err(|source| StoreError::Io {
                operation: "create temporary session store",
                path: self.json_path.clone(),
                source,
            })?;
        let result = (|| {
            temporary
                .write_all(&candidate)
                .map_err(|source| StoreError::Io {
                    operation: "write temporary session store",
                    path: temporary_path.clone(),
                    source,
                })?;
            temporary.flush().map_err(|source| StoreError::Io {
                operation: "flush temporary session store",
                path: temporary_path.clone(),
                source,
            })?;
            temporary.sync_all().map_err(|source| StoreError::Io {
                operation: "sync temporary session store",
                path: temporary_path.clone(),
                source,
            })?;
            drop(temporary);
            crate::platform::replace_file(&temporary_path, &self.json_path).map_err(|source| {
                StoreError::Io {
                    operation: "replace session store",
                    path: self.json_path.clone(),
                    source,
                }
            })?;
            #[cfg(test)]
            if FAIL_NEXT_PARENT_SYNC.with(|fail| fail.replace(false)) {
                return Err(StoreError::Io {
                    operation: "sync session-store directory",
                    path: self.json_path.clone(),
                    source: std::io::Error::other("injected parent-sync failure after replacement"),
                });
            }
            crate::platform::sync_parent(&self.json_path).map_err(|source| StoreError::Io {
                operation: "sync session-store directory",
                path: self.json_path.clone(),
                source,
            })
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
        self.generation += 1;
        self.dirty = false;
        Ok(self.generation)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn retry_dirty(&mut self, states: &[SessionState]) -> Result<(), StoreError> {
        if self.dirty {
            self.commit(states)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn set_read_back_failure_for_test(enabled: bool) {
        READ_BACK_FAILURE_FOR_TEST.with(|value| value.set(enabled));
    }
}

#[cfg(test)]
std::thread_local! {
    static READ_BACK_FAILURE_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_NEXT_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn read_authoritative(path: &Path) -> Result<Vec<SessionState>, StoreError> {
    #[cfg(test)]
    if READ_BACK_FAILURE_FOR_TEST.with(std::cell::Cell::get) {
        return Err(StoreError::ReadDocument {
            path: path.to_path_buf(),
            detail: "injected test read-back failure".into(),
        });
    }
    let bytes = fs::read(path).map_err(|source| StoreError::Io {
        operation: "read session store",
        path: path.to_path_buf(),
        source,
    })?;
    decode_document(&bytes).map_err(|error| error.with_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("asd-store-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn one_state(name: &str) -> SessionState {
        SessionState {
            agent_resume: None,
            name: name.into(),
            cwd: Some(PathBuf::from(r"C:\tools")),
            command: Some("printf 'a\tb\n'".into()),
        }
    }

    #[test]
    fn version_one_round_trips_windows_paths_and_multiline_commands() {
        let states = vec![one_state("work")];
        assert_eq!(
            decode_document(&encode_document(&states).unwrap()).unwrap(),
            states
        );
    }

    #[test]
    fn future_version_is_rejected_without_falling_back_to_tsv() {
        assert!(matches!(
            decode_document(br#"{"version":2,"sessions":[]}"#),
            Err(StoreError::FutureVersion(2))
        ));
    }

    #[test]
    fn agent_metadata_uses_lowercase_kind_and_rejects_invalid_records() {
        let valid = r#"{"version":1,"sessions":[{"name":"agent","cwd":null,"command":"codex","agent_resume":{"kind":"codex","session_ref":"thr_123","reported_at_ms":42}}]}"#;
        let decoded = decode_document(valid.as_bytes()).unwrap();
        assert_eq!(decoded[0].agent_resume.as_ref().unwrap().reported_at_ms, 42);
        assert_eq!(
            decode_document(&encode_document(&decoded).unwrap()).unwrap(),
            decoded
        );
        for invalid in [
            valid.replace("thr_123", "-bad"),
            valid.replace("thr_123", "bad;cmd"),
            valid.replace("\"kind\":\"codex\"", "\"kind\":\"unknown\""),
            valid.replace("\"reported_at_ms\":42", "\"reported_at_ms\":-1"),
        ] {
            assert!(matches!(
                decode_document(invalid.as_bytes()),
                Err(StoreError::InvalidSession { index: 0, .. })
            ));
        }
    }

    #[test]
    fn malformed_session_reports_its_entry_index() {
        let json =
            br#"{"version":1,"sessions":[{"name":"good","cwd":null,"command":null},{"name":7}]}"#;
        assert!(matches!(
            decode_document(json),
            Err(StoreError::InvalidSession { index: 1, .. })
        ));
    }

    /// Catches accepting a record the registry would later reject or normalize.
    #[test]
    fn invalid_session_name_is_rejected_with_its_entry_index() {
        let json = br#"{"version":1,"sessions":[{"name":"not valid","cwd":null,"command":null}]}"#;
        assert!(matches!(
            decode_document(json),
            Err(StoreError::InvalidSession { index: 0, .. })
        ));
    }

    /// Catches restoring two records into one registry entry and then writing a
    /// normalized document that silently loses one of their distinct states.
    #[test]
    fn duplicate_session_name_is_rejected_with_the_later_entry_index() {
        let json = br#"{"version":1,"sessions":[{"name":"same","cwd":null,"command":null},{"name":"same","cwd":"/tmp","command":"echo duplicate"}]}"#;
        assert!(matches!(
            decode_document(json),
            Err(StoreError::InvalidSession { index: 1, .. })
        ));
    }

    #[test]
    fn json_is_authoritative_over_legacy_tsv() {
        let dir = test_dir("precedence");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        fs::write(&json, b"{not json").unwrap();
        fs::write(&legacy, "from-tsv\t/tmp\n").unwrap();
        assert!(matches!(
            SessionStore::open(json, legacy),
            Err(StoreError::ReadDocument { .. })
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Catches treating a path inspection error as an absent JSON store and
    /// migrating stale TSV over a path the daemon cannot safely inspect.
    #[test]
    fn only_not_found_json_path_allows_legacy_migration() {
        let dir = test_dir("metadata");
        let blocked = dir.join("not-a-directory");
        fs::write(&blocked, "not a directory").unwrap();
        let json = blocked.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        fs::write(&legacy, "from-tsv\t/tmp\n").unwrap();

        assert!(matches!(
            SessionStore::open(json, legacy.clone()),
            Err(StoreError::Io {
                operation: "inspect session store",
                ..
            })
        ));
        assert!(legacy.is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    /// A dangling authoritative symlink is present and must fail its JSON read;
    /// it is not permission to import the legacy backup.
    #[cfg(unix)]
    #[test]
    fn dangling_json_symlink_does_not_fall_back_to_legacy_tsv() {
        use std::os::unix::fs::symlink;

        let dir = test_dir("dangling-json");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        symlink(dir.join("missing-sessions.json"), &json).unwrap();
        fs::write(&legacy, "from-tsv\t/tmp\n").unwrap();

        assert!(matches!(
            SessionStore::open(json, legacy.clone()),
            Err(StoreError::Io {
                operation: "read session store",
                ..
            })
        ));
        assert!(legacy.is_file());
        assert!(!legacy.with_extension("tsv.migrated").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_imports_valid_legacy_lines_and_reports_nameless_ones() {
        let dir = test_dir("legacy");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        fs::write(&legacy, "keep\t/tmp\n\t/orphaned\n").unwrap();
        let loaded = SessionStore::open(json.clone(), legacy.clone()).unwrap();
        assert_eq!(
            loaded.sessions,
            vec![SessionState {
                agent_resume: None,
                name: "keep".into(),
                cwd: Some(PathBuf::from("/tmp")),
                command: None
            }]
        );
        assert!(
            loaded
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("line 2"))
        );
        assert!(json.is_file());
        assert!(legacy.with_extension("tsv.migrated").is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_read_back_failure_keeps_legacy_file_unrenamed() {
        let dir = test_dir("readback");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        fs::write(&legacy, "keep\t/tmp\n").unwrap();
        SessionStore::set_read_back_failure_for_test(true);
        assert!(SessionStore::open(json, legacy.clone()).is_err());
        assert!(legacy.is_file());
        assert!(!legacy.with_extension("tsv.migrated").exists());
        SessionStore::set_read_back_failure_for_test(false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_commit_does_not_advance_generation() {
        let dir = test_dir("generation");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        let mut loaded = SessionStore::open(json, legacy).unwrap();
        assert_eq!(loaded.store.commit(&[one_state("first")]).unwrap(), 1);
        loaded.store.json_path = dir.join("missing").join("sessions.json");
        assert!(loaded.store.commit(&[one_state("broken")]).is_err());
        assert_eq!(loaded.store.generation, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn later_commit_is_the_only_durable_generation() {
        let dir = test_dir("latest");
        let json = dir.join("sessions.json");
        let legacy = dir.join("sessions.tsv");
        let mut loaded = SessionStore::open(json.clone(), legacy).unwrap();
        assert_eq!(loaded.store.commit(&[one_state("old")]).unwrap(), 1);
        assert_eq!(loaded.store.commit(&[one_state("new")]).unwrap(), 2);
        assert_eq!(
            decode_document(&fs::read(json).unwrap()).unwrap(),
            vec![one_state("new")]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_round_trips_names_cwds_and_commands() {
        let states = vec![
            SessionState {
                name: "web".into(),
                agent_resume: None,
                cwd: Some(PathBuf::from("/home/me/proj")),
                command: Some("npm run dev".into()),
            },
            SessionState {
                name: "s0".into(),
                agent_resume: None,
                cwd: None,
                command: None,
            },
        ];
        assert_eq!(parse(&serialize(&states)), states);
    }

    #[test]
    fn legacy_command_survives_tabs_newlines_and_backslashes() {
        let states = vec![SessionState {
            agent_resume: None,
            name: "odd".into(),
            cwd: Some(PathBuf::from("/tmp")),
            command: Some("printf 'a\tb\n' && grep -E '\\d+' C:\\tools".into()),
        }];
        let text = serialize(&states);
        assert_eq!(text.lines().count(), 1);
        assert_eq!(parse(&text), states);
    }

    #[test]
    fn legacy_two_field_lines_parse_as_commandless() {
        assert_eq!(
            parse("web\t/home/me/proj\nwin\tC:\\tools\n"),
            vec![
                SessionState {
                    name: "web".into(),
                    agent_resume: None,
                    cwd: Some(PathBuf::from("/home/me/proj")),
                    command: None
                },
                SessionState {
                    name: "win".into(),
                    agent_resume: None,
                    cwd: Some(PathBuf::from("C:\\tools")),
                    command: None
                }
            ]
        );
    }

    #[test]
    fn read_cwd_zero_pid_is_none() {
        assert_eq!(read_cwd(0), None);
    }
}
