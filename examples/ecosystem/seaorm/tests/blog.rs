//! End-to-end tests, each on its own in-memory SQLite database.

use seaorm_blog::{app, memory, Author, Post, PostPage};
use serde_json::{json, Value};
use zapline::test::TestClient;

async fn author(client: &TestClient, name: &str) -> Author {
    let res = client
        .post("/authors")
        .json(&json!({ "name": name }))
        .send()
        .await;
    res.assert_status(201);
    res.json()
}

async fn post(client: &TestClient, author_id: i32, title: &str) -> Post {
    let res = client
        .post(&format!("/authors/{author_id}/posts"))
        .json(&json!({ "title": title, "body": format!("about {title}") }))
        .send()
        .await;
    res.assert_status(201);
    res.json()
}

#[tokio::test]
async fn an_author_comes_with_their_posts() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    let first = post(&client, ada.id, "engines").await;
    let second = post(&client, ada.id, "notes").await;
    assert!(!first.published, "new posts are drafts");

    let loaded: Author = client
        .get(&format!("/authors/{}", ada.id))
        .send()
        .await
        .json();
    assert_eq!(loaded.name, "ada");
    assert_eq!(loaded.posts, Some(vec![first, second]));

    client.get("/authors/999").send().await.assert_status(404);
    client
        .post("/authors/999/posts")
        .json(&json!({ "title": "orphan" }))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn names_are_unique_and_validated() {
    let client = app(memory()).test();
    author(&client, "ada").await;
    let dup = client
        .post("/authors")
        .json(&json!({ "name": "ada" }))
        .send()
        .await;
    dup.assert_status(409);
    assert_eq!(dup.json::<Value>()["code"], "unique_violation");
    client
        .post("/authors")
        .json(&json!({ "name": "" }))
        .send()
        .await
        .assert_status(422);
}

#[tokio::test]
async fn patch_changes_only_the_given_fields() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    let draft = post(&client, ada.id, "engines").await;

    let res = client
        .patch(&format!("/posts/{}", draft.id))
        .json(&json!({ "published": true }))
        .send()
        .await;
    res.assert_status(200);
    let published: Post = res.json();
    assert!(published.published);
    assert_eq!((published.title, published.body), (draft.title, draft.body));

    client
        .patch("/posts/999")
        .json(&json!({ "published": true }))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn posts_are_paged_and_filtered() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    let bob = author(&client, "bob").await;
    for i in 0..5 {
        post(&client, ada.id, &format!("ada {i}")).await;
    }
    let bobs = post(&client, bob.id, "bob 0").await;
    client
        .patch(&format!("/posts/{}", bobs.id))
        .json(&json!({ "published": true }))
        .send()
        .await
        .assert_status(200);

    let page: PostPage = client.get("/posts?per_page=2&page=1").send().await.json();
    assert_eq!((page.total_items, page.total_pages), (6, 3));
    let titles: Vec<&str> = page.items.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["ada 3", "ada 2"], "newest first");

    let published: PostPage = client.get("/posts?published=true").send().await.json();
    assert_eq!(
        published.items,
        vec![Post {
            published: true,
            ..bobs
        }]
    );

    let adas: PostPage = client
        .get(&format!("/posts?author_id={}", ada.id))
        .send()
        .await
        .json();
    assert_eq!(adas.total_items, 5);

    client
        .get("/posts?per_page=0")
        .send()
        .await
        .assert_status(422);
}

#[tokio::test]
async fn merging_authors_moves_posts_in_one_transaction() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    let lovelace = author(&client, "lovelace").await;
    post(&client, ada.id, "engines").await;
    post(&client, lovelace.id, "notes").await;

    let res = client
        .post(&format!("/authors/{}/merge", ada.id))
        .json(&json!({ "from_id": lovelace.id, "rename_to": "ada lovelace" }))
        .send()
        .await;
    res.assert_status(200);
    let merged: Author = res.json();
    assert_eq!(merged.name, "ada lovelace");
    let titles: Vec<String> = merged.posts.unwrap().into_iter().map(|p| p.title).collect();
    assert_eq!(titles, ["engines", "notes"]);
    client
        .get(&format!("/authors/{}", lovelace.id))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn a_failed_merge_changes_nothing() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    let lovelace = author(&client, "lovelace").await;
    author(&client, "taken").await;
    post(&client, lovelace.id, "notes").await;

    // The posts move and lovelace is deleted, then the rename hits the
    // unique name: the transaction rolls all three steps back.
    let res = client
        .post(&format!("/authors/{}/merge", ada.id))
        .json(&json!({ "from_id": lovelace.id, "rename_to": "taken" }))
        .send()
        .await;
    res.assert_status(409);

    let still_there: Author = client
        .get(&format!("/authors/{}", lovelace.id))
        .send()
        .await
        .json();
    assert_eq!(still_there.posts.unwrap().len(), 1);
    let ada_now: Author = client
        .get(&format!("/authors/{}", ada.id))
        .send()
        .await
        .json();
    assert_eq!(
        (ada_now.name.as_str(), ada_now.posts.unwrap().len()),
        ("ada", 0)
    );

    // Unknown source: 404, also rolled back.
    client
        .post(&format!("/authors/{}/merge", ada.id))
        .json(&json!({ "from_id": 999 }))
        .send()
        .await
        .assert_status(404);
    client
        .post(&format!("/authors/{}/merge", ada.id))
        .json(&json!({ "from_id": ada.id }))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn deleting_an_author_cascades_to_their_posts() {
    let client = app(memory()).test();
    let ada = author(&client, "ada").await;
    post(&client, ada.id, "engines").await;

    client
        .delete(&format!("/authors/{}", ada.id))
        .send()
        .await
        .assert_status(204);
    let page: PostPage = client.get("/posts").send().await.json();
    assert_eq!(page.total_items, 0);
    client
        .delete(&format!("/authors/{}", ada.id))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn readiness_pings_the_database_and_the_api_is_documented() {
    let client = app(memory()).test();
    let res = client.get("/health/ready").send().await;
    res.assert_status(200);
    assert_eq!(res.json::<Value>()["checks"]["database"]["status"], "up");

    let spec: Value = client.get("/openapi.json").send().await.json();
    for path in [
        "/authors",
        "/authors/{id}",
        "/authors/{id}/merge",
        "/posts",
        "/posts/{id}",
    ] {
        assert!(spec["paths"][path].is_object(), "{path} missing");
    }
}
