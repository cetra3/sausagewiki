use futures::{self, Future};
use hyper;
use hyper::header::ContentType;
use hyper::server::*;
use serde_json;
use serde_urlencoded;

use assets::{StyleCss, ScriptJs};
use mimes::*;
use rendering::render_markdown;
use site::Layout;
use state::State;
use web::{Resource, ResponseFuture};

const NDASH: &str = "\u{2013}";

const EMPTY_ARTICLE_MESSAGE: &str = "
<p>Not found</p>
<p>There's no article here yet. You can create one by clicking the
edit-link below and saving a new article.</p>
";

fn title_from_slug(slug: &str) -> String {
    ::titlecase::titlecase(&slug.replace('-', " "))
}

pub struct NewArticleResource {
    state: State,
    slug: String,
}

impl NewArticleResource {
    pub fn new(state: State, slug: String) -> Self {
        Self { state, slug }
    }
}

impl Resource for NewArticleResource {
    fn allow(&self) -> Vec<hyper::Method> {
        use hyper::Method::*;
        vec![Options, Head, Get, Put]
    }

    fn head(&self) -> ResponseFuture {
        Box::new(futures::finished(Response::new()
            .with_status(hyper::StatusCode::NotFound)
            .with_header(ContentType(TEXT_HTML.clone()))
        ))
    }

    fn get(self: Box<Self>) -> ResponseFuture {
        #[derive(BartDisplay)]
        #[template="templates/article_revision.html"]
        struct Template<'a> {
            article_id: &'a str,
            revision: &'a str,
            created: &'a str,

            slug: &'a str,
            title: &'a str,
            raw: &'a str,
            rendered: &'a str,

            script_js_checksum: &'a str,
        }

        let title = title_from_slug(&self.slug);

        Box::new(self.head()
            .and_then(move |head| {
                Ok(head
                    .with_body(Layout {
                        title: &title,
                        body: &Template {
                            article_id: NDASH,
                            revision: NDASH,
                            created: NDASH,
                            slug: &self.slug,
                            title: &title,
                            raw: "",
                            rendered: EMPTY_ARTICLE_MESSAGE,
                            script_js_checksum: ScriptJs::checksum(),
                        },
                        style_css_checksum: StyleCss::checksum(),
                    }.to_string()))
            }))
    }

    fn put(self: Box<Self>, body: hyper::Body) -> ResponseFuture {
        // TODO Check incoming Content-Type

        use chrono::{TimeZone, Local};
        use futures::Stream;

        #[derive(Deserialize)]
        struct CreateArticle {
            base_revision: String,
            title: String,
            body: String,
        }

        #[derive(BartDisplay)]
        #[template="templates/article_revision_contents.html"]
        struct Template<'a> {
            title: &'a str,
            rendered: String,
        }

        #[derive(Serialize)]
        struct PutResponse<'a> {
            slug: &'a str,
            revision: i32,
            title: &'a str,
            rendered: &'a str,
            created: &'a str,
        }

        Box::new(body
            .concat2()
            .map_err(Into::into)
            .and_then(|body| {
                serde_urlencoded::from_bytes(&body)
                    .map_err(Into::into)
            })
            .and_then(move |arg: CreateArticle| {
                // TODO Check that update.base_revision == NDASH
                // ... which seems silly. But there should be a mechanism to indicate that
                // the client is actually trying to create a new article
                self.state.create_article(self.slug.clone(), arg.title, arg.body)
            })
            .and_then(|updated| {
                futures::finished(Response::new()
                    .with_status(hyper::StatusCode::Ok)
                    .with_header(ContentType(APPLICATION_JSON.clone()))
                    .with_body(serde_json::to_string(&PutResponse {
                        slug: &updated.slug,
                        revision: updated.revision,
                        title: &updated.title,
                        rendered: &Template {
                            title: &updated.title,
                            rendered: render_markdown(&updated.body),
                        }.to_string(),
                        created: &Local.from_utc_datetime(&updated.created).to_string(),
                    }).expect("Should never fail"))
                )
            })
        )
    }
}