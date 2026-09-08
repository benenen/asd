//! Immutable generations with last-known-valid overrides tracked by source path.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use asd_proto::ManifestDiagnostic;

use super::{Detector, ENGINE_VERSION, Manifest};

#[derive(Clone)]
pub struct DetectorSnapshot {
    pub generation: u64,
    pub detector: Arc<Detector>,
}

impl DetectorSnapshot {
    /// Delayed messages must never roll a session back to an older ruleset.
    pub fn apply(&mut self, newer: Self) -> bool {
        if newer.generation <= self.generation {
            return false;
        }
        *self = newer;
        true
    }
}

#[derive(Clone)]
pub struct DetectorStore {
    dir: PathBuf,
    overrides: Arc<BTreeMap<PathBuf, Manifest>>,
    snapshot: DetectorSnapshot,
}

pub struct PreparedDetectorReload {
    base_generation: u64,
    overrides: BTreeMap<PathBuf, Manifest>,
    detector: Arc<Detector>,
    pub diagnostics: Vec<ManifestDiagnostic>,
}

impl DetectorStore {
    pub fn load(dir: PathBuf) -> Self {
        let mut store = Self {
            dir,
            overrides: Arc::new(BTreeMap::new()),
            snapshot: DetectorSnapshot {
                generation: 0,
                detector: Arc::new(Detector::embedded()),
            },
        };
        let prepared = store.reload_candidate();
        for diagnostic in &prepared.diagnostics {
            tracing::warn!(path = %diagnostic.path, error = %diagnostic.message, "agent manifest load failed");
        }
        store.install(prepared);
        store
    }

    pub fn snapshot(&self) -> DetectorSnapshot {
        self.snapshot.clone()
    }

    /// Filesystem work occurs on a cloned store outside the Registry lock.
    pub fn reload_candidate(&self) -> PreparedDetectorReload {
        let mut diagnostics = Vec::new();
        let mut overrides = BTreeMap::new();
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                // An enumeration failure is not evidence that an override was
                // removed. Retain the entire previous set until a clean scan.
                let paths: std::io::Result<Vec<_>> = entries
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect();
                match paths {
                    Ok(mut paths) => {
                        paths.sort();
                        for path in paths
                            .into_iter()
                            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                        {
                            let previous = self.overrides.get(&path);
                            let parsed = std::fs::read_to_string(&path)
                                .map_err(|e| (None, e.to_string()))
                                .and_then(|text| parse_manifest(&text));
                            match parsed {
                                Ok(manifest) => {
                                    tracing::info!(agent = %manifest.id, version = %manifest.version, path = %path.display(), "agent manifest loaded");
                                    overrides.insert(path, manifest);
                                }
                                Err((manifest_id, message)) => {
                                    diagnostics.push(ManifestDiagnostic {
                                        path: path.to_string_lossy().into_owned(),
                                        manifest_id: previous
                                            .map(|manifest| manifest.id.clone())
                                            .or(manifest_id),
                                        message,
                                        retained_previous: previous.is_some(),
                                    });
                                    if let Some(previous) = previous {
                                        overrides.insert(path, previous.clone());
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => self.retain_scan_failure(error, &mut overrides, &mut diagnostics),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.retain_scan_failure(error, &mut overrides, &mut diagnostics),
        }
        let mut detector = Detector::embedded();
        for manifest in overrides.values() {
            detector
                .manifests
                .retain(|existing| existing.id != manifest.id);
            detector.manifests.push(manifest.clone());
        }
        PreparedDetectorReload {
            base_generation: self.snapshot.generation,
            overrides,
            detector: Arc::new(detector),
            diagnostics,
        }
    }

    fn retain_scan_failure(
        &self,
        error: std::io::Error,
        overrides: &mut BTreeMap<PathBuf, Manifest>,
        diagnostics: &mut Vec<ManifestDiagnostic>,
    ) {
        *overrides = self.overrides.as_ref().clone();
        diagnostics.push(ManifestDiagnostic {
            path: self.dir.to_string_lossy().into_owned(),
            manifest_id: None,
            message: error.to_string(),
            retained_previous: !overrides.is_empty(),
        });
    }

    /// Reject stale preparations defensively, even when the caller serializes
    /// preparation and installation. Only committed candidates advance time.
    pub fn install(&mut self, prepared: PreparedDetectorReload) -> DetectorSnapshot {
        if prepared.base_generation != self.snapshot.generation {
            return self.snapshot();
        }
        self.overrides = Arc::new(prepared.overrides);
        self.snapshot = DetectorSnapshot {
            generation: self
                .snapshot
                .generation
                .checked_add(1)
                .expect("detector generation exhausted"),
            detector: prepared.detector,
        };
        self.snapshot()
    }
}

fn parse_manifest(text: &str) -> Result<Manifest, (Option<String>, String)> {
    let manifest: Manifest = toml::from_str(text).map_err(|error| {
        let id = toml::from_str::<toml::Table>(text).ok().and_then(|table| {
            table
                .get("id")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        });
        (id, error.to_string())
    })?;
    if manifest.min_engine_version > ENGINE_VERSION {
        return Err((
            Some(manifest.id),
            format!(
                "manifest needs engine {}, running {ENGINE_VERSION}",
                manifest.min_engine_version
            ),
        ));
    }
    if manifest.id.trim().is_empty() {
        return Err((None, "manifest id must not be empty".into()));
    }
    Ok(manifest)
}
