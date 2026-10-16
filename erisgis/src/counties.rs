//! Every county and county equivalent in the 50 states, DC and Puerto Rico, from the Census
//! Bureau's 1:500,000 cartographic boundaries, simplified and embedded in the binary.
//! `regenerate` rebuilds the embedded file from census.gov.

use crate::geom::{self, Bbox};
use anyhow::{Context, Result, anyhow};
use geo::{Contains, InteriorPoint, MultiPolygon, Point, Simplify};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read};
use std::path::Path;

pub const SOURCE: &str = "https://www2.census.gov/geo/tiger/GENZ2024/shp/cb_2024_us_county_500k.zip";
const EMBEDDED: &[u8] = include_bytes!("../data/counties.json.gz");
/// Simplification tolerance in degrees (about 30 m).
const TOLERANCE: f64 = 0.0003;
/// State FIPS codes of territories other than Puerto Rico, which are left out.
const LEFT_OUT: [&str; 4] = ["60", "66", "69", "78"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct County {
    /// Five digits: state then county.
    pub fips: String,
    /// The full name, such as `Dane County` or `Orleans Parish`.
    pub name: String,
    /// The postal abbreviation, such as `WI`.
    pub state: String,
    pub state_name: String,
    pub bbox: Bbox,
    /// A point inside the county, where samples are taken.
    pub interior: [f64; 2],
    #[serde(skip, default = "empty")]
    pub geometry: MultiPolygon,
}

impl County {
    pub fn label(&self) -> String {
        format!("{}, {} ({})", self.name, self.state, self.fips)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.bbox.contains(x, y) && self.geometry.contains(&Point::new(x, y))
    }
}

fn empty() -> MultiPolygon {
    MultiPolygon::new(vec![])
}

pub struct Counties {
    pub list: Vec<County>,
    by_fips: HashMap<String, usize>,
    /// Counties whose boundaries come within a few hundred metres of each other's.
    neighbours: HashMap<String, Vec<String>>,
}

impl Counties {
    pub fn embedded() -> Result<Self> {
        let mut text = String::new();
        flate2::read::GzDecoder::new(EMBEDDED).read_to_string(&mut text)?;
        Self::from_json(&serde_json::from_str(&text)?)
    }

    fn from_json(value: &Value) -> Result<Self> {
        let mut list = Vec::new();
        for entry in value["counties"].as_array().ok_or_else(|| anyhow!("no counties"))? {
            let mut county: County = serde_json::from_value(entry.clone())?;
            county.geometry = geom::from_geojson(&entry["geometry"])?;
            list.push(county);
        }
        list.sort_by(|a, b| a.fips.cmp(&b.fips));
        let by_fips = list.iter().enumerate().map(|(i, c)| (c.fips.clone(), i)).collect();
        let neighbours = neighbours(&list);
        Ok(Self { list, by_fips, neighbours })
    }

    pub fn get(&self, fips: &str) -> Option<&County> {
        self.by_fips.get(fips).map(|&i| &self.list[i])
    }

    pub fn neighbours(&self, fips: &str) -> &[String] {
        self.neighbours.get(fips).map_or(&[], Vec::as_slice)
    }

    /// Postal abbreviations, sorted.
    pub fn states(&self) -> Vec<String> {
        let mut states: Vec<String> = self.list.iter().map(|c| c.state.clone()).collect();
        states.dedup();
        states.sort();
        states.dedup();
        states
    }

    pub fn in_state<'a>(&'a self, state: &'a str) -> impl Iterator<Item = &'a County> + 'a {
        self.list.iter().filter(move |c| c.state == state)
    }

    /// The state's counties spread across it: the first, the last, and evenly between.
    pub fn spread<'a>(&'a self, state: &'a str, count: usize) -> Vec<&'a County> {
        let counties: Vec<&County> = self.in_state(state).collect();
        if counties.len() <= count {
            return counties;
        }
        (0..count).map(|i| counties[i * (counties.len() - 1) / (count - 1).max(1)]).collect()
    }
}

