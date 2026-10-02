use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::{
    compiler::{build_pattern, CompileError},
    draft::*,
    loader::DefaultUrlLoader,
    root::Root,
    util::*,
    validator::Budget,
};

use serde_json::Value;
use url::Url;

// --

pub(crate) struct Roots {
    pub(crate) default_draft: &'static Draft,
    pub(crate) required_draft: Option<&'static Draft>, // VIA patch
    // VIA patch: the budget every metaschema check of this compiler
    // shares, when one is set.
    pub(crate) meta_budget: Option<Budget>,
    // VIA patch: the subschema and pattern limits every schema position of
    // a document is held to, when set, and the patterns that census built.
    pub(crate) limits: Option<(usize, usize)>,
    pub(crate) patterns: RefCell<HashMap<String, Pattern>>,
    map: HashMap<Url, Root>,
    pub(crate) loader: DefaultUrlLoader,
}

impl Roots {
    fn new() -> Self {
        Self {
            default_draft: latest(),
            required_draft: None,
            meta_budget: None,
            limits: None,
            patterns: Default::default(),
            map: Default::default(),
            loader: DefaultUrlLoader::new(),
        }
    }
}

impl Default for Roots {
    fn default() -> Self {
        Self::new()
    }
}

impl Roots {
    pub(crate) fn get(&self, url: &Url) -> Option<&Root> {
        self.map.get(url)
    }

    pub(crate) fn resolve_fragment(&mut self, uf: UrlFrag) -> Result<UrlPtr, CompileError> {
        self.or_load(uf.url.clone())?;
        let Some(root) = self.map.get(&uf.url) else {
            return Err(CompileError::Bug("or_load didn't add".into()));
        };
        root.resolve_fragment(&uf.frag)
    }

    pub(crate) fn ensure_subschema(&mut self, up: &UrlPtr) -> Result<(), CompileError> {
        self.or_load(up.url.clone())?;
        let Some(root) = self.map.get_mut(&up.url) else {
            return Err(CompileError::Bug("or_load didn't add".into()));
        };
        if !root.draft.is_subschema(up.ptr.as_str()) {
            let doc = self.loader.load(&root.url)?;
            let v = up.ptr.lookup(doc, &up.url)?;
            if let Some(required) = self.required_draft {
                required.require(v)?;
            }
            root.draft.validate(up, v, self.meta_budget.as_ref())?;
            root.add_subschema(doc, &up.ptr)?;
        }
        Ok(())
    }

    pub(crate) fn or_load(&mut self, url: Url) -> Result<(), CompileError> {
        debug_assert!(url.fragment().is_none(), "trying to add root with fragment");
        if self.map.contains_key(&url) {
            return Ok(());
        }
        let doc = self.loader.load(&url)?;
        let r = self.create_root(url.clone(), doc)?;
        self.map.insert(url, r);
        Ok(())
    }

    pub(crate) fn create_root(&self, url: Url, doc: &Value) -> Result<Root, CompileError> {
        let draft = {
            let up = UrlPtr {
                url: url.clone(),
                ptr: "".into(),
            };
            self.loader
                .get_draft(&up, doc, self.default_draft, HashSet::new())?
        };
        if let Some(required) = self.required_draft {
            if draft.version != required.version {
                return Err(CompileError::UnsupportedDraft { url: url.into() });
            }
            required.require(doc)?;
        }
        if let Some(limits) = self.limits {
            self.census(draft, doc, &url, limits)?;
        }
        let vocabs = self.loader.get_meta_vocabs(doc, draft)?;
        let resources = {
            let mut m = HashMap::default();
            draft.collect_resources(doc, &url, "".into(), &url, &mut m)?;
            m
        };

        if !matches!(url.host_str(), Some("json-schema.org")) {
            draft.validate(
                &UrlPtr {
                    url: url.clone(),
                    ptr: "".into(),
                },
                doc,
                self.meta_budget.as_ref(),
            )?;
        }

        Ok(Root {
            draft,
            resources,
            url: url.clone(),
            meta_vocabs: vocabs,
        })
    }

    // VIA patch: holds every schema position of `doc`, reached or not, to
    // `(schemas, patterns)`, and builds each `pattern` and
    // `patternProperties` expression, so the regex program limit and the
    // compile budget apply to them all; the compile reuses what it built.
    fn census(
        &self,
        draft: &Draft,
        doc: &Value,
        url: &Url,
        (max_schemas, max_patterns): (usize, usize),
    ) -> Result<(), CompileError> {
        let (mut schemas, mut patterns) = (0usize, 0usize);
        draft.each_schema(doc, &mut |sch| {
            schemas += 1;
            if schemas > max_schemas {
                return Err(CompileError::LimitExceeded { what: "subschemas" });
            }
            let Value::Object(obj) = sch else {
                return Ok(());
            };
            let pattern = obj.get("pattern").and_then(Value::as_str);
            let names = match obj.get("patternProperties") {
                Some(Value::Object(names)) => Some(names.keys().map(String::as_str)),
                _ => None,
            };
            for p in pattern.into_iter().chain(names.into_iter().flatten()) {
                patterns += 1;
                if patterns > max_patterns {
                    return Err(CompileError::LimitExceeded { what: "patterns" });
                }
                if self.patterns.borrow().contains_key(p) {
                    continue;
                }
                let built = build_pattern(p, url.to_string(), self.meta_budget.as_ref())?;
                self.patterns.borrow_mut().insert(p.to_owned(), built);
            }
            Ok(())
        })
    }

    pub(crate) fn insert(&mut self, roots: &mut HashMap<Url, Root>) {
        self.map.extend(roots.drain());
    }
}
