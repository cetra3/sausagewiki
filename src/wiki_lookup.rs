use std::borrow::Cow;
use std::collections::HashMap;
use std::str::Utf8Error;

use futures::{Future, finished, failed, done};
use futures::future::FutureResult;
use percent_encoding::percent_decode;
use slug::slugify;

use resources::*;
use assets::*;
use state::State;
use web::{Lookup, Resource};

type BoxResource = Box<Resource + Sync + Send>;
type ResourceFn = Box<Fn() -> BoxResource + Sync + Send>;

lazy_static! {
    static ref ASSETS_MAP: HashMap<String, ResourceFn> = {
        let mut map = HashMap::new();

        map.insert(
            format!("style-{}.css", StyleCss::checksum()),
            Box::new(|| Box::new(StyleCss) as BoxResource) as ResourceFn
        );

        map.insert(
            format!("script-{}.js", ScriptJs::checksum()),
            Box::new(|| Box::new(ScriptJs) as BoxResource) as ResourceFn
        );

        map.insert(
            format!("amatic-sc-v9-latin-regular.woff"),
            Box::new(|| Box::new(AmaticFont) as BoxResource) as ResourceFn
        );

        map
    };
}

#[derive(Clone)]
pub struct WikiLookup {
    state: State
}

fn split_one(path: &str) -> Result<(Cow<str>, Option<&str>), Utf8Error> {
    let mut split = path.splitn(2, '/');
    let head = split.next().expect("At least one item must be returned");
    let head = percent_decode(head.as_bytes()).decode_utf8()?;
    let tail = split.next();

    Ok((head, tail))
}

fn asset_lookup(path: &str) -> FutureResult<Option<BoxResource>, Box<::std::error::Error + Send + Sync>> {
    let (head, tail) = match split_one(path) {
        Ok(x) => x,
        Err(x) => return failed(x.into()),
    };

    if tail.is_some() {
        return finished(None);
    }

    match ASSETS_MAP.get(head.as_ref()) {
        Some(resource_fn) => finished(Some(resource_fn())),
        None => finished(None),
    }
}

impl WikiLookup {
    pub fn new(state: State) -> WikiLookup {
        WikiLookup { state }
    }

    fn reserved_lookup(&self, path: &str, query: Option<&str>) -> <Self as Lookup>::Future {
        let (head, tail) = match split_one(path) {
            Ok(x) => x,
            Err(x) => return Box::new(failed(x.into())),
        };

        match (head.as_ref(), tail) {
            ("_assets", Some(asset)) =>
                Box::new(asset_lookup(asset)),
            ("_changes", None) => {
                let state = self.state.clone();
                Box::new(
                    done(pagination::from_str(query.unwrap_or("")).map_err(Into::into))
                    .and_then(move |pagination| ChangesResource::new(state, pagination))
                    .and_then(|resource| Ok(Some(resource)))
                )
            },
            ("_new", None) =>
                Box::new(finished(Some(Box::new(NewArticleResource::new(self.state.clone(), None)) as BoxResource))),
            ("_sitemap", None) =>
                Box::new(finished(Some(Box::new(SitemapResource::new(self.state.clone())) as BoxResource))),
            _ => Box::new(finished(None)),
        }
    }

    fn article_lookup(&self, path: &str, query: Option<&str>) -> <Self as Lookup>::Future {
        let (slug, tail) = match split_one(path) {
            Ok(x) => x,
            Err(x) => return Box::new(failed(x.into())),
        };

        if tail.is_some() {
            // Currently disallow any URLs of the form /slug/...
            return Box::new(finished(None));
        }

        // Normalize all user-generated slugs:
        let slugified_slug = slugify(&slug);
        if slugified_slug != slug {
            return Box::new(finished(Some(
                Box::new(ArticleRedirectResource::new(slugified_slug)) as BoxResource
            )));
        }

        let state = self.state.clone();
        let edit = query == Some("edit");
        let slug = slug.into_owned();

        use state::SlugLookup;
        Box::new(self.state.lookup_slug(slug.clone())
            .and_then(move |x| Ok(Some(match x {
                SlugLookup::Miss =>
                    Box::new(NewArticleResource::new(state, Some(slug))) as BoxResource,
                SlugLookup::Hit { article_id, revision } =>
                    Box::new(ArticleResource::new(state, article_id, revision, edit)) as BoxResource,
                SlugLookup::Redirect(slug) =>
                    Box::new(ArticleRedirectResource::new(slug)) as BoxResource,
            })))
        )
    }
}

impl Lookup for WikiLookup {
    type Resource = BoxResource;
    type Error = Box<::std::error::Error + Send + Sync>;
    type Future = Box<Future<Item = Option<Self::Resource>, Error = Self::Error>>;

    fn lookup(&self, path: &str, query: Option<&str>) -> Self::Future {
        assert!(path.starts_with("/"));
        let path = &path[1..];

        if path.starts_with("_") {
            self.reserved_lookup(path, query)
        } else {
            self.article_lookup(path, query)
        }
    }
}
