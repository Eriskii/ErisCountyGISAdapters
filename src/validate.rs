//! Proves a spec works for a county by running it: enough parcels near a point inside the
//! county, polygons in longitude/latitude that fall where the county is, mapped fields that
//! exist and are filled in, and paging that returns every feature.

use crate::counties::County;
use crate::geom::Bbox;
use crate::runtime::{Fetch, Runtime};
use crate::spec::{Source, Spec};
use geo::{Centroid, Validation};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Fewest parcels a sample must return.
pub const MIN_FEATURES: usize = 5;
/// Most parcels read for one sample.
const MAX_FEATURES: usize = 2000;
/// Sample box sizes in degrees, tried in turn until one has enough parcels.
const SAMPLE_SIZES: [f64; 3] = [0.01, 0.04, 0.12];
/// Share of polygons that must be valid.
const VALID_SHARE: f64 = 0.9;
/// Share of centroids that must lie near the sample box, and inside the county.
const NEAR_SHARE: f64 = 0.9;
const INSIDE_SHARE: f64 = 0.5;
/// Share of parcels each mapped field must be filled in for.
const FILL_SHARE: f64 = 0.5;

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub spec: String,
    pub fips: String,
    pub county: String,
    pub passed: bool,
    pub checks: Vec<Check>,
    pub sample_bbox: Option<Bbox>,
    pub features: usize,
    pub pages: usize,
    /// Share of parcels with each mapped field filled in.
    pub fill: BTreeMap<String, f64>,
    /// Attribute names the source has, to map fields from.
    pub attribute_names: Vec<String>,
    /// The attributes of the first parcels, shortened.
    pub sample_attributes: Vec<Map<String, Value>>,
    pub first_url: String,
}

impl Report {
    fn new(spec: &Spec, county: &County) -> Self {
        Self {
            spec: spec.id.clone(),
            fips: county.fips.clone(),
            county: county.label(),
            passed: false,
            checks: Vec::new(),
            sample_bbox: None,
            features: 0,
            pages: 0,
            fill: BTreeMap::new(),
            attribute_names: Vec::new(),
            sample_attributes: Vec::new(),
            first_url: String::new(),
        }
    }

    fn check(&mut self, name: &'static str, ok: bool, detail: impl Into<String>) -> bool {
        self.checks.push(Check { name, ok, detail: detail.into() });
        ok
    }

    /// One line per check, for logs.
    pub fn summary(&self) -> String {
        let failed: Vec<String> =
            self.checks.iter().filter(|c| !c.ok).map(|c| format!("{}: {}", c.name, c.detail)).collect();
        if self.passed {
            format!("{} passes for {} ({} parcels)", self.spec, self.county, self.features)
        } else {
            format!("{} fails for {}: {}", self.spec, self.county, failed.join("; "))
        }
    }
}

fn share(count: usize, of: usize) -> f64 {
    if of == 0 { 0.0 } else { count as f64 / of as f64 }
}

/// Runs `spec` for `county` and reports every check.
/// Validates `spec` for `county`, failing the `time` check when that takes longer than the
/// runtime allows.
pub async fn validate(runtime: &Runtime, spec: &Spec, county: &County) -> Report {
    match tokio::time::timeout(runtime.validate_within, checks(runtime, spec, county)).await {
        Ok(report) => report,
        Err(_) => {
            let mut report = Report::new(spec, county);
            report.check(
                "time",
                false,
                format!(
                    "validation took longer than {} s: the source answers too slowly to read parcels from",
                    runtime.validate_within.as_secs()
                ),
            );
            report.passed = false;
            report
        }
    }
}

