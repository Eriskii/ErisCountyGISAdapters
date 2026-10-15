//! What agents are told: the spec format and how to search, then each job's briefing with its
//! county or state, what neighbouring counties' adapters use, and what earlier attempts tried.

use crate::counties::{Counties, County};
use crate::registry::Registry;
use crate::spec::{Source, Spec};

const SPEC_FORMAT: &str = r#"An adapter spec is JSON:

{
  "id": "ga-fulton",                       // lowercase letters, digits, dashes; unique
  "publisher": "Fulton County GIS",        // who publishes the layer
  "source": { "kind": "arcgis-feature", "url": "https://…/arcgis/rest/services/…/MapServer/0",
              "page_size": 1000,           // the layer's maxRecordCount, or less
              "geojson": true },           // false when supportedQueryFormats lacks geoJSON
  "coverage": { "counties": ["13121"] },   // or { "states": ["WI"] } for a statewide layer
  "fields": { "apn": "PARCELID", "owner": "OWNERNME1", "address": "SITEADRESS",
              "acres": "GISACRES", "land_use": "PROPCLASS", "assessed_value": "CNTASSDVALUE" },
  "filter": "COUNTY_NAME = 'FULTON'",      // optional ArcGIS where clause, e.g. to pick one county
  "notes": "CC-BY 4.0; updated weekly"     // license, terms, quirks
}

Other source kinds:
  { "kind": "wfs", "url": "https://…/geoserver/wfs", "type_name": "ns:parcels", "version": "2.0.0",
    "page_size": 1000, "lat_lon": true }   // lat_lon: the server wants EPSG:4326 latitude first
  { "kind": "ogc-features", "url": "https://…/collections/parcels", "page_size": 1000 }
  { "kind": "bulk-download", "url": "https://…/parcels.zip", "format": "shapefile-zip", "epsg": 4326 }
    // format geojson or shapefile-zip; epsg 4326, 4269 or 3857 only

Every field mapping is optional, but map apn, address or owner at least, and every field you can.
Use the attribute names exactly as the source spells them. The runtime asks for longitude/latitude
(outSR=4326) and pages by itself; you only describe the source."#;

const HOW: &str = r#"How to find parcels:
- Counties and states mostly publish parcels as ArcGIS REST services (…/arcgis/rest/services/…/FeatureServer/N
  or MapServer/N), often behind an ArcGIS Hub open-data site. Use arcgis_search first, then web_search
  (e.g. "<county> county <state> parcels gis", "<county> parcel viewer arcgis rest services").
- An open-data page links the service: look for "View API Resources", "I want to use this", or a
  …/FeatureServer/0 URL. A web viewer's page source or config often names its services too.
- Use arcgis_probe on a services directory (…/arcgis/rest/services) to list folders and services, on a
  service to list layers, and on a layer to list fields, maxRecordCount, paging and formats.
- Pick the polygon layer of tax parcels (not lot lines, not points, not subdivisions).
- Many counties host with a vendor (Schneider/Beacon, Tyler, Vision, county consortiums, state
  portals); neighbouring counties often use the same host, so try what they use.
- Then validate your spec, read the report, fix what it names, and submit_spec once it passes.

A county has no adapter only if, after a real search, its parcels are not published as a queryable
service or file: then mark_unavailable with the reason and the links that show it. A viewer that
loads parcels from a public ArcGIS service IS available: use that service. Never work around logins,
keys, captchas, or terms that forbid automated access; those make a county unavailable.

Be efficient: every call costs time. Do not repeat a fetch you already made."#;

pub fn system_prompt(state_job: bool) -> String {
    let goal = if state_job {
        "You look for ONE public layer of tax parcel polygons that covers a whole US state (a statewide \
         parcel layer from a state GIS office, often aggregated from counties), and when you find one, \
         write and validate an adapter spec for it with coverage { \"states\": [\"XX\"] }. Validation runs \
         it against three counties spread across the state; at least two must pass. If a statewide layer \
         has a county field, it covers every county without a filter. If you find no statewide layer \
         after a real search, stop and say so in one or two sentences, naming any per-county portals \
         you came across."
    } else {
        "You find where one US county publishes its tax parcel polygons, then write and validate an \
         adapter spec that reads them. Your job ends when submit_spec saves a passing spec, or when you \
         mark the county unavailable."
    };
    format!("{goal}\n\n{SPEC_FORMAT}\n\n{HOW}")
}

