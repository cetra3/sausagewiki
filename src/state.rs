use std;

use diesel;
use diesel::sqlite::SqliteConnection;
use diesel::prelude::*;
use futures_cpupool::{self, CpuFuture};
use r2d2::Pool;
use r2d2_diesel::ConnectionManager;

use models;
use schema::*;

#[derive(Clone)]
pub struct State {
    connection_pool: Pool<ConnectionManager<SqliteConnection>>,
    cpu_pool: futures_cpupool::CpuPool,
}

pub type Error = Box<std::error::Error + Send + Sync>;

pub enum SlugLookup {
    Miss,
    Hit {
        article_id: i32,
        revision: i32,
    },
    Redirect(String),
}

#[derive(Insertable)]
#[table_name="article_revisions"]
struct NewRevision<'a> {
    article_id: i32,
    revision: i32,
    slug: &'a str,
    title: &'a str,
    body: &'a str,
    author: Option<&'a str>,
    latest: bool,
}

fn decide_slug(conn: &SqliteConnection, article_id: i32, prev_title: &str, title: &str, prev_slug: Option<&str>) -> Result<String, Error> {
    let base_slug = ::slug::slugify(title);

    if let Some(prev_slug) = prev_slug {
        if prev_slug == "" {
            // Never give a non-empty slug to the front page
            return Ok(String::new());
        }

        if title == prev_title {
            return Ok(prev_slug.to_owned());
        }

        if base_slug == prev_slug {
            return Ok(base_slug);
        }
    }

    let base_slug = if base_slug.is_empty() { "article" } else { &base_slug };

    use schema::article_revisions;

    let mut slug = base_slug.to_owned();
    let mut disambiguator = 1;

    loop {
        let slug_in_use = article_revisions::table
            .filter(article_revisions::article_id.ne(article_id))
            .filter(article_revisions::slug.eq(&slug))
            .filter(article_revisions::latest.eq(true))
            .count()
            .first::<i64>(conn)? != 0;

        if !slug_in_use {
            break Ok(slug);
        }

        disambiguator += 1;
        slug = format!("{}-{}", base_slug, disambiguator);
    }
}

struct SyncState<'a> {
    db_connection: &'a diesel::SqliteConnection,
}

impl<'a> SyncState<'a> {
    fn new(db_connection: &diesel::SqliteConnection) -> SyncState {
        SyncState { db_connection }
    }

    pub fn get_article_slug(&self, article_id: i32) -> Result<Option<String>, Error> {
        use schema::article_revisions;

        Ok(article_revisions::table
            .filter(article_revisions::article_id.eq(article_id))
            .filter(article_revisions::latest.eq(true))
            .select((article_revisions::slug))
            .first::<String>(self.db_connection)
            .optional()?)
    }

    pub fn get_article_revision(&self, article_id: i32, revision: i32) -> Result<Option<models::ArticleRevision>, Error> {
        use schema::article_revisions;

        Ok(article_revisions::table
            .filter(article_revisions::article_id.eq(article_id))
            .filter(article_revisions::revision.eq(revision))
            .first::<models::ArticleRevision>(self.db_connection)
            .optional()?)
    }

