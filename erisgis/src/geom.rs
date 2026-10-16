//! Geometry in WGS84 longitude/latitude: bounding boxes, and polygons read from GeoJSON, Esri
//! JSON and shapefiles into `geo` multipolygons.

use anyhow::{Result, anyhow, bail};
use geo::{Coord, LineString, MultiPolygon, Polygon, Winding};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `[west, south, east, north]` in degrees.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bbox(pub [f64; 4]);

impl Bbox {
    pub fn west(&self) -> f64 {
        self.0[0]
    }
    pub fn south(&self) -> f64 {
        self.0[1]
    }
    pub fn east(&self) -> f64 {
        self.0[2]
    }
    pub fn north(&self) -> f64 {
        self.0[3]
    }

    /// A square box `size` degrees across centred on a point.
    pub fn around(x: f64, y: f64, size: f64) -> Self {
        Self([x - size / 2.0, y - size / 2.0, x + size / 2.0, y + size / 2.0])
    }

    pub fn of(geometry: &MultiPolygon) -> Option<Self> {
        use geo::BoundingRect;
        geometry.bounding_rect().map(|r| Self([r.min().x, r.min().y, r.max().x, r.max().y]))
    }

    pub fn intersects(&self, other: &Bbox) -> bool {
        self.west() <= other.east()
            && other.west() <= self.east()
            && self.south() <= other.north()
            && other.south() <= self.north()
    }

    /// Grown by `margin` degrees on every side.
    pub fn grown(&self, margin: f64) -> Self {
        Self([self.west() - margin, self.south() - margin, self.east() + margin, self.north() + margin])
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.west() && x <= self.east() && y >= self.south() && y <= self.north()
    }

    /// Whether the box could be longitude/latitude at all; projected coordinates are far
    /// outside it.
    pub fn is_lon_lat(&self) -> bool {
        self.0.iter().all(|v| v.is_finite())
            && self.west() >= -181.0
            && self.east() <= 181.0
            && self.south() >= -91.0
            && self.north() <= 91.0
    }

    pub fn polygon(&self) -> Polygon {
        Polygon::new(
            LineString::from(vec![
                (self.west(), self.south()),
                (self.east(), self.south()),
                (self.east(), self.north()),
                (self.west(), self.north()),
                (self.west(), self.south()),
            ]),
            vec![],
        )
    }
}

