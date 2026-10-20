//! A static site with file uploads: the site is served from `public/`,
//! uploads are streamed to disk and served back from `/uploads`.
//!
//!     cargo run -p zapline-examples --example realtime_files
//!     open http://localhost:8080
//!     curl -F note=hi -F file=@Cargo.toml localhost:8080/upload
//!     curl -r 0-99 localhost:8080/uploads/Cargo.toml

use std::path::{Path, PathBuf};

use serde::Serialize;
use zapline::fs::{ServeDir, ServeFile};
use zapline::multipart::{Multipart, MultipartConfig};
use zapline::prelude::*;

#[derive(Clone)]
struct UploadDir(PathBuf);

#[derive(Serialize)]
struct Uploaded {
    fields: Vec<(String, String)>,
    files: Vec<(String, u64)>,
}

async fn upload(State(dir): State<UploadDir>, mut form: Multipart) -> Result<Json<Uploaded>> {
    let mut out = Uploaded {
        fields: Vec::new(),
        files: Vec::new(),
    };
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        if field.file_name().is_some() {
            // Never trust the client's file name as a path.
            let Some(file_name) = field.safe_file_name() else {
                return Err(Error::bad_request("invalid file name"));
            };
            let size = field.save_to(dir.0.join(&file_name)).await?;
            out.files.push((file_name, size));
        } else {
            out.fields.push((name, field.text().await?));
        }
    }
    Ok(Json(out))
}

fn write_site(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("public"))?;
    std::fs::create_dir_all(root.join("uploads"))?;
    std::fs::write(
        root.join("public/index.html"),
        r#"<!doctype html><meta charset="utf-8"><title>uploads</title>
<link rel="stylesheet" href="/style.css">
<h1>Upload a file</h1>
<form action="/upload" method="post" enctype="multipart/form-data">
  <input name="note" placeholder="note">
  <input type="file" name="file" multiple>
  <button>Upload</button>
</form>
<p>Uploaded files are served from <code>/uploads/&lt;name&gt;</code>.</p>"#,
    )?;
    std::fs::write(
        root.join("public/style.css"),
        "body { font: 16px system-ui; margin: 2rem; }",
    )?;
    std::fs::write(root.join("public/robots.txt"), "User-agent: *\nAllow: /\n")
}

fn main() -> std::io::Result<()> {
    let root = std::env::temp_dir().join("zapline-realtime-files");
    write_site(&root)?;
    eprintln!("serving {}", root.display());

    App::new()
        .state(UploadDir(root.join("uploads")))
        // Uploads may be up to 50 MiB (the app-wide body limit stays 2 MiB).
        .post("/upload", upload)
        .meta(
            MultipartConfig::new()
                .body_limit(50 * 1024 * 1024)
                .max_fields(20),
        )
        .get(
            "/robots.txt",
            ServeFile::new(root.join("public/robots.txt")),
        )
        .static_files(
            "/uploads",
            ServeDir::new(root.join("uploads")).no_index_file(),
        )
        .static_files(
            "/",
            ServeDir::new(root.join("public")).cache_control("public, max-age=60"),
        )
        .run("0.0.0.0:8080")
}