    pub fn query_article_revision_stubs<F>(&self, f: F) -> Result<Vec<models::ArticleRevisionStub>, Error>
    where
        F: 'static + Send + Sync,
        for <'x> F:
            FnOnce(article_revisions::BoxedQuery<'x, diesel::sqlite::Sqlite>) ->
                article_revisions::BoxedQuery<'x, diesel::sqlite::Sqlite>,
    {
        use schema::article_revisions::dsl::*;

        Ok(f(article_revisions.into_boxed())
            .select((
                sequence_number,
                article_id,
                revision,
                created,
                slug,
                title,
                latest,
                author,
            ))
            .load(self.db_connection)?
        )
    }

    pub fn lookup_slug(&self, slug: String) -> Result<SlugLookup, Error> {
        #[derive(Queryable)]
        struct ArticleRevisionStub {
            article_id: i32,
            revision: i32,
            latest: bool,
        }

        self.db_connection.transaction(|| {
            use schema::article_revisions;

            Ok(match article_revisions::table
                .filter(article_revisions::slug.eq(slug))
                .order(article_revisions::sequence_number.desc())
                .select((
                    article_revisions::article_id,
                    article_revisions::revision,
                    article_revisions::latest,
                ))
                .first::<ArticleRevisionStub>(self.db_connection)
                .optional()?
            {
                None => SlugLookup::Miss,
                Some(ref stub) if stub.latest => SlugLookup::Hit {
                    article_id: stub.article_id,
                    revision: stub.revision,
                },
                Some(stub) => SlugLookup::Redirect(
                    article_revisions::table
                        .filter(article_revisions::latest.eq(true))
                        .filter(article_revisions::article_id.eq(stub.article_id))
                        .select(article_revisions::slug)
                        .first::<String>(self.db_connection)?
                )
            })
        })
    }

    pub fn update_article(&self, article_id: i32, base_revision: i32, title: String, body: String, author: Option<String>)
        -> Result<models::ArticleRevision, Error>
    {
        if title.is_empty() {
            Err("title cannot be empty")?;
        }

        self.db_connection.transaction(|| {
            use schema::article_revisions;

            let (latest_revision, prev_title, prev_slug) = article_revisions::table
                .filter(article_revisions::article_id.eq(article_id))
                .order(article_revisions::revision.desc())
                .select((
                    article_revisions::revision,
                    article_revisions::title,
                    article_revisions::slug,
                ))
                .first::<(i32, String, String)>(self.db_connection)?;

            if latest_revision != base_revision {
                // TODO: If it is the same edit repeated, just respond OK
                // TODO: If there is a conflict, transform the edit to work seamlessly
                unimplemented!("TODO Missing handling of revision conflicts");
            }
            let new_revision = base_revision + 1;

            let slug = decide_slug(self.db_connection, article_id, &prev_title, &title, Some(&prev_slug))?;

            diesel::update(
                article_revisions::table
                    .filter(article_revisions::article_id.eq(article_id))
                    .filter(article_revisions::revision.eq(base_revision))
            )
                .set(article_revisions::latest.eq(false))
                .execute(self.db_connection)?;

            diesel::insert(&NewRevision {
                    article_id,
                    revision: new_revision,
                    slug: &slug,
                    title: &title,
                    body: &body,
                    author: author.as_ref().map(|x| &**x),
                    latest: true,
                })
                .into(article_revisions::table)
                .execute(self.db_connection)?;

            Ok(article_revisions::table
                .filter(article_revisions::article_id.eq(article_id))
                .filter(article_revisions::revision.eq(new_revision))
                .first::<models::ArticleRevision>(self.db_connection)?
            )
        })
    }

    pub fn create_article(&self, target_slug: Option<String>, title: String, body: String, author: Option<String>)
        -> Result<models::ArticleRevision, Error>
    {
        if title.is_empty() {
            Err("title cannot be empty")?;
        }

        self.db_connection.transaction(|| {
            #[derive(Insertable)]
            #[table_name="articles"]
            struct NewArticle {
                id: Option<i32>
            }

            let article_id = {
                use diesel::expression::sql_literal::sql;
                // Diesel and SQLite are a bit in disagreement for how this should look:
                sql::<(diesel::types::Integer)>("INSERT INTO articles VALUES (null)")
                    .execute(self.db_connection)?;
                sql::<(diesel::types::Integer)>("SELECT LAST_INSERT_ROWID()")
                    .load::<i32>(self.db_connection)?
                    .pop().expect("Statement must evaluate to an integer")
            };

            let slug = decide_slug(self.db_connection, article_id, "", &title, target_slug.as_ref().map(|x| &**x))?;

            let new_revision = 1;

            diesel::insert(&NewRevision {
                    article_id,
                    revision: new_revision,
                    slug: &slug,
                    title: &title,
                    body: &body,
                    author: author.as_ref().map(|x| &**x),
                    latest: true,
                })
                .into(article_revisions::table)
                .execute(self.db_connection)?;

            Ok(article_revisions::table
                .filter(article_revisions::article_id.eq(article_id))
                .filter(article_revisions::revision.eq(new_revision))
                .first::<models::ArticleRevision>(self.db_connection)?
            )
        })
    }

    pub fn search_query(&self, query_string: String, limit: i32, offset: i32, snippet_size: i32) -> Result<Vec<models::SearchResult>, Error> {
        use diesel::expression::sql_literal::sql;
        use diesel::types::{Integer, Text};

        fn fts_quote(src: &str) -> String {
            format!("\"{}\"", src.replace('\"', "\"\""))
        }

        let words = query_string
            .split_whitespace()
            .map(fts_quote)
            .collect::<Vec<_>>();

        let query = if words.len() > 1 {
            format!("NEAR({})", words.join(" "))
        } else if words.len() == 1 {
            format!("{}*", words[0])
        } else {
            "\"\"".to_owned()
        };

        Ok(
            sql::<(Text, Text, Text)>(
                "SELECT title, snippet(article_search, 1, '', '', '\u{2026}', ?), slug \
                    FROM article_search \
                    WHERE article_search MATCH ? \
                    ORDER BY rank \
                    LIMIT ? OFFSET ?"
            )
            .bind::<Integer, _>(snippet_size)
            .bind::<Text, _>(query)
            .bind::<Integer, _>(limit)
            .bind::<Integer, _>(offset)
            .load(self.db_connection)?)
    }
}

impl State {
    pub fn new(connection_pool: Pool<ConnectionManager<SqliteConnection>>, cpu_pool: futures_cpupool::CpuPool) -> State {
        State {
            connection_pool,
            cpu_pool,
        }
    }

