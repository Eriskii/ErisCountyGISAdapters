//! Reads parcels through a spec: the features of its source within a bounding box, each with
//! its geometry in longitude/latitude, its source attributes, and the normalized fields.

use crate::geom::{self, Bbox};
use crate::http::Http;
use crate::spec::{BulkFormat, Source, Spec};
use anyhow::{Context, Result, anyhow, bail, ensure};
use geo::{MapCoords, MultiPolygon};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::PathBuf;

/// Biggest single page of features.
const PAGE_CAP: usize = 64 << 20;
/// Biggest bulk download.
const BULK_CAP: usize = 768 << 20;

pub struct Parcel {
    pub geometry: MultiPolygon,
    pub attributes: Map<String, Value>,
}

impl Parcel {
    /// An attribute by name, ignoring case, as servers differ on it.
    pub fn attribute(&self, name: &str) -> Option<&Value> {
        self.attributes
            .get(name)
            .or_else(|| self.attributes.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
    }

    /// The normalized fields the spec maps, those with a value.
    pub fn normalized(&self, spec: &Spec) -> BTreeMap<&'static str, Value> {
        spec.fields
            .mapped()
            .into_iter()
            .filter_map(|(field, source)| {
                let value = self.attribute(source)?;
                let empty = value.is_null() || value.as_str().is_some_and(|s| s.trim().is_empty());
                (!empty).then(|| (field, value.clone()))
            })
            .collect()
    }

    pub fn to_geojson(&self, spec: &Spec) -> Value {
        json!({
            "type": "Feature",
            "geometry": geom::to_geojson(&self.geometry, 7),
            "properties": self.normalized(spec),
        })
    }
}

pub struct Fetch {
    pub parcels: Vec<Parcel>,
    /// Requests made, one per page.
    pub pages: usize,
    /// Whether more features were there than were read.
    pub more: bool,
    /// Attribute names the source has, from its first feature.
    pub attribute_names: Vec<String>,
    /// The URL of the first request, for reports.
    pub first_url: String,
}

/// How ArcGIS Online answers a query that timed out inside it. Such a query, and one its gateway
/// gave up on (502, 503, 504), is asked again after each of [`ARCGIS_RETRIES`]' pauses before
/// it counts as failed.
const TIMED_OUT: &str = "Unable to perform query";
const ARCGIS_RETRIES: [std::time::Duration; 2] = [std::time::Duration::from_secs(2), std::time::Duration::from_secs(5)];

/// How long one county's validation may take by default: a source slower than this is too slow
/// to read parcels from.
pub const VALIDATE_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

pub struct Runtime {
    pub http: Http,
    /// Where bulk downloads are kept between validations.
    pub cache: PathBuf,
    /// The longest one county's validation may take.
    pub validate_within: std::time::Duration,
}

impl Runtime {
    /// Up to `max` parcels intersecting `bbox`, reading `page` at a time when the source pages
    /// (the spec's page size by default).
    pub async fn fetch(&self, spec: &Spec, bbox: Bbox, max: usize, page: Option<u32>) -> Result<Fetch> {
        let mut fetch = match &spec.source {
            Source::ArcgisFeature { url, page_size, geojson } => {
                self.arcgis(url, page.unwrap_or(*page_size), *geojson, spec.filter.as_deref(), bbox, max).await?
            }
            Source::Wfs { url, type_name, version, page_size, lat_lon } => {
                self.wfs(url, type_name, version, page.unwrap_or(*page_size), *lat_lon, bbox, max).await?
            }
            Source::OgcFeatures { url, page_size } => self.ogc(url, page.unwrap_or(*page_size), bbox, max).await?,
            Source::BulkDownload { url, format, epsg } => self.bulk(url, *format, *epsg, bbox, max).await?,
        };
        if let Some(first) = fetch.parcels.first() {
            fetch.attribute_names = first.attributes.keys().cloned().collect();
        }
        Ok(fetch)
    }

