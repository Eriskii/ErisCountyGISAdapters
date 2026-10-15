//! What a map view's box holds: the counties it crosses, the adapter each county's parcels are
//! read through, and those parcels as GeoJSON features with their normalized fields. A county
//! is read through an adapter verified for it, or else one whose coverage claims it; a county
//! with neither says why it shows nothing.

use crate::counties::{Counties, County};
use crate::geom::Bbox;
use crate::registry::{Registry, Standing, now};
use crate::runtime::Runtime;
use crate::spec::{Source, Spec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// The most parcels read from one source for one box.
pub const MAX_PARCELS: usize = 5000;
/// How long a kept answer serves the same box.
const KEEP_FOR_MS: u64 = 7 * 86_400_000;

/// A view's parcels and where each county it crosses stands.
#[derive(Debug, Default, Serialize)]
pub struct View {
    /// GeoJSON features: a parcel's normalized fields, `key` (the adapter's id and the parcel's
    /// own id, the same in every box), `adapter` and `publisher`.
    pub features: Vec<Value>,
    pub counties: Vec<CountyView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountyView {
    pub fips: String,
    /// Such as `Dane County, WI`.
    pub county: String,
    /// The adapter the county's parcels are read through, when it has one.
    pub source: Option<SourceView>,
    /// Why the county has no adapter, when the registry records it.
    pub unavailable: Option<UnavailableView>,
    /// Whether the box held more parcels than were read.
    pub more: bool,
    /// Why reading the county's parcels failed.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    pub adapter: String,
    pub publisher: String,
    /// Whether the adapter passed validation for this county, rather than claiming it.
    pub verified: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnavailableView {
    pub reason: String,
    pub note: String,
}

/// The counties whose shape meets `bbox`.
pub fn counties_in(counties: &Counties, bbox: Bbox) -> Vec<&County> {
    use geo::Intersects;
    let area = bbox.polygon();
    counties.list.iter().filter(|c| c.bbox.intersects(&bbox) && c.geometry.intersects(&area)).collect()
}

/// The adapter a county's parcels are read through: one verified for it, or else one whose
/// coverage claims it, with whether it was verified.
pub fn source_of<'r>(registry: &'r Registry, county: &County) -> Option<(&'r Spec, bool)> {
    if let Some(spec) = registry.index.get(&county.fips).into_iter().flatten().find_map(|id| registry.adapters.get(id))
    {
        return Some((spec, true));
    }
    registry
        .adapters
        .values()
        .find(|spec| spec.coverage.claims(&county.fips, |_| Some(county.state.clone())))
        .map(|spec| (spec, false))
}

/// What one source gave for the box.
#[derive(Default, Serialize, Deserialize)]
struct Read {
    at: u64,
    features: Vec<Value>,
    more: bool,
}

/// Reads the parcels of every county `bbox` crosses. Answers are kept in `keep` (by adapter and
/// box) and serve the same box for a week.
pub async fn read(
    runtime: &Runtime,
    registry: &Registry,
    counties: &Counties,
    bbox: Bbox,
    keep: Option<&Path>,
) -> View {
    let crossed = counties_in(counties, bbox);
    let mut sources: BTreeMap<String, &Spec> = BTreeMap::new();
    let mut view = View::default();
    for county in &crossed {
        let source = source_of(registry, county);
        let unavailable = match (source, registry.standing(&county.fips, now())) {
            (None, Standing::Unavailable(record)) => {
                Some(UnavailableView { reason: record.reason.name().to_owned(), note: record.note })
            }
            _ => None,
        };
        if let Some((spec, _)) = source {
            sources.insert(spec.id.clone(), spec);
        }
        view.counties.push(CountyView {
            fips: county.fips.clone(),
            county: format!("{}, {}", county.name, county.state),
            source: source.map(|(spec, verified)| SourceView {
                adapter: spec.id.clone(),
                publisher: spec.publisher.clone(),
                verified,
            }),
            unavailable,
            more: false,
            error: None,
        });
    }
    let reads =
        futures_util::future::join_all(sources.values().map(|spec| read_source(runtime, spec, bbox, keep))).await;
    let mut seen = HashSet::new();
    for (spec, read) in sources.values().zip(reads) {
        for county in view.counties.iter_mut().filter(|c| c.source.as_ref().is_some_and(|s| s.adapter == spec.id)) {
            match &read {
                Ok(read) => county.more = read.more,
                Err(error) => county.error = Some(error.clone()),
            }
        }
        for feature in read.map(|r| r.features).unwrap_or_default() {
            if seen.insert(feature["properties"]["key"].as_str().unwrap_or_default().to_owned()) {
                view.features.push(feature);
            }
        }
    }
    view
}

async fn read_source(runtime: &Runtime, spec: &Spec, bbox: Bbox, keep: Option<&Path>) -> Result<Read, String> {
    if matches!(spec.source, Source::BulkDownload { .. }) {
        return Err("this source is a bulk download, which the map does not fetch".into());
    }
    let kept = keep.map(|dir| dir.join(format!("{}-{:016x}.json", spec.id, box_hash(bbox))));
    if let Some(path) = &kept
        && let Ok(bytes) = std::fs::read(path)
        && let Ok(read) = serde_json::from_slice::<Read>(&bytes)
        && now().saturating_sub(read.at) < KEEP_FOR_MS
    {
        return Ok(read);
    }
    let fetch = runtime.fetch(spec, bbox, MAX_PARCELS, None).await.map_err(|e| format!("{e:#}"))?;
    let features = fetch.parcels.iter().map(|parcel| feature(spec, parcel)).collect();
    let read = Read { at: now(), features, more: fetch.more };
    if let Some(path) = kept
        && let Ok(text) = serde_json::to_vec(&read)
    {
        let _ = crate::registry::write_atomic(&path, &text);
    }
    Ok(read)
}

/// A parcel as a map feature: its normalized fields, its source, and a key that is the same
/// whichever box it was read in: the adapter's id and the parcel's APN, or a hash of its
/// attributes when it has none.
pub fn feature(spec: &Spec, parcel: &crate::runtime::Parcel) -> Value {
    let mut properties = serde_json::Map::new();
    for (field, value) in parcel.normalized(spec) {
        properties.insert(field.to_owned(), value);
    }
    let own = match properties.get("apn") {
        Some(Value::String(apn)) => apn.clone(),
        Some(other) => other.to_string(),
        None => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            Value::Object(parcel.attributes.clone()).to_string().hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        }
    };
    properties.insert("key".into(), json!(format!("{}:{own}", spec.id)));
    properties.insert("adapter".into(), json!(spec.id));
    properties.insert("publisher".into(), json!(spec.publisher));
    json!({"type": "Feature", "geometry": crate::geom::to_geojson(&parcel.geometry, 7), "properties": properties})
}

fn box_hash(bbox: Bbox) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for edge in bbox.0 {
        ((edge * 1e6).round() as i64).hash(&mut hasher);
    }
    hasher.finish()
}