fn ring(value: &Value) -> Result<LineString> {
    let points = value.as_array().ok_or_else(|| anyhow!("a ring is not an array"))?;
    let coords = points
        .iter()
        .map(|p| {
            let x = p.get(0).and_then(Value::as_f64);
            let y = p.get(1).and_then(Value::as_f64);
            match (x, y) {
                (Some(x), Some(y)) => Ok(Coord { x, y }),
                _ => Err(anyhow!("a coordinate is not a pair of numbers")),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let mut ring = LineString::new(coords);
    ring.close();
    Ok(ring)
}

fn polygon_rings(value: &Value) -> Result<Polygon> {
    let rings = value.as_array().ok_or_else(|| anyhow!("a polygon is not an array of rings"))?;
    let mut rings = rings.iter().map(ring);
    let exterior = rings.next().ok_or_else(|| anyhow!("a polygon has no rings"))??;
    Ok(Polygon::new(exterior, rings.collect::<Result<_>>()?))
}

/// A GeoJSON geometry object as a multipolygon. Points and lines are not parcels.
pub fn from_geojson(geometry: &Value) -> Result<MultiPolygon> {
    let coordinates = &geometry["coordinates"];
    match geometry["type"].as_str() {
        Some("Polygon") => Ok(MultiPolygon::new(vec![polygon_rings(coordinates)?])),
        Some("MultiPolygon") => Ok(MultiPolygon::new(
            coordinates
                .as_array()
                .ok_or_else(|| anyhow!("a multipolygon is not an array"))?
                .iter()
                .map(polygon_rings)
                .collect::<Result<_>>()?,
        )),
        Some("GeometryCollection") => {
            let mut polygons = Vec::new();
            for part in geometry["geometries"].as_array().into_iter().flatten() {
                polygons.extend(from_geojson(part)?.0);
            }
            Ok(MultiPolygon::new(polygons))
        }
        Some(other) => bail!("{other} geometry is not a polygon"),
        None => bail!("the feature has no geometry"),
    }
}

/// An Esri JSON polygon (`{"rings": [...]}`): clockwise rings are outer rings, and each
/// counter-clockwise ring is a hole in the outer ring before it.
pub fn from_esri(geometry: &Value) -> Result<MultiPolygon> {
    let rings = geometry["rings"].as_array().ok_or_else(|| anyhow!("the Esri geometry has no rings"))?;
    let mut polygons: Vec<Polygon> = Vec::new();
    for value in rings {
        let ring = ring(value)?;
        if ring.0.len() < 4 {
            continue;
        }
        if ring.is_cw() || polygons.is_empty() {
            polygons.push(Polygon::new(ring, vec![]));
        } else {
            polygons.last_mut().expect("an outer ring").interiors_push(ring);
        }
    }
    Ok(MultiPolygon::new(polygons))
}

/// A shapefile polygon: outer rings and the holes that follow each.
pub fn from_shape(shape: &shapefile::Shape) -> Result<MultiPolygon> {
    use shapefile::PolygonRing;
    let rings = match shape {
        shapefile::Shape::Polygon(p) => p.rings().iter().map(|r| ring_points(r, |p| (p.x, p.y))).collect::<Vec<_>>(),
        shapefile::Shape::PolygonZ(p) => p.rings().iter().map(|r| ring_points(r, |p| (p.x, p.y))).collect(),
        shapefile::Shape::PolygonM(p) => p.rings().iter().map(|r| ring_points(r, |p| (p.x, p.y))).collect(),
        shapefile::Shape::NullShape => return Ok(MultiPolygon::new(vec![])),
        other => bail!("{} shape is not a polygon", other.shapetype()),
    };
    fn ring_points<P>(ring: &PolygonRing<P>, xy: impl Fn(&P) -> (f64, f64)) -> (bool, LineString) {
        let outer = matches!(ring, PolygonRing::Outer(_));
        let mut line = LineString::from(ring.points().iter().map(xy).collect::<Vec<_>>());
        line.close();
        (outer, line)
    }
    let mut polygons: Vec<Polygon> = Vec::new();
    for (outer, ring) in rings {
        if outer || polygons.is_empty() {
            polygons.push(Polygon::new(ring, vec![]));
        } else {
            polygons.last_mut().expect("an outer ring").interiors_push(ring);
        }
    }
    Ok(MultiPolygon::new(polygons))
}

/// A multipolygon as a GeoJSON geometry, coordinates rounded to `decimals`.
pub fn to_geojson(geometry: &MultiPolygon, decimals: i32) -> Value {
    let scale = 10f64.powi(decimals);
    let round = |v: f64| (v * scale).round() / scale;
    let ring = |r: &LineString| r.0.iter().map(|c| json!([round(c.x), round(c.y)])).collect::<Vec<_>>();
    json!({
        "type": "MultiPolygon",
        "coordinates": geometry.0.iter().map(|p| {
            std::iter::once(ring(p.exterior())).chain(p.interiors().iter().map(ring)).collect::<Vec<_>>()
        }).collect::<Vec<_>>(),
    })
}

/// Web Mercator metres to longitude/latitude.
pub fn from_web_mercator(x: f64, y: f64) -> (f64, f64) {
    const R: f64 = 6_378_137.0;
    let lon = (x / R).to_degrees();
    let lat = (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    #[test]
    fn esri_rings_become_polygons_with_holes() {
        // Clockwise outer square, counter-clockwise hole, then a second clockwise square.
        let geometry = json!({"rings": [
            [[0,0],[0,10],[10,10],[10,0],[0,0]],
            [[2,2],[4,2],[4,4],[2,4],[2,2]],
            [[20,0],[20,1],[21,1],[21,0],[20,0]],
        ]});
        let polygons = from_esri(&geometry).unwrap();
        assert_eq!(polygons.0.len(), 2);
        assert_eq!(polygons.0[0].interiors().len(), 1);
        assert!((polygons.unsigned_area() - (100.0 - 4.0 + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn geojson_polygons_and_multipolygons_read_and_write_back() {
        let polygon = json!({"type": "Polygon", "coordinates": [[[0.123456789,0],[1,0],[1,1],[0,0.123456789]]]});
        let read = from_geojson(&polygon).unwrap();
        assert_eq!(read.0[0].exterior().0.len(), 5, "rings are closed");
        let written = to_geojson(&read, 5);
        assert_eq!(written["coordinates"][0][0][0], json!([0.12346, 0.0]));
        assert!(from_geojson(&json!({"type": "Point", "coordinates": [0, 0]})).is_err());
        assert!(from_geojson(&written).is_ok());
    }

    #[test]
    fn web_mercator_converts_to_degrees() {
        let (lon, lat) = from_web_mercator(-9_754_000.0, 5_351_000.0);
        assert!((lon - -87.62).abs() < 0.01 && (lat - 43.27).abs() < 0.05, "{lon} {lat}");
    }
}