    /// One page of an ArcGIS query, asked again while the server timed out answering it.
    async fn arcgis_page(&self, url: &str) -> Result<Value> {
        let mut pauses = ARCGIS_RETRIES.iter();
        loop {
            let fetched = self.http.get(url, PAGE_CAP).await?;
            let gateway = matches!(fetched.status, 502..=504);
            let body = if gateway { Value::Null } else { fetched.json().unwrap_or(Value::Null) };
            let timed_out = body["error"]["message"].as_str().is_some_and(|m| m.starts_with(TIMED_OUT));
            match pauses.next() {
                Some(pause) if gateway || timed_out => tokio::time::sleep(*pause).await,
                _ => return self.http.json_answer(url, fetched),
            }
        }
    }

    async fn arcgis(
        &self,
        url: &str,
        page: u32,
        geojson: bool,
        filter: Option<&str>,
        bbox: Bbox,
        max: usize,
    ) -> Result<Fetch> {
        let base = format!("{}/query", url.trim_end_matches('/'));
        let mut fetch = empty();
        let mut offset = 0usize;
        loop {
            let mut query = url::Url::parse(&base)?;
            query
                .query_pairs_mut()
                .append_pair("where", filter.unwrap_or("1=1"))
                .append_pair("geometry", &format!("{},{},{},{}", bbox.west(), bbox.south(), bbox.east(), bbox.north()))
                .append_pair("geometryType", "esriGeometryEnvelope")
                .append_pair("inSR", "4326")
                .append_pair("spatialRel", "esriSpatialRelIntersects")
                .append_pair("outFields", "*")
                .append_pair("returnGeometry", "true")
                .append_pair("outSR", "4326")
                .append_pair("resultOffset", &offset.to_string())
                .append_pair("resultRecordCount", &page.to_string())
                .append_pair("f", if geojson { "geojson" } else { "json" });
            if fetch.first_url.is_empty() {
                fetch.first_url = query.to_string();
            }
            let body = self.arcgis_page(query.as_str()).await?;
            if let Some(error) = body.get("error") {
                bail!(
                    "the ArcGIS server refused the query: {} {}",
                    error["message"].as_str().unwrap_or("(no message)"),
                    error["details"]
                );
            }
            fetch.pages += 1;
            let before = fetch.parcels.len();
            if geojson {
                read_geojson_features(&body, &mut fetch.parcels)?;
            } else {
                for feature in
                    body["features"].as_array().ok_or_else(|| anyhow!("the ArcGIS answer has no features"))?
                {
                    fetch.parcels.push(Parcel {
                        geometry: geom::from_esri(&feature["geometry"])?,
                        attributes: feature["attributes"].as_object().cloned().unwrap_or_default(),
                    });
                }
            }
            let got = fetch.parcels.len() - before;
            repeated(&fetch.parcels, before, got, "resultOffset")?;
            let exceeded =
                body["exceededTransferLimit"].as_bool().or(body["properties"]["exceededTransferLimit"].as_bool());
            let more = exceeded.unwrap_or(got >= page as usize) && got > 0;
            if !more {
                break;
            }
            if fetch.parcels.len() >= max {
                fetch.more = true;
                break;
            }
            offset += got;
        }
        Ok(fetch)
    }

