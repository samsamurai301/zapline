//! `Listing`: pagination, sorting, filters, search and scopes, on SQLite and
//! (when `ZAPLINE_TEST_POSTGRES_URL` is set) Postgres.
#![cfg(any(feature = "sqlite", feature = "postgres"))]

use serde::{Deserialize, Serialize};
use zapline::db::{sqlx, ListBackend, ListParams, Listing, Paged};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
struct Post {
    id: i64,
    title: String,
    published: bool,
    author_id: i64,
}

const SEED: &[(&str, bool, i64)] = &[
    ("Rust web frameworks", true, 1),
    ("Why RUST?", true, 2),
    ("Draft: 100% done_ish", false, 1),
    ("Cooking pasta", true, 2),
    ("Another rust post", false, 1),
];

fn listing(table: &'static str) -> Listing {
    Listing::new(table, "id, title, published, author_id")
        .sortable(["title", "published"])
        .search(["title"])
        .filter_bool("published")
        .filter_int("author_id")
        .page_size(2, 3)
}

async fn get<DB>(
    pool: &sqlx::Pool<DB>,
    table: &'static str,
    query: &str,
) -> zapline::Result<Paged<Post>>
where
    DB: ListBackend,
    Post: for<'r> sqlx::FromRow<'r, DB::Row>,
{
    listing(table)
        .fetch(pool, &ListParams::from_query(query))
        .await
}

fn titles(page: &Paged<Post>) -> Vec<&str> {
    page.items.iter().map(|p| p.title.as_str()).collect()
}

async fn check_all<DB>(pool: &sqlx::Pool<DB>, table: &'static str)
where
    DB: ListBackend,
    Post: for<'r> sqlx::FromRow<'r, DB::Row>,
{
    // Default page size and order (by id), with the total.
    let page = get(pool, table, "").await.unwrap();
    assert_eq!((page.total, page.limit, page.offset), (5, 2, 0));
    assert_eq!(titles(&page), ["Rust web frameworks", "Why RUST?"]);
    assert_eq!(page.next_offset(), Some(2));

    let last = get(pool, table, "offset=4").await.unwrap();
    assert_eq!(titles(&last), ["Another rust post"]);
    assert_eq!(last.next_offset(), None);
    assert_eq!(
        get(pool, table, "limit=500").await.unwrap().limit,
        3,
        "capped"
    );

    // Sorting, descending, several keys.
    let page = get(pool, table, "sort=-title&limit=3").await.unwrap();
    assert_eq!(
        titles(&page),
        ["Why RUST?", "Rust web frameworks", "Draft: 100% done_ish"]
    );
    let page = get(pool, table, "sort=published,-id&limit=3")
        .await
        .unwrap();
    assert_eq!(
        titles(&page),
        ["Another rust post", "Draft: 100% done_ish", "Cooking pasta"]
    );

    // Filters, search (case-insensitive, LIKE wildcards are literal) and both.
    let page = get(pool, table, "published=false").await.unwrap();
    assert_eq!(page.total, 2);
    let page = get(pool, table, "author_id=2&published=1").await.unwrap();
    assert_eq!(titles(&page), ["Why RUST?", "Cooking pasta"]);
    let page = get(pool, table, "q=rust&limit=3").await.unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(get(pool, table, "q=100%25").await.unwrap().total, 1);
    assert_eq!(
        get(pool, table, "q=_").await.unwrap().total,
        1,
        "_ is literal"
    );
    let page = get(pool, table, "q=RUST&published=false").await.unwrap();
    assert_eq!(titles(&page), ["Another rust post"]);

    // A scope can't be widened by the request.
    let mine = listing(table)
        .scope("author_id", 1)
        .fetch::<DB, Post>(pool, &ListParams::from_query("author_id=2&limit=3"))
        .await
        .unwrap();
    assert_eq!(mine.total, 0, "both conditions apply");
    let mine = listing(table)
        .scope("author_id", 1)
        .fetch::<DB, Post>(pool, &ListParams::from_query("limit=3"))
        .await
        .unwrap();
    assert!(mine.items.iter().all(|p| p.author_id == 1) && mine.total == 3);

    // Bad input is a 400 with a code, never SQL.
    for (query, code) in [
        ("sort=password", "invalid_sort"),
        ("sort=title;DROP TABLE x", "invalid_sort"),
        ("published=maybe", "invalid_filter"),
        ("author_id=abc", "invalid_filter"),
        ("limit=-1", "invalid_page"),
        ("offset=x", "invalid_page"),
    ] {
        let err = get(pool, table, query).await.unwrap_err();
        assert_eq!(
            (err.status().as_u16(), err.code()),
            (400, Some(code)),
            "{query}"
        );
    }
    let err = get(pool, table, "sort=password").await.unwrap_err();
    assert!(
        err.message().contains("sortable: title, published, id"),
        "{}",
        err.message()
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_listing() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL, \
         published BOOLEAN NOT NULL, author_id INTEGER NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (title, published, author) in SEED {
        sqlx::query("INSERT INTO posts (title, published, author_id) VALUES ($1, $2, $3)")
            .bind(title)
            .bind(published)
            .bind(author)
            .execute(&pool)
            .await
            .unwrap();
    }
    check_all(&pool, "posts").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_listing() {
    let Ok(url) = std::env::var("ZAPLINE_TEST_POSTGRES_URL") else {
        eprintln!("ZAPLINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    // A temporary table lives as long as the one connection.
    sqlx::query(
        "CREATE TEMPORARY TABLE list_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
         published BOOLEAN NOT NULL, author_id BIGINT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (title, published, author) in SEED {
        sqlx::query("INSERT INTO list_posts (title, published, author_id) VALUES ($1, $2, $3)")
            .bind(title)
            .bind(published)
            .bind(author)
            .execute(&pool)
            .await
            .unwrap();
    }
    check_all(&pool, "list_posts").await;
}