async fn checks(runtime: &Runtime, spec: &Spec, county: &County) -> Report {
    let mut report = Report::new(spec, county);
    if let Err(error) = spec.check() {
        report.check("spec", false, format!("{error:#}"));
        return report;
    }
    if !spec.coverage.claims(&county.fips, |_| Some(county.state.clone())) {
        report.check("coverage", false, format!("the spec's coverage does not include {}", county.fips));
        return report;
    }

    let [x, y] = county.interior;
    let mut sample: Option<(Bbox, Fetch)> = None;
    for size in SAMPLE_SIZES {
        let bbox = Bbox::around(x, y, size);
        match runtime.fetch(spec, bbox, MAX_FEATURES, None).await {
            Ok(fetch) => {
                let enough = fetch.parcels.len() >= MIN_FEATURES;
                sample = Some((bbox, fetch));
                if enough {
                    break;
                }
            }
            Err(error) => {
                report.check("query", false, format!("{error:#}"));
                return report;
            }
        }
    }
    let (bbox, fetch) = sample.expect("at least one sample size");
    report.check("query", true, format!("{} pages", fetch.pages));
    report.sample_bbox = Some(bbox);
    report.features = fetch.parcels.len();
    report.pages = fetch.pages;
    report.first_url = fetch.first_url.clone();
    report.attribute_names = fetch.attribute_names.clone();
    report.sample_attributes = fetch
        .parcels
        .iter()
        .take(2)
        .map(|p| {
            p.attributes
                .iter()
                .take(40)
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) if s.len() > 80 => {
                            Value::String(format!("{}…", &s[..s.floor_char_boundary(80)]))
                        }
                        other => other.clone(),
                    };
                    (k.clone(), v)
                })
                .collect()
        })
        .collect();
    let n = fetch.parcels.len();
    if !report.check(
        "features",
        n >= MIN_FEATURES,
        format!("{n} parcels within {:.2}° of {x},{y} (at least {MIN_FEATURES} needed)", bbox.east() - bbox.west()),
    ) {
        return report;
    }

    let lon_lat = fetch.parcels.iter().filter(|p| Bbox::of(&p.geometry).is_some_and(|b| b.is_lon_lat())).count();
    report.check(
        "longitude-latitude",
        lon_lat == n,
        format!("{lon_lat} of {n} parcels have longitude/latitude coordinates; the others are projected"),
    );
    let valid = fetch.parcels.iter().filter(|p| !p.geometry.0.is_empty() && p.geometry.is_valid()).count();
    report.check(
        "valid-polygons",
        share(valid, n) >= VALID_SHARE,
        format!("{valid} of {n} polygons are valid ({:.0}% needed)", VALID_SHARE * 100.0),
    );
    let centroids: Vec<_> = fetch.parcels.iter().filter_map(|p| p.geometry.centroid()).collect();
    let near_box = bbox.grown(0.02);
    let near = centroids.iter().filter(|c| near_box.contains(c.x(), c.y())).count();
    let inside = centroids.iter().filter(|c| county.contains(c.x(), c.y())).count();
    let first = centroids.first().map(|c| format!(" (the first is at {:.5},{:.5})", c.x(), c.y())).unwrap_or_default();
    report.check(
        "location",
        share(near, n) >= NEAR_SHARE,
        format!(
            "{near} of {n} parcels lie near the box asked for{first}; swapped or projected coordinates land elsewhere"
        ),
    );
    report.check(
        "in-county",
        share(inside, n) >= INSIDE_SHARE,
        format!("{inside} of {n} parcel centroids lie inside {}", county.name),
    );

    let mapped = spec.fields.mapped();
    report.check(
        "fields-mapped",
        ["apn", "address", "owner"].iter().any(|f| mapped.contains_key(f)),
        format!("mapped: {:?}; at least one of apn, address or owner is needed", mapped.keys().collect::<Vec<_>>()),
    );
    for (field, source) in &mapped {
        let exists = fetch.parcels.iter().any(|p| p.attribute(source).is_some());
        let filled = fetch.parcels.iter().filter(|p| p.normalized(spec).contains_key(field)).count();
        report.fill.insert(field.to_string(), (share(filled, n) * 1000.0).round() / 1000.0);
        let detail = if exists {
            format!("{field} ← {source}: filled for {filled} of {n} parcels")
        } else {
            format!("{field} ← {source}: the source has no attribute {source}")
        };
        report.check("field-fill", exists && share(filled, n) >= FILL_SHARE, detail);
    }

    if !matches!(spec.source, Source::BulkDownload { .. }) {
        if fetch.pages > 1 {
            report.check("paging", true, format!("{} pages read", fetch.pages));
        } else {
            // The sample came in one page: read the same box again in two halves. A larger box
            // costs some servers far more than smaller pages do.
            let page = n.div_ceil(2);
            match runtime.fetch(spec, bbox, n + page, Some(page as u32)).await {
                Ok(paged) => {
                    let distinct: std::collections::HashSet<String> =
                        paged.parcels.iter().map(|p| Value::Object(p.attributes.clone()).to_string()).collect();
                    let read = paged.parcels.len();
                    report.check(
                        "paging",
                        paged.pages > 1 && distinct.len() + n / 50 >= n && read <= n + n / 50,
                        format!(
                            "pages of {page}: {read} parcels ({} distinct) in {} pages, against {n} in one",
                            distinct.len(),
                            paged.pages
                        ),
                    );
                }
                Err(error) => {
                    report.check("paging", false, format!("pages of {page} failed: {error:#}"));
                }
            }
        }
    }
    report.passed = report.checks.iter().all(|c| c.ok);
    report
}