    #[allow(clippy::too_many_arguments)]
    async fn wfs(
        &self,
        url: &str,
        type_name: &str,
        version: &str,
        page: u32,
        lat_lon: bool,
        bbox: Bbox,
        max: usize,
    ) -> Result<Fetch> {
        let two = version.starts_with('2');
        let mut fetch = empty();
        let mut offset = 0usize;
        loop {
            let mut query = url::Url::parse(url)?;
            let corners = if lat_lon {
                format!("{},{},{},{}", bbox.south(), bbox.west(), bbox.north(), bbox.east())
            } else {
                format!("{},{},{},{}", bbox.west(), bbox.south(), bbox.east(), bbox.north())
            };
            query
                .query_pairs_mut()
                .append_pair("service", "WFS")
                .append_pair("version", version)
                .append_pair("request", "GetFeature")
                .append_pair(if two { "typeNames" } else { "typeName" }, type_name)
                .append_pair("outputFormat", "application/json")
                .append_pair("srsName", "EPSG:4326")
                .append_pair("bbox", &format!("{corners},EPSG:4326"))
                .append_pair(if two { "count" } else { "maxFeatures" }, &page.to_string())
                .append_pair("startIndex", &offset.to_string());
            if fetch.first_url.is_empty() {
                fetch.first_url = query.to_string();
            }
            let fetched = self.http.get(query.as_str(), PAGE_CAP).await?;
            if fetched.text().trim_start().starts_with('<') {
                bail!("the WFS answered XML, not GeoJSON: {}", fetched.text().chars().take(400).collect::<String>());
            }
            ensure!((200..300).contains(&fetched.status), "the WFS answered {}", fetched.status);
            let body = fetched.json()?;
            fetch.pages += 1;
            let before = fetch.parcels.len();
            read_geojson_features(&body, &mut fetch.parcels)?;
            if lat_lon {
                for parcel in &mut fetch.parcels[before..] {
                    parcel.geometry = parcel.geometry.map_coords(|c| geo::Coord { x: c.y, y: c.x });
                }
            }
            let got = fetch.parcels.len() - before;
            repeated(&fetch.parcels, before, got, "startIndex")?;
            if got < page as usize || got == 0 {
                break;
            }
            if fetch.parcels.len() >= max {
                fetch.more = true;
                break;
            }
            offset += got;
        }
        Ok(fetch)
    }

    async fn ogc(&self, url: &str, page: u32, bbox: Bbox, max: usize) -> Result<Fetch> {
        let mut fetch = empty();
        let mut next = {
            let mut query = url::Url::parse(&format!("{}/items", url.trim_end_matches('/')))?;
            query
                .query_pairs_mut()
                .append_pair("bbox", &format!("{},{},{},{}", bbox.west(), bbox.south(), bbox.east(), bbox.north()))
                .append_pair("limit", &page.to_string());
            Some(query.to_string())
        };
        while let Some(url) = next.take() {
            if fetch.first_url.is_empty() {
                fetch.first_url = url.clone();
            }
            let body = self.http.get_json(&url, PAGE_CAP).await?;
            fetch.pages += 1;
            read_geojson_features(&body, &mut fetch.parcels)?;
            let link = body["links"].as_array().into_iter().flatten().find(|l| l["rel"] == "next");
            if let Some(href) = link.and_then(|l| l["href"].as_str()) {
                if fetch.parcels.len() >= max {
                    fetch.more = true;
                    break;
                }
                next = Some(url::Url::parse(&url)?.join(href)?.to_string());
            }
        }
        Ok(fetch)
    }

    async fn bulk(&self, url: &str, format: BulkFormat, epsg: u32, bbox: Bbox, max: usize) -> Result<Fetch> {
        let bytes = self.download(url).await?;
        let mut parcels = Vec::new();
        match format {
            BulkFormat::Geojson => {
                let body: Value = serde_json::from_slice(&bytes).context("the download is not GeoJSON")?;
                read_geojson_features(&body, &mut parcels)?;
            }
            BulkFormat::ShapefileZip => read_shapefile_zip(&bytes, &mut parcels)?,
        }
        if epsg == 3857 {
            for parcel in &mut parcels {
                parcel.geometry = parcel.geometry.map_coords(|c| {
                    let (x, y) = geom::from_web_mercator(c.x, c.y);
                    geo::Coord { x, y }
                });
            }
        }
        let mut inside: Vec<Parcel> =
            parcels.into_iter().filter(|p| Bbox::of(&p.geometry).is_some_and(|b| b.intersects(&bbox))).collect();
        let more = inside.len() > max;
        inside.truncate(max);
        Ok(Fetch { parcels: inside, pages: 1, more, attribute_names: Vec::new(), first_url: url.to_owned() })
    }

