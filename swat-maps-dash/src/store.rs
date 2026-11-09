//! Where pages and each user's view of them live.
//!
//! A [`PageStore`] lists, reads, saves, duplicates and deletes pages, each
//! with a store envelope ([`PageMeta`]: id, title, version, updated time,
//! owner). The page JSON itself is unchanged: the envelope sits beside it,
//! so the page format gains nothing and a page shared as a file stays a
//! plain page.
//!
//! [`LocalPageStore`] is today's implementation: built-in pages (read-only;
//! users edit a duplicate) plus a folder of the user's own pages. A
//! hosted store (teams, roles, history) can implement the same trait,
//! behind the same Dashboards list.
//!
//! [`ViewState`] is per user and per page: the applied filter values, the
//! selections, the scroll offset and the per-user source overrides (which
//! connection name a page source reads). It is stored apart from the page,
//! keyed by page id, so sharing or duplicating a page never carries it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A page's store envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMeta {
    /// The page id; the same as the page's own `id`.
    pub id: String,
    pub title: String,
    /// Increments on every save; a save names the version it edited.
    pub version: u64,
    /// Seconds since the Unix epoch.
    pub updated_at: u64,
    /// Who owns it: `"built-in"`, or the local user (`"local"`). A Pro store
    /// puts a user or team id here.
    pub owner: String,
    /// Built-ins cannot be saved over or deleted, only duplicated.
    pub built_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

/// A page with its envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPage {
    pub meta: PageMeta,
    pub json: String,
}

/// Why a store operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("there is no page \"{0}\"")]
    NotFound(String),
    #[error("\"{0}\" is a built-in page; duplicate it to edit")]
    ReadOnly(String),
    #[error("\"{id}\" changed since it was opened (version {current}, not {expected})")]
    Conflict { id: String, expected: u64, current: u64 },
    #[error("\"{0}\" is not a usable page id")]
    BadId(String),
    #[error("the page is not valid JSON with an \"id\": {0}")]
    BadPage(String),
    #[error("the store could not be read or written: {0}")]
    Io(String),
}

/// Pages behind the Dashboards list.
pub trait PageStore: Send + Sync {
    /// Every page's envelope, built-ins first, then by group, order, title.
    fn list(&self) -> Result<Vec<PageMeta>, StoreError>;
    fn get(&self, id: &str) -> Result<StoredPage, StoreError>;
    /// Saves an edited page over version `expected`; the page's `id` must be
    /// `id`. Refused for built-ins and on a version conflict.
    fn save(&self, id: &str, json: &str, expected: u64) -> Result<PageMeta, StoreError>;
    /// Copies a page under a new id (`<id>-copy`, `-copy-2`, …) owned by the
    /// user, with the page's own `id` rewritten. View state is not copied.
    fn duplicate(&self, id: &str) -> Result<PageMeta, StoreError>;
    /// Deletes a user page and its view state. Refused for built-ins.
    fn delete(&self, id: &str) -> Result<(), StoreError>;
}

/// `[a-z0-9_-]{1,80}`, so an id is always a safe file name.
pub fn is_page_id(s: &str) -> bool {
    (1..=80).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Reads the envelope fields a page carries: `id`, `title` (`en`, else any
/// locale), `group` and `order`.
pub fn meta_of(json: &str, owner: &str, version: u64, updated_at: u64, built_in: bool) -> Result<PageMeta, StoreError> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| StoreError::BadPage(e.to_string()))?;
    let id = v.get("id").and_then(|i| i.as_str()).ok_or_else(|| StoreError::BadPage("no id".into()))?.to_string();
    let title = match v.get("title") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Object(m)) => {
            m.get("en").or_else(|| m.values().next()).and_then(|t| t.as_str()).unwrap_or(&id).to_string()
        }
        _ => id.clone(),
    };
    Ok(PageMeta {
        id,
        title,
        version,
        updated_at,
        owner: owner.to_string(),
        built_in,
        group: v.get("group").and_then(|g| g.as_str()).map(String::from),
        order: v.get("order").and_then(serde_json::Value::as_i64),
    })
}

