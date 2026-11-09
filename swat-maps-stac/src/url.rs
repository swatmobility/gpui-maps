//! Resolving a STAC `href` against the document it was read from.
//!
//! STAC links are relative to the JSON file that holds them (`./x.json`,
//! `../catalog.json`), or absolute. A style's `pmtiles://../x.pmtiles` is
//! relative to the style file. This is RFC 3986 reference resolution for
//! the shapes catalogs actually use: absolute URLs, absolute paths, and
//! dot-segment relative paths. Query strings and fragments are kept on the
//! reference and dropped from the base.

/// `href` resolved against the document at `base` (a URL or a path).
pub fn join(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return base.to_string();
    }
    if has_scheme(href) {
        return normalise(href);
    }
    let base = base.split(['?', '#']).next().unwrap_or(base);
    let (origin, path) = split_origin(base);
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = origin.split("://").next().unwrap_or("https");
        return normalise(&format!("{scheme}://{rest}"));
    }
    let joined = if href.starts_with('/') {
        href.to_string()
    } else {
        // The base's directory: everything up to its last '/'.
        let dir = match path.rfind('/') {
            Some(i) => &path[..=i],
            None => "",
        };
        format!("{dir}{href}")
    };
    format!("{origin}{}", remove_dots(&joined))
}

/// The directory of a document URL, with its trailing slash.
pub fn dir_of(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    match url.rfind('/') {
        Some(i) => url[..=i].to_string(),
        None => String::new(),
    }
}

/// The host of an absolute URL (`data.source.coop`), lowercase.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn has_scheme(s: &str) -> bool {
    match s.find(':') {
        Some(i) if i > 0 => s[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) && s[..i].chars().next().is_some_and(|c| c.is_ascii_alphabetic()),
        _ => false,
    }
}

/// `(scheme://authority, path)`; a bare path has no origin.
fn split_origin(url: &str) -> (&str, &str) {
    if let Some(i) = url.find("://") {
        let after = i + 3;
        match url[after..].find('/') {
            Some(j) => (&url[..after + j], &url[after + j..]),
            None => (url, "/"),
        }
    } else {
        ("", url)
    }
}

fn normalise(url: &str) -> String {
    let (head, tail) = match url.find(['?', '#']) {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    let (origin, path) = split_origin(head);
    if origin.is_empty() {
        return url.to_string();
    }
    format!("{origin}{}{tail}", remove_dots(path))
}

/// RFC 3986 §5.2.4 on a path: `a/./b/../c` -> `a/c`.
fn remove_dots(path: &str) -> String {
    let (path, tail) = match path.find(['?', '#']) {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    let absolute = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    let last = segments.len().saturating_sub(1);
    let mut trailing = false;
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." => trailing = i == last,
            ".." => {
                if out.last().is_some_and(|s| !s.is_empty()) {
                    out.pop();
                }
                trailing = i == last;
            }
            "" if i == 0 => out.push(""),
            s => {
                out.push(s);
                trailing = false;
            }
        }
    }
    let mut s = out.join("/");
    if absolute && !s.starts_with('/') {
        s.insert(0, '/');
    }
    if trailing && !s.ends_with('/') {
        s.push('/');
    }
    format!("{s}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/collection.json";

    #[test]
    fn relative_hrefs_resolve_against_the_document() {
        assert_eq!(join(BASE, "./natura2000.pmtiles"), "https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/natura2000.pmtiles");
        assert_eq!(join(BASE, "../catalog.json"), "https://data.source.coop/cholmes/portolan-nl/rvo/catalog.json");
        assert_eq!(join(BASE, "../../catalog.json"), "https://data.source.coop/cholmes/portolan-nl/catalog.json");
        assert_eq!(join(BASE, "styles/default.json"), "https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/styles/default.json");
        assert_eq!(join(BASE, "/x/y.json"), "https://data.source.coop/x/y.json");
        assert_eq!(join(BASE, "https://example.org/a/./b/../c.json"), "https://example.org/a/c.json");
        assert_eq!(join(BASE, "//example.org/a.json"), "https://example.org/a.json");
    }

    #[test]
    fn a_style_relative_pmtiles_path_climbs_out_of_styles() {
        let style = "https://data.source.coop/cholmes/portolan-nl/cbs/wijken_buurten/styles/default.json";
        assert_eq!(join(style, "../wijken_buurten.pmtiles"), "https://data.source.coop/cholmes/portolan-nl/cbs/wijken_buurten/wijken_buurten.pmtiles");
    }

    #[test]
    fn too_many_parents_stop_at_the_root_and_paths_work_too() {
        assert_eq!(join("https://h.org/a/b.json", "../../../c.json"), "https://h.org/c.json");
        assert_eq!(join("/tmp/pack/nl/catalog.json", "./rvo/catalog.json"), "/tmp/pack/nl/rvo/catalog.json");
        assert_eq!(join("https://h.org/a/b.json?x=1", "c.json"), "https://h.org/a/c.json");
    }

    #[test]
    fn hosts_and_dirs() {
        assert_eq!(host_of(BASE).as_deref(), Some("data.source.coop"));
        assert_eq!(host_of("https://User@Example.ORG:8443/x").as_deref(), Some("example.org"));
        assert_eq!(host_of("/local/path"), None);
        assert_eq!(dir_of(BASE), "https://data.source.coop/cholmes/portolan-nl/rvo/natura2000/");
    }
}
