//! The reader on the saved portolan-nl and TriMet documents
//! (`tests/fixtures/source.coop/`, fetched 2026-10-24: a URL's path on
//! data.source.coop with `/` as `__`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::*;

const NL: &str = "https://data.source.coop/cholmes/portolan-nl/catalog.json";
const TRIMET: &str = "https://data.source.coop/cholmes/trimet/catalog.json";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source.coop")
}

/// Answers from the fixtures, as data.source.coop would (with an ETag).
struct Fixtures;

impl Transport for Fixtures {
    fn get(&self, url: &str, c: &Conditional) -> Result<Response, String> {
        let path = url.strip_prefix("https://data.source.coop/").ok_or("not source.coop")?;
        let file = fixtures().join(path.replace('/', "__"));
        let body = std::fs::read(&file).map_err(|_| "HTTP 404".to_string());
        let body = match body {
            Ok(b) => b,
            Err(_) => return Ok(Response { status: 404, ..Default::default() }),
        };
        let etag = format!("\"{:x}\"", cache::fnv1a(&body));
        if c.if_none_match.as_deref() == Some(etag.as_str()) {
            return Ok(Response { status: 304, etag: Some(etag), ..Default::default() });
        }
        Ok(Response { status: 200, etag: Some(etag), last_modified: None, body })
    }
}

fn catalog(root: &str) -> Catalog {
    Catalog::new(root, Arc::new(HttpCache::new(None, Some(Arc::new(Fixtures)), Duration::ZERO)))
}

fn collection(id_path: &str) -> Node {
    let url = format!("https://data.source.coop/cholmes/portolan-nl/{id_path}/collection.json");
    let file = fixtures().join(format!("cholmes/portolan-nl/{id_path}/collection.json").replace('/', "__"));
    Node::parse(&url, &std::fs::read(file).unwrap()).unwrap()
}

#[test]
fn the_netherlands_tree_has_eight_agencies_and_thirty_four_collections() {
    let cat = catalog(NL);
    let tree = cat.tree(4).unwrap();
    assert_eq!(tree.node.kind, NodeKind::Catalog);
    assert_eq!(tree.node.portolan_version.as_deref(), Some("0.1.0"));
    assert_eq!(tree.children.len(), 8, "{:?}", tree.errors);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let ids: Vec<&str> = tree.children.iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(ids, ["rijkswaterstaat", "kadaster", "cbs", "rvo", "rce", "beeldmateriaal", "tudelft", "vro"]);
    assert_eq!(tree.collections().len(), 34);
    // VRO nests two more catalogs.
    let vro = tree.find("vro").unwrap();
    assert!(vro.children.iter().filter(|c| c.node.kind == NodeKind::Catalog).count() == 2);
    // Lazily: the aerial photos' map sheets and the crop parcels' years
    // (a collection's children and items) were never fetched.
    let sent = cat.cache().requests_sent();
    assert_eq!(sent, 1 + 8 + 34 + 2, "root, agencies, collections, VRO's two sub-catalogs");
    assert!(tree.any_live());
}

#[test]
fn a_collection_reads_its_assets_roles_licence_providers_and_agents_link() {
    let n = collection("rvo/natura2000");
    assert_eq!(n.kind, NodeKind::Collection);
    assert_eq!(n.display_title(), "Natura 2000 Protected Areas");
    assert_eq!(n.licence.id.as_deref(), Some("CC0-1.0"));
    assert_eq!(n.licence.class, LicenceClass::PublicDomain);
    assert_eq!(n.pmtiles_url(), Some("https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/natura2000.pmtiles"));
    assert_eq!(n.pmtiles_layers(), ["natura2000"]);
    let style = n.default_style().unwrap();
    assert_eq!(style.href, "https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/styles/default.json");
    assert!(style.has_role("default"));
    assert_eq!(n.thumbnail_url(), Some("https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/thumbnail.webp"));
    assert_eq!(n.agents_url(), Some("https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/AGENTS.md"));
    assert_eq!(n.readme_url(), Some("https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/README.md"));
    assert_eq!(n.crs.as_deref(), Some("EPSG:28992"));
    assert_eq!(n.row_count, Some(209));
    assert_eq!(n.pmtiles_zoom, Some((0, 12)));
    assert_eq!(n.formats(), [Format::PmTiles, Format::GeoParquet]);
    assert_eq!(n.producers(), ["Rijksdienst voor Ondernemend Nederland"]);
    assert!(n.is_mirror());
    assert_eq!(n.credit_line(), "Rijksdienst voor Ondernemend Nederland · CC0-1.0 · via Source Cooperative");
    assert_eq!(n.bbox, Some([2.681884, 50.729086, 7.319863, 55.657302]));
    assert!(n.columns.iter().any(|c| c.name == "naam_n2k"));
    assert!(n.short_description(80).chars().count() <= 80);
}