/// One user's view of one page. Never written into the page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    /// Applied (committed) variable values, as page `default`s are written:
    /// `"all"`, a scalar, an array, or `{ "lo": …, "hi": … }`.
    #[serde(default)]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Vertical scroll of the page, in logical px.
    #[serde(default)]
    pub scroll_y: f32,
    /// Page source name → the connection name this user reads it through,
    /// replacing the page's `connection` for this user only.
    #[serde(default)]
    pub source_overrides: BTreeMap<String, String>,
}

/// Where view state lives.
pub trait ViewStateStore: Send + Sync {
    fn load(&self, page_id: &str) -> Option<ViewState>;
    fn save(&self, page_id: &str, state: &ViewState) -> Result<(), StoreError>;
    fn forget(&self, page_id: &str);
}

#[cfg(feature = "fs")]
fn ordered(mut v: Vec<PageMeta>) -> Vec<PageMeta> {
    v.sort_by(|a, b| {
        b.built_in
            .cmp(&a.built_in)
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| a.order.unwrap_or(i64::MAX).cmp(&b.order.unwrap_or(i64::MAX)))
            .then_with(|| a.title.cmp(&b.title))
    });
    v
}

/// Rewrites a page's own `id`.
#[cfg(feature = "fs")]
fn with_id(json: &str, id: &str) -> Result<String, StoreError> {
    let mut v: serde_json::Value = serde_json::from_str(json).map_err(|e| StoreError::BadPage(e.to_string()))?;
    let obj = v.as_object_mut().ok_or_else(|| StoreError::BadPage("not an object".into()))?;
    obj.insert("id".into(), serde_json::Value::String(id.to_string()));
    serde_json::to_string_pretty(&v).map_err(|e| StoreError::BadPage(e.to_string()))
}

#[cfg(feature = "fs")]
pub use local::{LocalPageStore, LocalViewStateStore};

#[cfg(feature = "fs")]
mod local {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn io(e: std::io::Error) -> StoreError {
        StoreError::Io(e.to_string())
    }

    /// Built-in pages in memory plus the user's pages in `<dir>/pages/`:
    /// `<id>.json` (the page as written) and `<id>.meta.json` (its envelope).
    pub struct LocalPageStore {
        dir: PathBuf,
        owner: String,
        built_ins: Vec<StoredPage>,
        /// Serialises writes from one process.
        lock: Mutex<()>,
    }

    #[derive(Serialize, Deserialize)]
    struct Envelope {
        version: u64,
        updated_at: u64,
        owner: String,
    }

    impl LocalPageStore {
        /// A store over `dir` (created on first save) with these built-in
        /// pages, each `(json)`; a built-in that does not parse is skipped.
        pub fn new(dir: impl Into<PathBuf>, built_ins: impl IntoIterator<Item = String>) -> Self {
            let built_ins = built_ins
                .into_iter()
                .filter_map(|json| meta_of(&json, "built-in", 1, 0, true).ok().map(|meta| StoredPage { meta, json }))
                .collect();
            LocalPageStore { dir: dir.into(), owner: "local".into(), built_ins, lock: Mutex::new(()) }
        }

        /// Built-ins from every `*.json` directly in `folder`.
        pub fn built_ins_from(folder: &Path) -> Vec<String> {
            let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect())
                .unwrap_or_default();
            files.sort();
            files.iter().filter_map(|f| std::fs::read_to_string(f).ok()).collect()
        }

        fn pages(&self) -> PathBuf {
            self.dir.join("pages")
        }

        fn page_path(&self, id: &str) -> PathBuf {
            self.pages().join(format!("{id}.json"))
        }

        fn meta_path(&self, id: &str) -> PathBuf {
            self.pages().join(format!("{id}.meta.json"))
        }