fn source_line(spec: &Spec) -> String {
    let (kind, url) = match &spec.source {
        Source::ArcgisFeature { url, .. } => ("arcgis-feature", url),
        Source::Wfs { url, .. } => ("wfs", url),
        Source::OgcFeatures { url, .. } => ("ogc-features", url),
        Source::BulkDownload { url, .. } => ("bulk-download", url),
    };
    let filter = spec.filter.as_deref().map(|f| format!(" where {f}")).unwrap_or_default();
    format!("{} ({}): {kind} {url}{filter}", spec.id, spec.publisher)
}

/// Adapters verified for the county's neighbours, or claiming its state.
fn neighbour_adapters(registry: &Registry, counties: &Counties, county: &County) -> Vec<String> {
    let mut lines = Vec::new();
    for fips in counties.neighbours(&county.fips) {
        for id in registry.index.get(fips).into_iter().flatten() {
            if let (Some(spec), Some(n)) = (registry.adapters.get(id), counties.get(fips)) {
                lines.push(format!("- {}: {}", n.label(), source_line(spec)));
            }
        }
    }
    lines.sort();
    lines.dedup();
    lines.truncate(12);
    lines
}

pub fn county_briefing(
    county: &County,
    counties: &Counties,
    registry: &Registry,
    summaries: &[String],
    dead: Option<(&Spec, &str)>,
) -> String {
    let b = county.bbox;
    let mut text = format!(
        "Your county: {} ({}), FIPS {}.\nBounding box (lon/lat): west {:.4}, south {:.4}, east {:.4}, north {:.4}. \
         A point inside it: {:.5},{:.5}. Validation samples parcels around that point.",
        county.name,
        county.state_name,
        county.fips,
        b.west(),
        b.south(),
        b.east(),
        b.north(),
        county.interior[0],
        county.interior[1],
    );
    let neighbours = neighbour_adapters(registry, counties, county);
    if !neighbours.is_empty() {
        text.push_str("\n\nNeighbouring counties' adapters (try the same hosts and patterns first):\n");
        text.push_str(&neighbours.join("\n"));
    }
    let claims = registry.unverified_claims(&county.fips, &county.state);
    if !claims.is_empty() {
        text.push_str("\n\nThese adapters claim your county but do not pass for it:\n");
        for spec in claims.iter().take(5) {
            text.push_str(&format!("- {}\n", source_line(spec)));
        }
    }
    if let Some((spec, report)) = dead {
        text.push_str(&format!(
            "\n\nYour county's adapter stopped working. Its spec was:\n{}\nIts validation now fails: {report}\n\
             Find where the service moved (or what replaced it) and submit a fixed spec; keep its id if the \
             source is the same layer, otherwise choose a new id.",
            serde_json::to_string_pretty(spec).unwrap_or_default()
        ));
    }
    if !summaries.is_empty() {
        text.push_str("\n\nEarlier attempts at this county, which did not succeed, reported:\n");
        for (i, summary) in summaries.iter().enumerate() {
            text.push_str(&format!("--- attempt {} ---\n{}\n", i + 1, summary.trim()));
        }
        text.push_str("Start from what they learned; do not repeat what failed.");
    }
    text
}

pub fn state_briefing(state: &str, state_name: &str, counties: usize, summaries: &[String]) -> String {
    let mut text = format!(
        "Your state: {state_name} ({state}), {counties} counties. Find a statewide parcel layer, if one exists."
    );
    if !summaries.is_empty() {
        text.push_str("\n\nEarlier attempts reported:\n");
        for summary in summaries {
            text.push_str(&format!("- {}\n", summary.trim()));
        }
    }
    text
}

pub const NUDGE: &str = "You stopped without calling submit_spec or mark_unavailable. Keep going: \
    validate and submit a spec, or mark the county unavailable with evidence. If you are truly stuck, \
    reply with a short summary of what you tried (URLs, layers, what failed) instead.";