#[test]
fn portolan_nl_deviations_are_warnings_not_refusals() {
    let n = collection("rvo/natura2000");
    let codes: Vec<&str> = n.warnings.iter().map(|w| w.code).collect();
    assert!(codes.contains(&"media_type"), "application/x-parquet is noted: {codes:?}");
    // A collection holding catalogs (the aerial photos' map sheets).
    let photos = collection("beeldmateriaal/luchtfoto_2024");
    assert!(photos.warnings.iter().any(|w| w.code == "nesting"), "{:?}", photos.warnings);
    assert_eq!(photos.crs.as_deref(), Some("EPSG:28992"), "from summaries");
    assert_eq!(photos.formats(), [Format::Cog]);
    // A broken document still parses.
    let odd = Node::parse("https://h.org/c/x.json", br#"{"id": "x", "links": [{"rel": "child"}, 7], "assets": {"a": {"href": "s3://b/a.parquet"}}}"#).unwrap();
    let codes: Vec<&str> = odd.warnings.iter().map(|w| w.code).collect();
    for c in ["type", "stac_version", "link", "s3_href", "missing_roles", "missing_type", "agents"] {
        assert!(codes.contains(&c), "{c} in {codes:?}");
    }
    assert!(Node::parse("https://h.org/x.json", b"{}").is_err(), "no id at all is the one refusal");
    assert!(Node::parse("https://h.org/x.json", b"not json").is_err());
}

#[test]
fn an_item_reads_its_properties_and_bbox() {
    let url = "https://data.source.coop/cholmes/portolan-nl/beeldmateriaal/luchtfoto_2024/kb19/luchtfoto-2024-19an1.json";
    let file = fixtures().join("cholmes__portolan-nl__beeldmateriaal__luchtfoto_2024__kb19__luchtfoto-2024-19an1.json");
    let item = Node::parse(url, &std::fs::read(file).unwrap()).unwrap();
    assert_eq!(item.kind, NodeKind::Item);
    assert_eq!(item.crs.as_deref(), Some("EPSG:28992"));
    assert_eq!(item.bbox.map(|b| b[0]), Some(4.5733676));
    assert!(item.formats().contains(&Format::Cog));
    assert!(!item.warnings.iter().any(|w| w.code == "agents"), "items carry no AGENTS.md");
}

#[test]
fn what_the_host_can_draw_today_is_said_not_guessed() {
    let support = Support::default();
    let n = assess(&collection("rvo/natura2000"), &support);
    assert!(n.can_add_to_map());
    assert!(matches!(&n.map, MapPath::PmTiles { style_url: Some(s), .. } if s.ends_with("styles/default.json")));
    assert!(matches!(&n.table, TablePath::NeedsReprojection { crs, .. } if crs == "EPSG:28992"), "RD New GeoParquet: {:?}", n.table);
    let photos = assess(&collection("beeldmateriaal/luchtfoto_2024"), &support);
    assert!(!photos.can_add_to_map());
    assert!(photos.needs_reprojection());
    assert!(matches!(&photos.map, MapPath::NeedsReprojection { format: Format::Cog, crs: Some(c) } if c == "EPSG:28992"));
    // 3D BAG's GeoParquet says 3857: still not CRS84.
    let bag = assess(&collection("tudelft/3dbag"), &support);
    assert!(bag.can_add_to_map());
}

#[test]
fn the_licence_gate_hides_other_and_a_catalog_of_only_other_is_hidden() {
    let tree = catalog(NL).tree(4).unwrap();
    let hidden = portolan::hidden_collections(&tree);
    assert_eq!(hidden.len(), 7, "Rijkswaterstaat's seven PDOK-terms collections: {hidden:?}");
    assert!(hidden.iter().all(|(_, l)| l == "other"));
    assert_eq!(portolan::visible_collections(&tree).len(), 27);
    assert!(portolan::catalog_visible(&tree));
    // Rijkswaterstaat keeps only its CC0 road network.
    let rws = tree.find("rijkswaterstaat").unwrap();
    let shown: Vec<&str> = portolan::visible_children(rws).iter().map(|c| c.node.id.as_str()).collect();
    assert_eq!(shown, ["nwb_wegen"]);
    assert_eq!(portolan::visible_children(&tree).len(), 8);

    let trimet = catalog(TRIMET).tree(3).unwrap();
    assert_eq!(trimet.collections().len(), 8);
    assert!(!portolan::catalog_visible(&trimet), "TriMet's licence is other: no card until a person reads the terms");
    assert!(trimet.collections()[0].node.licence.terms_url.as_deref().is_some_and(|u| u.contains("trimet.org")));
}

#[test]
fn an_offline_catalog_reads_from_the_pack_and_says_so() {
    let pack = Arc::new(|url: &str| -> Option<Vec<u8>> {
        let path = url.strip_prefix("https://data.source.coop/")?;
        std::fs::read(fixtures().join(path.replace('/', "__"))).ok()
    });
    let cat = Catalog::new(NL, Arc::new(HttpCache::new(None, None, Duration::ZERO).with_pack(pack)));
    let tree = cat.tree(4).unwrap();
    assert_eq!(tree.collections().len(), 34);
    assert!(tree.all_recorded() && !tree.any_live());
    let path: Vec<&str> = tree.path_to("natura2000").unwrap().iter().map(|t| t.node.id.as_str()).collect();
    assert_eq!(path, ["portolan-nl", "rvo", "natura2000"]);
}