        fn built_in(&self, id: &str) -> Option<&StoredPage> {
            self.built_ins.iter().find(|p| p.meta.id == id)
        }

        fn user_page(&self, id: &str) -> Result<Option<StoredPage>, StoreError> {
            if !is_page_id(id) {
                return Err(StoreError::BadId(id.into()));
            }
            let Ok(json) = std::fs::read_to_string(self.page_path(id)) else { return Ok(None) };
            let env: Envelope = std::fs::read_to_string(self.meta_path(id))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Envelope { version: 1, updated_at: 0, owner: self.owner.clone() });
            let meta = meta_of(&json, &env.owner, env.version, env.updated_at, false)?;
            Ok(Some(StoredPage { meta, json }))
        }

        fn write(&self, id: &str, json: &str, version: u64) -> Result<PageMeta, StoreError> {
            std::fs::create_dir_all(self.pages()).map_err(io)?;
            let env = Envelope { version, updated_at: now(), owner: self.owner.clone() };
            std::fs::write(self.page_path(id), json).map_err(io)?;
            let env_text = serde_json::to_string_pretty(&env).map_err(|e| StoreError::Io(e.to_string()))?;
            std::fs::write(self.meta_path(id), env_text).map_err(io)?;
            meta_of(json, &env.owner, env.version, env.updated_at, false)
        }

        fn exists(&self, id: &str) -> bool {
            self.built_in(id).is_some() || self.page_path(id).exists()
        }
    }

    impl PageStore for LocalPageStore {
        fn list(&self) -> Result<Vec<PageMeta>, StoreError> {
            let mut out: Vec<PageMeta> = self.built_ins.iter().map(|p| p.meta.clone()).collect();
            if let Ok(rd) = std::fs::read_dir(self.pages()) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let Some(id) = name.strip_suffix(".json").filter(|s| !s.ends_with(".meta")) else { continue };
                    if let Ok(Some(p)) = self.user_page(id) {
                        out.push(p.meta);
                    }
                }
            }
            Ok(ordered(out))
        }

        fn get(&self, id: &str) -> Result<StoredPage, StoreError> {
            if let Some(p) = self.built_in(id) {
                return Ok(p.clone());
            }
            self.user_page(id)?.ok_or_else(|| StoreError::NotFound(id.into()))
        }

        fn save(&self, id: &str, json: &str, expected: u64) -> Result<PageMeta, StoreError> {
            let _g = self.lock.lock().map_err(|_| StoreError::Io("poisoned".into()))?;
            if self.built_in(id).is_some() {
                return Err(StoreError::ReadOnly(id.into()));
            }
            let current = self.user_page(id)?.ok_or_else(|| StoreError::NotFound(id.into()))?;
            if current.meta.version != expected {
                return Err(StoreError::Conflict { id: id.into(), expected, current: current.meta.version });
            }
            let written = meta_of(json, &self.owner, 0, 0, false)?;
            if written.id != id {
                return Err(StoreError::BadPage(format!("the page's id is \"{}\", not \"{id}\"", written.id)));
            }
            self.write(id, json, expected + 1)
        }

        fn duplicate(&self, id: &str) -> Result<PageMeta, StoreError> {
            let src = self.get(id)?;
            let _g = self.lock.lock().map_err(|_| StoreError::Io("poisoned".into()))?;
            let base = format!("{}-copy", id.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-').trim_end_matches("-copy"));
            let mut new_id = base.clone();
            let mut n = 2;
            while self.exists(&new_id) {
                new_id = format!("{base}-{n}");
                n += 1;
            }
            if !is_page_id(&new_id) {
                return Err(StoreError::BadId(new_id));
            }
            let json = with_id(&src.json, &new_id)?;
            self.write(&new_id, &json, 1)
        }

        fn delete(&self, id: &str) -> Result<(), StoreError> {
            if self.built_in(id).is_some() {
                return Err(StoreError::ReadOnly(id.into()));
            }
            if self.user_page(id)?.is_none() {
                return Err(StoreError::NotFound(id.into()));
            }
            std::fs::remove_file(self.page_path(id)).map_err(io)?;
            let _ = std::fs::remove_file(self.meta_path(id));
            LocalViewStateStore::new(&self.dir).forget(id);
            Ok(())
        }
    }

    /// View state in `<dir>/view-state/<page id>.json`.
    pub struct LocalViewStateStore {
        dir: PathBuf,
    }

    impl LocalViewStateStore {
        pub fn new(dir: impl AsRef<Path>) -> Self {
            LocalViewStateStore { dir: dir.as_ref().join("view-state") }
        }

        fn path(&self, id: &str) -> Option<PathBuf> {
            is_page_id(id).then(|| self.dir.join(format!("{id}.json")))
        }
    }

    impl ViewStateStore for LocalViewStateStore {
        fn load(&self, page_id: &str) -> Option<ViewState> {
            let text = std::fs::read_to_string(self.path(page_id)?).ok()?;
            serde_json::from_str(&text).ok()
        }

        fn save(&self, page_id: &str, state: &ViewState) -> Result<(), StoreError> {
            let path = self.path(page_id).ok_or_else(|| StoreError::BadId(page_id.into()))?;
            std::fs::create_dir_all(&self.dir).map_err(io)?;
            let text = serde_json::to_string_pretty(state).map_err(|e| StoreError::Io(e.to_string()))?;
            std::fs::write(path, text).map_err(io)
        }

        fn forget(&self, page_id: &str) {
            if let Some(p) = self.path(page_id) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;

    const PAGE: &str = r#"{ "dashboard": 1, "id": "boardings", "title": { "ja": "乗車", "en": "Boardings" }, "group": "Ridership", "order": 3 }"#;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dash-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn duplicate_then_edit_leaves_the_built_in_unchanged() {
        let dir = temp("dup");
        let store = LocalPageStore::new(&dir, [PAGE.to_string()]);
        assert_eq!(store.save("boardings", PAGE, 1), Err(StoreError::ReadOnly("boardings".into())));
        let copy = store.duplicate("boardings").unwrap();
        assert_eq!(copy.id, "boardings-copy");
        assert!(!copy.built_in);
        let edited = store.get(&copy.id).unwrap().json.replace("Boardings", "My boardings");
        let saved = store.save(&copy.id, &edited, 1).unwrap();
        assert_eq!(saved.version, 2);
        assert_eq!(saved.title, "My boardings");
        assert_eq!(store.get("boardings").unwrap().json, PAGE, "the built-in is untouched");
        assert_eq!(store.duplicate("boardings").unwrap().id, "boardings-copy-2");
        assert!(matches!(store.save(&copy.id, &edited, 1), Err(StoreError::Conflict { current: 2, .. })));
        assert_eq!(store.list().unwrap().len(), 3);
        store.delete(&copy.id).unwrap();
        assert_eq!(store.delete("boardings"), Err(StoreError::ReadOnly("boardings".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_state_persists_apart_from_the_page_and_is_not_duplicated() {
        let dir = temp("view");
        let store = LocalPageStore::new(&dir, [PAGE.to_string()]);
        let views = LocalViewStateStore::new(&dir);
        let mut vs = ViewState::default();
        vs.values.insert("route".into(), serde_json::json!(["系統10"]));
        vs.scroll_y = 240.0;
        vs.source_overrides.insert("ridership".into(), "my-clickhouse".into());
        views.save("boardings", &vs).unwrap();
        // Reopen: a fresh store reads the same state.
        assert_eq!(LocalViewStateStore::new(&dir).load("boardings"), Some(vs));
        assert!(!store.get("boardings").unwrap().json.contains("my-clickhouse"));
        let copy = store.duplicate("boardings").unwrap();
        assert_eq!(views.load(&copy.id), None, "a duplicate starts with no view state");
        assert!(!store.get(&copy.id).unwrap().json.contains("系統10"));
        assert!(views.save("../escape", &ViewState::default()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
