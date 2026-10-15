//! What agents are told: the spec format and how to search, then each job's briefing with its
//! county or state, what neighbouring counties' adapters use, and what earlier attempts tried.

use crate::counties::{Counties, County};
use crate::registry::Registry;
use crate::spec::{Source, Spec};

const HOW: &str = r#"How to find parcels:
- Counties and states mostly publish parcels as ArcGIS REST services (…/arcgis/rest/services/…/FeatureServer/N
  or MapServer/N), often behind an ArcGIS Hub open-data site. Start with `arcgis search parcels`, then
  `web search <county> county <state> parcels gis`.
- Read pages by asking about them: `web read <url> which links lead to parcel data or ArcGIS REST
  services?` answers in a few lines with the URLs. An open-data page links its service ("View API
  Resources", "I want to use this", a …/FeatureServer/0 URL); a web viewer's page or config names its
  services. Do not `web get` HTML pages: `web get` is for JSON endpoints, with --grep.
- `arcgis probe` a services directory, then a service, then its parcel layer. Pick the polygon layer of
  tax parcels: not lot lines, points or subdivisions.
- Neighbouring counties often share a host or vendor (Schneider/Beacon, Tyler, Vision, a regional
  consortium, a state portal): try theirs first.
- Then `spec validate '<json>'`, read the report, fix what it names, and `spec submit` once it passes.

A county is unavailable only if, after a real search, its parcels are not published as a queryable
service or file. A viewer that draws parcels from a public ArcGIS service is available: use that
service. Never work around logins, keys, captchas, or terms that forbid automated access; those make a
county unavailable.

Be efficient: every command costs time, and `--help` on any command explains it. Do not repeat a
command you already ran."#;

pub fn system_prompt(state_job: bool) -> String {
    let goal = if state_job {
        "You look for ONE public layer of tax parcel polygons that covers a whole US state (a statewide \
         parcel layer from a state GIS office, often aggregated from counties). When you find one, \
         validate a spec for it with coverage {\"states\": [\"XX\"]} using `statewide_spec validate`, and \
         save it with `statewide_spec submit`. A statewide layer with a county field covers every county \
         without a filter. If you find no statewide layer after a real search, stop and say so in one or \
         two sentences, naming any per-county portals you came across."
    } else {
        "You find where one US county publishes its tax parcel polygons, then write and validate an \
         adapter spec that reads them. Your job ends when `spec submit` saves a passing spec, or when \
         `spec unavailable` records that the county has none."
    };
    format!("{goal}\n\n{HOW}")
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

pub const NUDGE: &str = "You stopped without `spec submit` or `spec unavailable`. Keep going: validate \
    and submit a spec, or record the county unavailable with evidence. If you are truly stuck, reply with \
    a short summary of what you tried (URLs, layers, what failed) instead.";
