//! The project file on disk (spec §6.3, §6.4): where a source's data file
//! is written relative to the document, how a reopened document finds it
//! again, the content fingerprint that says it changed, and the version
//! migrations. Pure: the host reads the files and passes in their bytes,
//! the document's folder and the home folder.
//!
//! **Paths.** In memory a file source holds the absolute path it was
//! loaded from. A saved document holds, in order of preference:
//!
//! - a path relative to the document's folder (`data/trips.csv`,
//!   `../shared/trips.csv`), when the two share a folder below the root;
//! - else, under the home folder, a `~/` path, which names no user;
//! - else the absolute path (another volume), and the save says so.
//!
//! The save's notices name each source that is not written relative, so
//! a document about to be shared never carries a folder name unnoticed.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::doc::{DataRef, Doc, DocError, MAPDOC_VERSION};

/// Content fingerprint of a data file: lowercase hex SHA-256.
pub fn fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `path` with `.` and `..` folded away lexically (no file system access).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// How a saved path was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathStyle {
    /// Relative to the document's folder.
    Relative,
    /// `~/…`: under the home folder, outside the document's tree.
    Home,
    /// A full path: another volume.
    Absolute,
}

/// The string a document stores for the data file at `data` (absolute),
/// written from a document in `doc_dir` (absolute), with `home` the user's
/// home folder when known. Separators are always `/`.
pub fn stored_path(doc_dir: &Path, data: &Path, home: Option<&Path>) -> (String, PathStyle) {
    let (doc_dir, data) = (normalize(doc_dir), normalize(data));
    let a: Vec<Component> = doc_dir.components().collect();
    let b: Vec<Component> = data.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    // Sharing only the root (or the root and `/Users`, `/Volumes`, `/home`)
    // is not sharing a tree: a `../../Users/<name>/…` path would name the
    // user anyway.
    let shared_tree = common >= 3
        || (common == 2
            && !matches!(
                a.get(1).and_then(|c| c.as_os_str().to_str()),
                Some("Users" | "Volumes" | "home" | "private")
            ));
    if shared_tree {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(std::iter::repeat_n("..".to_string(), a.len() - common));
        parts.extend(
            b[common..]
                .iter()
                .map(|c| c.as_os_str().to_string_lossy().into_owned()),
        );
        return (parts.join("/"), PathStyle::Relative);
    }
    if let Some(home) = home.map(normalize) {
        if let Ok(rest) = data.strip_prefix(&home) {
            let parts: Vec<String> = rest
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            return (format!("~/{}", parts.join("/")), PathStyle::Home);
        }
    }
    (
        data.to_string_lossy().replace('\\', "/"),
        PathStyle::Absolute,
    )
}

/// The absolute path a stored path names, read from a document in
/// `doc_dir`.
pub fn resolve_path(doc_dir: &Path, stored: &str, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (stored.strip_prefix("~/"), home) {
        return normalize(&home.join(rest));
    }
    let p = Path::new(stored);
    if p.is_absolute() {
        return normalize(p);
    }
    normalize(&doc_dir.join(p))
}

/// Something the user is told when a document is saved or opened. The
/// text follows the copy rule: what happened, then what to do, at most
/// 200 characters; it names files by their file name only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The source it is about.
    pub source: String,
    pub kind: NoticeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// Saved as a `~/` or full path: folder names are in the document.
    NotRelative { file: String, style: PathStyle },
    /// Opened from a document that stored a full path.
    StoredAbsolute { file: String },
    /// The data file is not where the document says.
    Missing { file: String },
    /// The data file's content is not what was saved.
    Changed { file: String },
}

impl Notice {
    pub fn text(&self) -> String {
        match &self.kind {
            NoticeKind::NotRelative { file, style: PathStyle::Home } => {
                format!("{file} is outside the project's folder, so its path from your home folder is saved. Move it beside the project to keep folder names out.")
            }
            NoticeKind::NotRelative { file, .. } => format!("{file} is on another disk, so its full path is saved. Move it beside the project before sharing it."),
            NoticeKind::StoredAbsolute { file } => format!("This project stores the full path of {file}. Save it again to store the path relative to the project."),
            NoticeKind::Missing { file } => format!("Data file not found: {file}. Locate it to draw its layers again."),
            NoticeKind::Changed { file } => format!("{file} changed since the project was saved. Continue with the new data, or locate the original."),
        }
    }
}

fn file_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