    /// A bulk file, from the cache when it has been downloaded before.
    async fn download(&self, url: &str) -> Result<Vec<u8>> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut hasher);
        let path = self.cache.join(format!("{:016x}", hasher.finish()));
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return Ok(bytes);
        }
        let fetched = self.http.get(url, BULK_CAP).await?;
        ensure!((200..300).contains(&fetched.status), "{url} answered {}", fetched.status);
        ensure!(!fetched.truncated, "{url} is larger than {} MB", BULK_CAP >> 20);
        tokio::fs::create_dir_all(&self.cache).await?;
        tokio::fs::write(&path, &fetched.body).await?;
        Ok(fetched.body)
    }
}

/// Fails when the page just read starts as the one before it did: the server ignores the
/// paging parameter and would return the same features forever.
fn repeated(parcels: &[Parcel], before: usize, got: usize, parameter: &str) -> Result<()> {
    if before == 0 || got == 0 || before < got {
        return Ok(());
    }
    let first = |i: usize| (&parcels[i].attributes, Bbox::of(&parcels[i].geometry));
    ensure!(
        first(before) != first(before - got),
        "the server answered the same page twice: it ignores {parameter}, so it cannot be paged"
    );
    Ok(())
}

fn empty() -> Fetch {
    Fetch { parcels: Vec::new(), pages: 0, more: false, attribute_names: Vec::new(), first_url: String::new() }
}

fn read_geojson_features(body: &Value, into: &mut Vec<Parcel>) -> Result<()> {
    let features = body["features"].as_array().ok_or_else(|| {
        anyhow!(
            "the answer is not a GeoJSON FeatureCollection: {}",
            body.to_string().chars().take(300).collect::<String>()
        )
    })?;
    for feature in features {
        if feature["geometry"].is_null() {
            continue;
        }
        into.push(Parcel {
            geometry: geom::from_geojson(&feature["geometry"])?,
            attributes: feature["properties"].as_object().cloned().unwrap_or_default(),
        });
    }
    Ok(())
}

fn read_shapefile_zip(bytes: &[u8], into: &mut Vec<Parcel>) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("the download is not a zip")?;
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    let shp =
        names.iter().find(|n| n.to_lowercase().ends_with(".shp")).ok_or_else(|| anyhow!("the zip has no .shp"))?;
    let stem = &shp[..shp.len() - 4];
    let dbf = names
        .iter()
        .find(|n| n.len() == shp.len() && n.starts_with(stem) && n.to_lowercase().ends_with(".dbf"))
        .ok_or_else(|| anyhow!("the zip has no .dbf beside {shp}"))?
        .clone();
    let mut read = |name: &str| -> Result<Vec<u8>> {
        let mut data = Vec::new();
        archive.by_name(name)?.read_to_end(&mut data)?;
        Ok(data)
    };
    let (shp, dbf) = (read(&shp.clone())?, read(&dbf)?);
    let mut reader = shapefile::Reader::new(
        shapefile::ShapeReader::new(Cursor::new(shp))?,
        shapefile::dbase::Reader::new(Cursor::new(dbf))?,
    );
    for item in reader.iter_shapes_and_records() {
        let (shape, record) = item?;
        let geometry = geom::from_shape(&shape)?;
        if geometry.0.is_empty() {
            continue;
        }
        let mut attributes = Map::new();
        for (name, value) in record {
            use shapefile::dbase::FieldValue as F;
            let value = match value {
                F::Character(s) => s.map_or(Value::Null, |s| json!(s.trim())),
                F::Memo(s) => json!(s.trim()),
                F::Numeric(n) => n.map_or(Value::Null, |n| json!(n)),
                F::Float(n) => n.map_or(Value::Null, |n| json!(n)),
                F::Double(n) => json!(n),
                F::Integer(n) => json!(n),
                F::Currency(n) => json!(n),
                F::Logical(b) => b.map_or(Value::Null, |b| json!(b)),
                other => json!(format!("{other:?}")),
            };
            attributes.insert(name, value);
        }
        into.push(Parcel { geometry, attributes });
    }
    Ok(())
}
