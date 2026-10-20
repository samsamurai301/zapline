//! Scaffold a project and add a resource without the command line.
//!
//! Everything the `zapline` binary does is available as a library, which is how
//! the CLI is tested:
//!
//!     cargo run -p zapline-cli --example scaffold

use zapline_cli::generate;
use zapline_cli::new::{create, NewOptions};
use zapline_cli::templates::Template;

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("zapline-scaffold-example-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    // Same as `zapline new demo-api --template api --no-git`.
    let created = create(&NewOptions {
        name: "demo-api".into(),
        template: Template::Api,
        parent: dir.clone(),
        zapline_path: None,
        git: false,
    })?;
    println!(
        "created {} with {} files:",
        created.root.display(),
        created.files.len()
    );
    for file in &created.files {
        println!("  {file}");
    }

    // Same as `zapline generate resource blog-post` run inside the project.
    let report = generate::resource(&created.root, "blog-post", None, false)?;
    println!("\ngenerated:");
    for path in &report.created {
        println!("  created  {}", path.display());
    }
    for path in &report.modified {
        println!("  modified {}", path.display());
    }

    let routes = std::fs::read_to_string(created.root.join("src/routes/mod.rs"))?;
    println!("\nsrc/routes/mod.rs:\n{routes}");

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
