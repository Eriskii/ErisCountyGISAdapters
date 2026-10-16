//! Zoning through the same machinery as parcels: a zoning spec validates against a fake zoning
//! layer, and a county with a zoning layer of its own and one of a city's shows both, each
//! district classed by its code and name.

use erisgis::counties::{Counties, County};
use erisgis::geom::Bbox;
use erisgis::http::{Http, Limits};
use erisgis::lookup;
use erisgis::parcels::PARCELS;
use erisgis::registry::Registry;
use erisgis::runtime::{Runtime, VALIDATE_WITHIN};
use erisgis::spec::Spec;
use erisgis::testing as support;
use erisgis::validate::validate;
use erisgis::zoning::ZONING;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::OnceLock;

fn counties() -> &'static Counties {
    static COUNTIES: OnceLock<Counties> = OnceLock::new();
    COUNTIES.get_or_init(|| Counties::embedded().unwrap())
}

fn dane() -> &'static County {
    counties().get("55025").unwrap()
}

async fn server() -> String {
    support::serve(support::Grid::around(dane().interior[0], dane().interior[1])).await
}

fn runtime(dir: &Path) -> Runtime {
    Runtime {
        http: Http::new(Limits { allow_private: true, ..Limits::default() }).unwrap(),
        cache: dir.join("downloads"),
        validate_within: VALIDATE_WITHIN,
    }
}

fn zoning_spec(id: &str, publisher: &str, root: &str, fields: Value) -> Value {
    json!({
        "version": 1, "id": id, "publisher": publisher, "official": true,
        "source": {"kind": "arcgis-feature", "url": format!("{root}/zoning/FeatureServer/0"), "page_size": 4},
        "coverage": {"counties": ["55025"]}, "fields": fields,
    })
}

#[tokio::test]
async fn a_zoning_spec_passes_on_a_zoning_layer() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let spec: Spec = serde_json::from_value(zoning_spec(
        "wi-dane",
        "Dane County Zoning",
        &root,
        json!({"zone": "ZONING", "zone_name": "ZONE_DESC", "jurisdiction": "MUNI"}),
    ))
    .unwrap();
    let report = validate(&runtime(dir.path()), &ZONING, &spec, dane()).await;
    assert!(report.passed, "{}", serde_json::to_string_pretty(&report).unwrap());
    assert_eq!(report.features, 9, "every district of the grid lies in the first sample box");
    assert!(report.pages > 1, "the layer pages by 4");
    assert_eq!(report.fill["zone"], 1.0);
    assert!(report.summary().contains("9 zoning districts"), "{}", report.summary());
}

#[tokio::test]
async fn a_zoning_spec_must_map_a_zone_and_only_zoning_fields() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let run = |fields: Value, dataset| {
        let spec: Spec = serde_json::from_value(zoning_spec("wi-dane", "Dane County Zoning", &root, fields)).unwrap();
        let runtime = runtime(dir.path());
        async move { validate(&runtime, dataset, &spec, dane()).await }
    };
    let only_link = run(json!({"jurisdiction": "MUNI"}), &ZONING).await;
    let mapped = only_link.checks.iter().find(|c| c.name == "fields-mapped").unwrap();
    assert!(!mapped.ok && mapped.detail.contains("zone, zone_name"), "{}", mapped.detail);

    let as_parcels = run(json!({"zone": "ZONING"}), &PARCELS).await;
    assert!(!as_parcels.passed);
    assert!(as_parcels.checks[0].detail.contains("zone is not a field of parcels"), "{:?}", as_parcels.checks);
}

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

#[tokio::test]
async fn a_county_shows_every_jurisdictions_zoning_with_each_district_classed() {
    let root = server().await;
    let dir = tempfile::tempdir().unwrap();
    let registry = dir.path().join("zoning");
    let fields = json!({"zone": "ZONING", "zone_name": "ZONE_DESC"});
    write(
        &registry.join("adapters/wi-dane.json"),
        &zoning_spec("wi-dane", "Dane County Zoning", &root, fields.clone()),
    );
    write(&registry.join("adapters/wi-madison.json"), &zoning_spec("wi-madison", "City of Madison", &root, fields));
    write(&registry.join("index.json"), &json!({"version": 1, "counties": {"55025": ["wi-dane", "wi-madison"]}}));
    let registry = Registry::open(&registry).unwrap();
    let [x, y] = dane().interior;
    let read = |bbox: Bbox| {
        let (runtime, registry) = (runtime(dir.path()), &registry);
        async move { lookup::read(&runtime, registry, &ZONING, counties(), bbox, None).await }
    };

    let view = read(Bbox::around(x, y, 0.003)).await;
    let county = &view.counties[0];
    let adapters: Vec<&str> = county.sources.iter().map(|s| s.adapter.as_str()).collect();
    assert_eq!(adapters, ["wi-dane", "wi-madison"], "both jurisdictions' layers are read");
    assert!(county.sources.iter().all(|s| s.verified && s.error.is_none()));

    let from = |adapter: &str| -> Vec<&Value> {
        view.features.iter().filter(|f| f["properties"]["adapter"] == adapter).collect()
    };
    assert!(!from("wi-dane").is_empty() && from("wi-dane").len() == from("wi-madison").len());
    for feature in &view.features {
        let properties = &feature["properties"];
        let expected = erisgis::zoning::class(properties["zone"].as_str(), properties["zone_name"].as_str());
        assert_eq!(properties["class"], expected, "{properties}");
        assert!(properties.get("unit_count").is_none(), "zoning does not stack");
    }

    // A district read in another box keeps its key.
    let keys = |view: &lookup::View| -> std::collections::BTreeSet<String> {
        view.features.iter().map(|f| f["properties"]["key"].as_str().unwrap().to_owned()).collect()
    };
    let wider = read(Bbox::around(x, y, 0.02)).await;
    assert_eq!(wider.features.len(), 18, "nine districts from each layer");
    assert!(keys(&view).is_subset(&keys(&wider)));
    let classes: std::collections::BTreeSet<&str> =
        wider.features.iter().filter_map(|f| f["properties"]["class"].as_str()).collect();
    assert_eq!(
        classes.into_iter().collect::<Vec<_>>(),
        ["agricultural", "commercial", "industrial", "mixed-use", "open-space", "planned", "residential"]
    );
}
