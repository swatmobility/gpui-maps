//! A catalog read lazily through the cache: each document is fetched the
//! first time something asks for it, parsed once, and kept.
//!
//! [`Catalog::node`] reads one document. [`Catalog::children`] reads a
//! node's `child` links (in parallel, bounded by the cache's request
//! limit). [`Catalog::tree`] walks down to the collections (never into
//! items, nor into what a collection holds as children) for a listing.
//! [`Catalog::text`] reads a README.md or AGENTS.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::cache::{HttpCache, Origin};
use crate::model::{Node, NodeKind};

/// A child link and its node (or why it could not be read).
pub type Child = (String, Result<(Arc<Node>, Origin), String>);

pub struct Catalog {
    pub root_url: String,
    cache: Arc<HttpCache>,
    nodes: Mutex<HashMap<String, (Arc<Node>, Origin)>>,
}

/// A walked tree: a node and what it holds.
#[derive(Debug, Clone)]
pub struct Tree {
    pub node: Arc<Node>,
    pub origin: Origin,
    pub children: Vec<Tree>,
    /// Child links that could not be read (`(href, why)`).
    pub errors: Vec<(String, String)>,
}

impl Tree {
    /// Every collection under this tree, depth first.
    pub fn collections(&self) -> Vec<&Tree> {
        let mut out = Vec::new();
        self.visit(&mut |t| {
            if t.node.kind == NodeKind::Collection {
                out.push(t);
            }
        });
        out
    }

    fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Tree)) {
        f(self);
        for c in &self.children {
            c.visit(f);
        }
    }

    /// Every node's warnings, with the node's id.
    pub fn warnings(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.visit(&mut |t| out.extend(t.node.warnings.iter().map(|w| (t.node.id.clone(), w.message.clone()))));
        out
    }

    /// Whether any document came from the network just now.
    pub fn any_live(&self) -> bool {
        let mut live = false;
        self.visit(&mut |t| live |= t.origin.is_live());
        live
    }

    /// Whether every document came from the recorded pack or old copies.
    pub fn all_recorded(&self) -> bool {
        let mut recorded = true;
        self.visit(&mut |t| recorded &= matches!(t.origin, Origin::Pack | Origin::Stale));
        recorded
    }

    pub fn find(&self, id: &str) -> Option<&Tree> {
        let mut hit = None;
        self.visit(&mut |t| {
            if hit.is_none() && t.node.id == id {
                hit = Some(t);
            }
        });
        hit
    }

    /// The path of ids from this tree down to `id` (inclusive).
    pub fn path_to(&self, id: &str) -> Option<Vec<&Tree>> {
        if self.node.id == id {
            return Some(vec![self]);
        }
        for c in &self.children {
            if let Some(mut p) = c.path_to(id) {
                p.insert(0, self);
                return Some(p);
            }
        }
        None
    }
}

impl Catalog {
    pub fn new(root_url: &str, cache: Arc<HttpCache>) -> Catalog {
        Catalog { root_url: root_url.to_string(), cache, nodes: Mutex::new(HashMap::new()) }
    }

    pub fn cache(&self) -> &HttpCache {
        &self.cache
    }

    pub fn root(&self) -> Result<(Arc<Node>, Origin), String> {
        self.node(&self.root_url.clone())
    }

    /// The document at `url`, read once.
    pub fn node(&self, url: &str) -> Result<(Arc<Node>, Origin), String> {
        if let Some(hit) = self.nodes.lock().unwrap_or_else(|e| e.into_inner()).get(url) {
            return Ok(hit.clone());
        }
        let fetched = self.cache.get(url)?;
        let node = Arc::new(Node::parse(url, &fetched.bytes)?);
        let entry = (node, fetched.origin);
        self.nodes.lock().unwrap_or_else(|e| e.into_inner()).insert(url.to_string(), entry.clone());
        Ok(entry)
    }

    /// A node's children, in link order: each `Ok` or the reason it could
    /// not be read. Read on up to four threads (the cache's limit).
    pub fn children(&self, node: &Node) -> Vec<Child> {
        let hrefs: Vec<String> = node.child_links().iter().map(|l| l.href.clone()).collect();
        let mut out: Vec<Child> = Vec::with_capacity(hrefs.len());
        std::thread::scope(|s| {
            let handles: Vec<_> = hrefs.iter().map(|h| s.spawn(move || self.node(h))).collect();
            for (h, handle) in hrefs.iter().zip(handles) {
                let r = handle.join().unwrap_or_else(|_| Err("reader panicked".into()));
                out.push((h.clone(), r));
            }
        });
        out
    }

    /// The tree from the root down to the collections, at most `depth`
    /// levels of catalogs deep.
    pub fn tree(&self, depth: usize) -> Result<Tree, String> {
        let (root, origin) = self.root()?;
        Ok(self.walk(root, origin, depth))
    }

    fn walk(&self, node: Arc<Node>, origin: Origin, depth: usize) -> Tree {
        let mut tree = Tree { node: node.clone(), origin, children: Vec::new(), errors: Vec::new() };
        // Collections are leaves for a listing: what a collection holds as
        // children (portolan-nl's aerial photo map sheets) is items' business.
        if node.kind != NodeKind::Catalog || depth == 0 {
            return tree;
        }
        for (href, r) in self.children(&node) {
            match r {
                Ok((child, o)) => tree.children.push(self.walk(child, o, depth - 1)),
                Err(e) => tree.errors.push((href, e)),
            }
        }
        tree
    }

    /// A text document (README.md, AGENTS.md).
    pub fn text(&self, url: &str) -> Result<(String, Origin), String> {
        let f = self.cache.get(url)?;
        Ok((String::from_utf8_lossy(&f.bytes).into_owned(), f.origin))
    }

    /// Bytes of any document (a style, a thumbnail).
    pub fn bytes(&self, url: &str) -> Result<(Arc<Vec<u8>>, Origin), String> {
        let f = self.cache.get(url)?;
        Ok((f.bytes, f.origin))
    }
}