    fn execute<F, T>(&self, f: F) -> CpuFuture<T, Error>
    where
        F: 'static + Sync + Send,
        for <'a> F: FnOnce(SyncState<'a>) -> Result<T, Error>,
        T: 'static + Send,
    {
        let connection_pool = self.connection_pool.clone();

        self.cpu_pool.spawn_fn(move || {
            let db_connection = connection_pool.get()?;

            f(SyncState::new(&*db_connection))
        })
    }

    pub fn get_article_slug(&self, article_id: i32) -> CpuFuture<Option<String>, Error> {
        self.execute(move |state| state.get_article_slug(article_id))
    }

    pub fn get_article_revision(&self, article_id: i32, revision: i32) -> CpuFuture<Option<models::ArticleRevision>, Error> {
        self.execute(move |state| state.get_article_revision(article_id, revision))
    }

    pub fn query_article_revision_stubs<F>(&self, f: F) -> CpuFuture<Vec<models::ArticleRevisionStub>, Error>
    where
        F: 'static + Send + Sync,
        for <'a> F:
            FnOnce(article_revisions::BoxedQuery<'a, diesel::sqlite::Sqlite>) ->
                article_revisions::BoxedQuery<'a, diesel::sqlite::Sqlite>,
    {
        self.execute(move |state| state.query_article_revision_stubs(f))
    }

    pub fn get_latest_article_revision_stubs(&self) -> CpuFuture<Vec<models::ArticleRevisionStub>, Error> {
        self.query_article_revision_stubs(|query| {
            query
                .filter(article_revisions::latest.eq(true))
                .order(article_revisions::title.asc())
        })
    }

    pub fn lookup_slug(&self, slug: String) -> CpuFuture<SlugLookup, Error> {
        self.execute(move |state| state.lookup_slug(slug))
    }

    pub fn update_article(&self, article_id: i32, base_revision: i32, title: String, body: String, author: Option<String>)
        -> CpuFuture<models::ArticleRevision, Error>
    {
        self.execute(move |state| state.update_article(article_id, base_revision, title, body, author))
    }

    pub fn create_article(&self, target_slug: Option<String>, title: String, body: String, author: Option<String>)
        -> CpuFuture<models::ArticleRevision, Error>
    {
        self.execute(move |state| state.create_article(target_slug, title, body, author))
    }

    pub fn search_query(&self, query_string: String, limit: i32, offset: i32, snippet_size: i32) -> CpuFuture<Vec<models::SearchResult>, Error> {
        self.execute(move |state| state.search_query(query_string, limit, offset, snippet_size))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use db;

    #[test]
    fn get_article_slug() {
        let db = db::test_connection();
        let state = SyncState::new(&db);

        assert_matches!(state.get_article_slug(0), Ok(None));
    }
}