impl Doc {
    /// The document as it is written into `doc_dir`: file sources' paths
    /// stored per [`stored_path`]. Returns the notices the save shows.
    pub fn for_save(&self, doc_dir: &Path, home: Option<&Path>) -> (Doc, Vec<Notice>) {
        let mut out = self.clone();
        let mut notices = Vec::new();
        for (id, source) in out.sources.iter_mut() {
            let data = &mut source.data;
            if data.kind != "file" {
                continue;
            }
            let Some(path) = data.path.clone() else {
                continue;
            };
            let p = Path::new(&path);
            if !p.is_absolute() {
                continue;
            }
            let (stored, style) = stored_path(doc_dir, p, home);
            if style != PathStyle::Relative {
                notices.push(Notice {
                    source: id.clone(),
                    kind: NoticeKind::NotRelative {
                        file: file_name(&path),
                        style,
                    },
                });
            }
            data.path = Some(stored);
        }
        (out, notices)
    }

    /// Resolves every file source of a document read from `doc_dir` to an
    /// absolute path, in place. Returns, by source id, the notice for a
    /// document that stored a full path.
    pub fn resolve_paths(&mut self, doc_dir: &Path, home: Option<&Path>) -> Vec<Notice> {
        let mut notices = Vec::new();
        for (id, source) in self.sources.iter_mut() {
            let data: &mut DataRef = &mut source.data;
            if data.kind != "file" {
                continue;
            }
            let Some(stored) = data.path.clone() else {
                continue;
            };
            if Path::new(&stored).is_absolute() {
                notices.push(Notice {
                    source: id.clone(),
                    kind: NoticeKind::StoredAbsolute {
                        file: file_name(&stored),
                    },
                });
            }
            data.path = Some(
                resolve_path(doc_dir, &stored, home)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        notices
    }
}

/// What the host found when it read a file source's bytes on open.
pub fn check_file(source: &str, data: &DataRef, read: Option<&[u8]>) -> Option<Notice> {
    let file = file_name(data.path.as_deref().unwrap_or(""));
    match read {
        None => Some(Notice {
            source: source.into(),
            kind: NoticeKind::Missing { file },
        }),
        Some(bytes) => {
            let changed = data.bytes.is_some_and(|n| n != bytes.len() as u64)
                || data
                    .sha256
                    .as_deref()
                    .is_some_and(|want| want != fingerprint(bytes));
            changed.then(|| Notice {
                source: source.into(),
                kind: NoticeKind::Changed { file },
            })
        }
    }
}

/// Brings a document of an older version up to [`MAPDOC_VERSION`], as
/// JSON, one step per version (spec §6.4). Version 1 is the first, so
/// today this only checks the version; the next format change adds a
/// `1 => 2` step here, with a fixture before and after.
pub fn migrate(mut value: serde_json::Value) -> Result<serde_json::Value, DocError> {
    let version = value
        .get("mapdoc")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| DocError::Shape("it has no \"mapdoc\" version".into()))?;
    let version = u32::try_from(version).unwrap_or(u32::MAX);
    if version > MAPDOC_VERSION {
        return Err(DocError::Version(version));
    }
    if version == 0 {
        return Err(DocError::Shape(
            "version 0 is not a map document version".into(),
        ));
    }
    // `match version { 1 => step_1_to_2(&mut value), .. }` goes here.
    angles_to_degrees(&mut value);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("mapdoc".into(), MAPDOC_VERSION.into());
    }
    Ok(value)
}

/// Pitch at or above this is degrees whatever the document says: a radian
/// pitch is at most 85 deg = 1.48 rad, and a tilt under 1.6 deg is flat to
/// the eye, so reading one as radians loses nothing a user chose.
const RADIAN_PITCH_MAX: f64 = 1.6;

