//! The adapter registry on disk, which is what the repository publishes:
//!
//! ```text
//! registry/adapters/<id>.json        one spec each
//! registry/unavailable/<fips>.json   why a county has no adapter, and when to look again
//! registry/index.json                county FIPS → the adapters verified for it
//! ```
//!
//! Every write replaces its file whole, so a killed run leaves each file old or new.

use crate::spec::{Spec, Unavailable};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Writes `bytes` to `path` through a sibling file and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("partial");
    std::fs::write(&partial, bytes).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    counties: BTreeMap<String, Vec<String>>,
}

/// Where a county stands in the registry.
#[derive(Clone, Debug, PartialEq)]
pub enum Standing {
    Covered(Vec<String>),
    Unavailable(Unavailable),
    Open,
}

pub struct Registry {
    dir: PathBuf,
    pub adapters: BTreeMap<String, Spec>,
    pub unavailable: BTreeMap<String, Unavailable>,
    /// County FIPS → ids of adapters verified for it.
    pub index: BTreeMap<String, Vec<String>>,
}

impl Registry {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut adapters = BTreeMap::new();
        for spec in read_all::<Spec>(&dir.join("adapters"))? {
            adapters.insert(spec.id.clone(), spec);
        }
        let mut unavailable = BTreeMap::new();
        for record in read_all::<Unavailable>(&dir.join("unavailable"))? {
            unavailable.insert(record.fips.clone(), record);
        }
        let index = match std::fs::read(dir.join("index.json")) {
            Ok(bytes) => serde_json::from_slice::<Index>(&bytes).context("reading index.json")?.counties,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { dir: dir.to_owned(), adapters, unavailable, index })
    }

    pub fn standing(&self, fips: &str, at: u64) -> Standing {
        if let Some(ids) = self.index.get(fips).filter(|ids| !ids.is_empty()) {
            return Standing::Covered(ids.clone());
        }
        match self.unavailable.get(fips) {
            Some(record) if record.recheck_after > at => Standing::Unavailable(record.clone()),
            _ => Standing::Open,
        }
    }

    /// Saves a spec. A spec with a taken id must point at the same source; its coverage and
    /// verified counties are merged into the saved one.
    pub fn save_adapter(&mut self, mut spec: Spec) -> Result<Spec> {
        if let Some(existing) = self.adapters.get(&spec.id) {
            ensure!(
                existing.source == spec.source && existing.fields == spec.fields && existing.filter == spec.filter,
                "the id {} is taken by a different spec; choose another id",
                spec.id
            );
            use crate::spec::Coverage;
            spec.coverage = match (&existing.coverage, &spec.coverage) {
                (Coverage::Counties(a), Coverage::Counties(b)) => {
                    let mut all: Vec<String> = a.iter().chain(b).cloned().collect();
                    all.sort();
                    all.dedup();
                    Coverage::Counties(all)
                }
                (Coverage::States(_), _) => existing.coverage.clone(),
                (_, Coverage::States(_)) => spec.coverage.clone(),
            };
            let mut counties: Vec<String> =
                existing.verification.counties.iter().chain(&spec.verification.counties).cloned().collect();
            counties.sort();
            counties.dedup();
            spec.verification.counties = counties;
            if spec.verification.model.is_none() {
                spec.verification.model = existing.verification.model.clone();
                spec.verification.rung = existing.verification.rung;
            }
        }
        write_json(&self.dir.join("adapters").join(format!("{}.json", spec.id)), &spec)?;
        self.adapters.insert(spec.id.clone(), spec.clone());
        Ok(spec)
    }

    /// Records that `id` passed for `fips`, in the spec and the index; clears an unavailable
    /// record for the county.
    pub fn verified(&mut self, id: &str, fips: &str, at: u64) -> Result<()> {
        if let Some(spec) = self.adapters.get(id) {
            let mut spec = spec.clone();
            if !spec.verification.counties.iter().any(|f| f == fips) {
                spec.verification.counties.push(fips.to_owned());
                spec.verification.counties.sort();
            }
            spec.verification.verified_at = at;
            write_json(&self.dir.join("adapters").join(format!("{id}.json")), &spec)?;
            self.adapters.insert(id.to_owned(), spec);
        }
        let ids = self.index.entry(fips.to_owned()).or_default();
        if !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
        if self.unavailable.remove(fips).is_some() {
            let _ = std::fs::remove_file(self.dir.join("unavailable").join(format!("{fips}.json")));
        }
        self.write_index()
    }

    /// Records that `id` no longer works for `fips`.
    pub fn unverified(&mut self, id: &str, fips: &str) -> Result<()> {
        if let Some(ids) = self.index.get_mut(fips) {
            ids.retain(|i| i != id);
            if ids.is_empty() {
                self.index.remove(fips);
            }
        }
        if let Some(spec) = self.adapters.get(id) {
            let mut spec = spec.clone();
            spec.verification.counties.retain(|f| f != fips);
            write_json(&self.dir.join("adapters").join(format!("{id}.json")), &spec)?;
            self.adapters.insert(id.to_owned(), spec);
        }
        self.write_index()
    }

    pub fn mark_unavailable(&mut self, record: Unavailable) -> Result<()> {
        write_json(&self.dir.join("unavailable").join(format!("{}.json", record.fips)), &record)?;
        self.unavailable.insert(record.fips.clone(), record);
        Ok(())
    }

    fn write_index(&self) -> Result<()> {
        write_json(&self.dir.join("index.json"), &Index { version: 1, counties: self.index.clone() })
    }

    /// Adapters claiming `fips` that are not yet verified for it.
    pub fn unverified_claims(&self, fips: &str, state: &str) -> Vec<Spec> {
        let verified = self.index.get(fips);
        self.adapters
            .values()
            .filter(|s| s.coverage.claims(fips, |_| Some(state.to_owned())))
            .filter(|s| !verified.is_some_and(|ids| ids.contains(&s.id)))
            .cloned()
            .collect()
    }
}

fn read_all<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths.into_iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let bytes = std::fs::read(&path)?;
        out.push(serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?);
    }
    Ok(out)
}