/// Counties with boundary vertices in a shared cell of a grid about 400 m across: those that
/// share a border, whatever simplification did to it.
fn neighbours(list: &[County]) -> HashMap<String, Vec<String>> {
    use std::collections::BTreeSet;
    const CELL: f64 = 0.004;
    let mut cells: HashMap<(i64, i64), BTreeSet<usize>> = HashMap::new();
    for (i, county) in list.iter().enumerate() {
        for polygon in &county.geometry.0 {
            for c in polygon.exterior().0.iter() {
                cells.entry(((c.x / CELL).floor() as i64, (c.y / CELL).floor() as i64)).or_default().insert(i);
            }
        }
    }
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for members in cells.values().filter(|m| m.len() > 1) {
        for &a in members {
            for &b in members.range(a + 1..) {
                pairs.insert((a, b));
            }
        }
    }
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (a, b) in pairs {
        out.entry(list[a].fips.clone()).or_default().push(list[b].fips.clone());
        out.entry(list[b].fips.clone()).or_default().push(list[a].fips.clone());
    }
    out
}

/// Downloads the Census boundaries and writes the embedded county file to `out`.
pub async fn regenerate(out: &Path) -> Result<usize> {
    let bytes = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .build()?
        .get(SOURCE)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let value = from_census_zip(&bytes)?;
    let count = value["counties"].as_array().map_or(0, Vec::len);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut encoder, serde_json::to_string(&value)?.as_bytes())?;
    std::fs::write(out, encoder.finish()?).with_context(|| format!("writing {}", out.display()))?;
    Ok(count)
}

/// The county file's JSON from the Census shapefile zip.
fn from_census_zip(bytes: &[u8]) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut entry = |suffix: &str| -> Result<Vec<u8>> {
        let name = archive
            .file_names()
            .find(|n| n.ends_with(suffix))
            .ok_or_else(|| anyhow!("the Census zip has no {suffix}"))?
            .to_owned();
        let mut data = Vec::new();
        archive.by_name(&name)?.read_to_end(&mut data)?;
        Ok(data)
    };
    let (shp, dbf) = (entry(".shp")?, entry(".dbf")?);
    let shapes = shapefile::ShapeReader::new(Cursor::new(shp))?;
    let records = shapefile::dbase::Reader::new(Cursor::new(dbf))?;
    let mut reader = shapefile::Reader::new(shapes, records);
    let mut counties = BTreeMap::new();
    for item in reader.iter_shapes_and_records() {
        let (shape, record) = item?;
        let text = |key: &str| match record.get(key) {
            Some(shapefile::dbase::FieldValue::Character(Some(s))) => s.trim().to_owned(),
            _ => String::new(),
        };
        let fips = text("GEOID");
        if LEFT_OUT.contains(&&fips[..2]) {
            continue;
        }
        let geometry = geom::from_shape(&shape)?.simplify(TOLERANCE);
        let bbox = Bbox::of(&geometry).ok_or_else(|| anyhow!("{fips} has no geometry"))?;
        let interior = geometry.interior_point().ok_or_else(|| anyhow!("{fips} has no interior"))?;
        counties.insert(
            fips.clone(),
            json!({
                "fips": fips,
                "name": text("NAMELSAD"),
                "state": text("STUSPS"),
                "state_name": text("STATE_NAME"),
                "bbox": bbox.0.map(|v| (v * 1e5).round() / 1e5),
                "interior": [(interior.x() * 1e5).round() / 1e5, (interior.y() * 1e5).round() / 1e5],
                "geometry": geom::to_geojson(&geometry, 5),
            }),
        );
    }
    Ok(json!({"version": 1, "source": SOURCE, "counties": counties.into_values().collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_county_is_embedded_with_its_shape() {
        let counties = Counties::embedded().unwrap();
        assert!((3200..3260).contains(&counties.list.len()), "{}", counties.list.len());
        assert_eq!(counties.states().len(), 52, "50 states, DC and Puerto Rico");
        let dane = counties.get("55025").unwrap();
        assert_eq!((dane.name.as_str(), dane.state.as_str()), ("Dane County", "WI"));
        assert!(dane.contains(-89.40, 43.07), "Madison is in Dane County");
        assert!(!dane.contains(-87.91, 43.04), "Milwaukee is not");
        assert!(dane.contains(dane.interior[0], dane.interior[1]));
        let neighbours = counties.neighbours("55025");
        assert!(neighbours.contains(&"55021".to_owned()), "Columbia borders Dane: {neighbours:?}");
        assert!(!neighbours.contains(&"55079".to_owned()), "Milwaukee does not");
    }

    #[test]
    fn spread_picks_counties_across_the_state() {
        let counties = Counties::embedded().unwrap();
        let picked = counties.spread("WI", 3);
        assert_eq!(picked.iter().map(|c| c.fips.as_str()).collect::<Vec<_>>(), ["55001", "55071", "55141"]);
    }
}