/// Documents written before `map.view.angles` existed saved the camera's
/// pitch and bearing in radians, and reopen nearly flat and north-up when
/// read as degrees. A view without the key whose pitch is in `(0, 1.6)`,
/// or whose pitch is 0 and bearing is a non-zero angle within one turn of
/// radians, is converted; every view leaves with `"angles": "degrees"`, so
/// a document is migrated once and a new one never is.
fn angles_to_degrees(value: &mut serde_json::Value) {
    let Some(view) = value
        .pointer_mut("/map/view")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if !view.contains_key("angles") {
        let num = |k: &str| {
            view.get(k)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
        };
        let (pitch, bearing) = (num("pitch"), num("bearing"));
        let radian_pitch = pitch > 0.0 && pitch < RADIAN_PITCH_MAX;
        let radian_bearing = bearing != 0.0 && bearing.abs() <= std::f64::consts::TAU + 1e-9;
        if radian_pitch || (pitch == 0.0 && radian_bearing) {
            view.insert("pitch".into(), pitch.to_degrees().into());
            if radian_bearing {
                view.insert("bearing".into(), bearing.to_degrees().into());
            }
        }
    }
    view.insert("angles".into(), crate::doc::ANGLES_DEGREES.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Source;

    fn source(path: &str) -> Source {
        Source {
            kind: "csv".into(),
            data: DataRef::file(path),
            label: "t".into(),
            columns: Default::default(),
            geometry: Default::default(),
            attribution: None,
            extra: Default::default(),
        }
    }

    const HOME: &str = "/Users/someone";

    #[test]
    fn relative_paths_are_written_when_the_data_is_beside_the_doc() {
        let home = Some(Path::new(HOME));
        let dir = Path::new("/Users/someone/clients/acme");
        assert_eq!(
            stored_path(
                dir,
                Path::new("/Users/someone/clients/acme/trips.csv"),
                home
            ),
            ("trips.csv".into(), PathStyle::Relative)
        );
        assert_eq!(
            stored_path(
                dir,
                Path::new("/Users/someone/clients/acme/data/trips.csv"),
                home
            ),
            ("data/trips.csv".into(), PathStyle::Relative)
        );
        assert_eq!(
            stored_path(dir, Path::new("/Users/someone/shared/trips.csv"), home),
            ("../../shared/trips.csv".into(), PathStyle::Relative)
        );
        // Sharing only /Users is not a shared tree: the path would name a user.
        assert_eq!(
            stored_path(
                Path::new("/Users/other/x"),
                Path::new("/Users/someone/t.csv"),
                home
            ),
            ("~/t.csv".into(), PathStyle::Home)
        );
        assert_eq!(
            stored_path(
                Path::new("/private/tmp/p"),
                Path::new("/Users/someone/d/t.csv"),
                home
            ),
            ("~/d/t.csv".into(), PathStyle::Home)
        );
        assert_eq!(
            stored_path(dir, Path::new("/Volumes/Ext/t.csv"), home),
            ("/Volumes/Ext/t.csv".into(), PathStyle::Absolute)
        );

        let mut doc = Doc::default();
        doc.sources.insert(
            "t".into(),
            source("/Users/someone/clients/acme/data/trips.csv"),
        );
        let (saved, notices) = doc.for_save(dir, home);
        assert!(notices.is_empty());
        let text = saved.to_json();
        assert!(
            text.contains("\"path\": \"data/trips.csv\"") && !text.contains("someone"),
            "{text}"
        );
        // Open resolves it back; a second save writes the same bytes.
        let mut opened = Doc::from_json(&text).unwrap();
        assert!(opened.resolve_paths(dir, home).is_empty());
        assert_eq!(
            opened.sources["t"].data.path.as_deref(),
            Some("/Users/someone/clients/acme/data/trips.csv")
        );
        assert_eq!(opened.for_save(dir, home).0.to_json(), text);
    }

    #[test]
    fn a_path_outside_the_tree_is_saved_with_a_notice_naming_the_file_only() {
        let mut doc = Doc::default();
        doc.sources
            .insert("t".into(), source("/Volumes/Ext/clients/acme/trips.csv"));
        let (_, notices) = doc.for_save(Path::new("/Users/someone/p"), Some(Path::new(HOME)));
        assert_eq!(notices.len(), 1);
        let text = notices[0].text();
        assert!(
            text.contains("trips.csv") && !text.contains("acme") && text.len() <= 200,
            "{text}"
        );
    }

    #[test]
    fn a_home_path_resolves_under_this_home() {
        assert_eq!(
            resolve_path(Path::new("/x"), "~/d/t.csv", Some(Path::new("/Users/b"))),
            PathBuf::from("/Users/b/d/t.csv")
        );
        assert_eq!(
            resolve_path(Path::new("/a/b"), "../c/t.csv", None),
            PathBuf::from("/a/c/t.csv")
        );
    }

    #[test]
    fn a_document_with_absolute_paths_opens_with_a_notice() {
        let mut doc = Doc::default();
        doc.sources
            .insert("t".into(), source("/Users/someone/trips.csv"));
        let notices = doc.resolve_paths(Path::new("/Users/someone/p"), None);
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0].kind, NoticeKind::StoredAbsolute { file } if file == "trips.csv")
        );
        assert_eq!(
            doc.sources["t"].data.path.as_deref(),
            Some("/Users/someone/trips.csv"),
            "the path is still used"
        );
    }

    #[test]
    fn a_missing_file_reference_is_a_warning_not_an_error() {
        let text = r#"{"mapdoc":1,"profile":"map","sources":{"t":{"kind":"csv","data":{"ref":"file","path":"gone/trips.csv"}}},
            "map":{"view":{"lon":0,"lat":0,"zoom":1},"layers":[{"id":"t-points","type":"points","source":"t","vis":{"radius":10,"opacity":0.8,"color":[1,2,3]}}]}}"#;
        let mut doc = Doc::from_json(text).expect("a missing file does not stop the parse");
        doc.resolve_paths(Path::new("/p"), None);
        let n = check_file("t", &doc.sources["t"].data, None).expect("a notice");
        assert!(
            n.text().starts_with("Data file not found: trips.csv."),
            "{}",
            n.text()
        );
        assert_eq!(doc.map.layers.len(), 1, "the layer is kept");
    }

    #[test]
    fn a_changed_file_is_detected_by_hash() {
        let bytes = b"lat,lon\n1,2\n";
        let mut data = DataRef::file("/p/t.csv");
        data.sha256 = Some(fingerprint(bytes));
        data.bytes = Some(bytes.len() as u64);
        assert_eq!(check_file("t", &data, Some(bytes)), None);
        let n =
            check_file("t", &data, Some(b"lat,lon\n1,3\n")).expect("same length, other content");
        assert!(matches!(n.kind, NoticeKind::Changed { .. }));
        assert!(
            check_file("t", &data, Some(b"lat,lon\n")).is_some(),
            "other length"
        );
        assert_eq!(
            fingerprint(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn embedded_data_round_trips() {
        let mut doc = Doc::default();
        let csv = "name,lat,lon\n\"Zürich, CH\",47.37,8.54\nquote \"\"x\"\",1,2\n";
        let mut s = source("x");
        s.data = DataRef::embedded_text("cities.csv", csv);
        doc.sources.insert("c".into(), s);
        let text = doc.to_json();
        let back = Doc::from_json(&text).unwrap();
        assert_eq!(back.sources["c"].data.text.as_deref(), Some(csv));
        assert_eq!(back.sources["c"].data.encoding.as_deref(), Some("text"));
        assert_eq!(back.to_json(), text);
    }

    #[test]
    fn migrate_checks_the_version_and_keeps_a_v1_document() {
        let v1: serde_json::Value =
            serde_json::from_str(r#"{"mapdoc":1,"profile":"map","x":{"y":1}}"#).unwrap();
        assert_eq!(migrate(v1.clone()).unwrap(), v1);
        assert!(matches!(
            migrate(serde_json::json!({"mapdoc": 7})),
            Err(DocError::Version(7))
        ));
        assert!(migrate(serde_json::json!({"profile": "map"})).is_err());
    }

    fn view_of(view: &str) -> crate::doc::View {
        let text = format!(r#"{{"mapdoc": 1, "profile": "map", "map": {{"view": {view}}}}}"#);
        crate::doc::Doc::from_json(&text).expect("a doc").map.view
    }

    #[test]
    fn a_pre_fix_document_s_radian_pitch_and_bearing_open_as_degrees() {
        // Written before `angles`: 45 deg pitch and -15 deg bearing in radians.
        let v = view_of(
            r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 0.7853981633974483, "bearing": -0.2617993877991494}"#,
        );
        assert!(
            (v.pitch - 45.0).abs() < 1e-9 && (v.bearing + 15.0).abs() < 1e-9,
            "{v:?}"
        );
        assert_eq!(v.angles, "degrees");
        // A bearing alone (a flat map turned) is radians too.
        let v = view_of(
            r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 0, "bearing": 1.5707963267948966}"#,
        );
        assert!((v.bearing - 90.0).abs() < 1e-9, "{v:?}");
        // A flat north-up old document, and one already in degrees (pitch
        // above 1.6), are left as they are.
        let v = view_of(r#"{"lon": 0, "lat": 0, "zoom": 3}"#);
        assert_eq!((v.pitch, v.bearing), (0.0, 0.0));
        let v = view_of(r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 1.79, "bearing": -0.6}"#);
        assert_eq!((v.pitch, v.bearing), (1.79, -0.6));
        // Migrated once: the saved document says degrees and reopens the same.
        let mut doc = crate::doc::Doc::default();
        doc.map.view = view_of(r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 0.5, "bearing": 0.5}"#);
        let text = doc.to_json();
        assert!(text.contains("\"angles\": \"degrees\""), "{text}");
        assert_eq!(
            crate::doc::Doc::from_json(&text).unwrap().map.view,
            doc.map.view
        );
    }

    #[test]
    fn a_new_document_s_small_degrees_are_never_converted() {
        let v = view_of(
            r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 0.5, "bearing": 1.2, "angles": "degrees"}"#,
        );
        assert_eq!((v.pitch, v.bearing), (0.5, 1.2));
        let v = view_of(
            r#"{"lon": 0, "lat": 0, "zoom": 3, "pitch": 60, "bearing": -20, "angles": "degrees"}"#,
        );
        assert_eq!((v.pitch, v.bearing), (60.0, -20.0));
        // A new view writes the key.
        assert!(crate::doc::Doc::default()
            .to_json()
            .contains("\"angles\": \"degrees\""));
    }
}
