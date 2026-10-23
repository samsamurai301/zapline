//! `zapline generate deploy <target>`: hosting config for Fly.io, Railway,
//! Render or Docker Compose, built from what the project uses (SQLite file,
//! Postgres, Redis, health checks, secrets in `.env.example`).
//!
//! Every target builds the project's Dockerfile, so one image runs
//! everywhere. A project without one gets the `api` template's.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;

use super::Report;
use crate::project::Project;
use crate::render::render;

/// Where to deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    /// Fly.io: `fly.toml`
    Fly,
    /// Railway: `railway.json`
    Railway,
    /// Render: `render.yaml` (a Blueprint)
    Render,
    /// Docker Compose: `compose.yaml`, with Postgres and Redis when the app uses them
    Compose,
}

impl Target {
    /// The file this target writes.
    pub fn file(self) -> &'static str {
        match self {
            Target::Fly => "fly.toml",
            Target::Railway => "railway.json",
            Target::Render => "render.yaml",
            Target::Compose => "compose.yaml",
        }
    }
}

/// Where the fullstack Dockerfile keeps the SQLite file (a volume goes here).
pub const DATA_DIR: &str = "/home/app/data";
const PORT: u16 = 8080;

/// What the generated config depends on, read from the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// App name: the package name.
    pub name: String,
    /// The app keeps a SQLite file in [`DATA_DIR`], which needs a volume.
    pub sqlite: bool,
    /// zapline's `postgres` feature is on: provision a Postgres database.
    pub postgres: bool,
    /// zapline's `redis` (or `redis-tls`) feature is on: provision Redis.
    pub redis: bool,
    /// `/health/ready` exists (the `Health` plugin is installed).
    pub health: bool,
    /// `src/main.rs` reads `PORT`.
    pub reads_port: bool,
    /// Other settings from `.env.example` the host must provide.
    pub secrets: Vec<String>,
}

/// Settings that every target already provides, or that only matter locally.
const PROVIDED: &[&str] = &[
    "ADDR",
    "PORT",
    "DATABASE_URL",
    "REDIS_URL",
    "TRUSTED_PROXIES",
];

/// The host's proxy reaches the app from a private address; trusting those
/// lets rate limits see each client's own IP (see `TrustedProxies`).
pub const PRIVATE_NETWORKS: &str = "10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,fc00::/7";

/// The keys of a `.env` file, in order, without duplicates.
pub fn env_keys(env: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for line in env.lines() {
        // `# SMTP_URL=...` documents an optional setting; still worth listing.
        let line = line.trim().trim_start_matches('#').trim();
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_start_matches("export ").trim();
        let valid = !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !key.starts_with(|c: char| c.is_ascii_digit());
        if valid && !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }
    keys
}

impl Facts {
    /// Reads the facts from the project's files.
    pub fn read(project: &Project) -> Facts {
        let read = |rel: &str| fs::read_to_string(project.root.join(rel)).unwrap_or_default();
        let manifest = read("Cargo.toml");
        let dockerfile = read("Dockerfile");
        let main = read("src/main.rs");
        let feature = |name: &str| {
            manifest
                .lines()
                .filter(|l| l.trim_start().starts_with("zapline"))
                .any(|l| l.contains(&format!("\"{name}\"")))
        };
        let postgres = feature("postgres");
        let sqlite = !postgres
            && (dockerfile.contains(DATA_DIR)
                || main.contains("sqlite://")
                || project.root.join(crate::db::MIGRATIONS_DIR).is_dir());
        let health = sources_contain(&project.root.join("src"), "Health::new()");
        let secrets = env_keys(&read(".env.example"))
            .into_iter()
            .filter(|k| !PROVIDED.contains(&k.as_str()))
            .collect();
        Facts {
            name: project.package_name.clone(),
            sqlite,
            postgres,
            redis: feature("redis") || feature("redis-tls"),
            health,
            reads_port: main.contains("\"PORT\""),
            secrets,
        }
    }

    /// Settings that look like keys, which hosts can generate.
    fn is_secret_key(key: &str) -> bool {
        key.ends_with("_SECRET") || key.ends_with("_KEY")
    }
}

fn sources_contain(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        if path.is_dir() {
            sources_contain(&path, needle)
        } else {
            path.extension().is_some_and(|x| x == "rs")
                && fs::read_to_string(&path).is_ok_and(|s| s.contains(needle))
        }
    })
}

/// Fly.io app names: lowercase letters, digits and dashes.
fn fly_name(name: &str) -> String {
    name.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `fly.toml`.
pub fn fly(f: &Facts) -> String {
    let mut out = format!(
        "# Fly.io: `fly launch --copy-config --no-deploy` once (it can rename the app),\n\
         # then `fly deploy`. Docs: https://fly.io/docs/reference/configuration/\n\
         app = \"{name}\"\n\
         primary_region = \"iad\"\n\
         # zapline finishes in-flight requests on SIGTERM (up to 30 s).\n\
         kill_signal = \"SIGTERM\"\n\
         kill_timeout = 35\n\
         \n\
         [build]\n\
         \x20 dockerfile = \"Dockerfile\"\n\
         \n\
         [env]\n\
         \x20 PORT = \"{PORT}\"\n\
         \x20 # Fly's proxy connects from its private network: trust it for client IPs.\n\
         \x20 TRUSTED_PROXIES = \"{PRIVATE_NETWORKS}\"\n\
         \n\
         [http_service]\n\
         \x20 internal_port = {PORT}\n\
         \x20 force_https = true\n\
         \x20 auto_stop_machines = \"stop\"\n\
         \x20 auto_start_machines = true\n\
         \x20 min_machines_running = 0\n",
        name = fly_name(&f.name),
    );
    if f.health {
        out.push_str(
            "\n  [[http_service.checks]]\n\
             \x20   method = \"GET\"\n\
             \x20   path = \"/health/ready\"\n\
             \x20   interval = \"15s\"\n\
             \x20   timeout = \"5s\"\n\
             \x20   grace_period = \"10s\"\n",
        );
    }
    if f.sqlite {
        out.push_str(&format!(
            "\n# The SQLite file lives on this volume. Run one machine: `fly scale count 1`.\n\
             [[mounts]]\n\
             \x20 source = \"data\"\n\
             \x20 destination = \"{DATA_DIR}\"\n"
        ));
    }
    out.push_str("\n[[vm]]\n  memory = \"256mb\"\n");
    out
}

/// `railway.json`.
pub fn railway(f: &Facts) -> String {
    let mut deploy = vec![
        "    \"restartPolicyType\": \"ON_FAILURE\"".to_string(),
        "    \"restartPolicyMaxRetries\": 5".to_string(),
    ];
    if f.health {
        deploy.insert(0, "    \"healthcheckTimeout\": 60".to_string());
        deploy.insert(0, "    \"healthcheckPath\": \"/health/ready\"".to_string());
    }
    format!(
        "{{\n  \"$schema\": \"https://railway.com/railway.schema.json\",\n  \"build\": {{\n    \"builder\": \"DOCKERFILE\",\n    \"dockerfilePath\": \"Dockerfile\"\n  }},\n  \"deploy\": {{\n{}\n  }}\n}}\n",
        deploy.join(",\n")
    )
}

/// `render.yaml`.
pub fn render_yaml(f: &Facts) -> String {
    let name = fly_name(&f.name);
    let mut out = format!(
        "# Render Blueprint: in the dashboard, New > Blueprint, and pick this repository.\n\
         # Docs: https://render.com/docs/blueprint-spec\n\
         services:\n\
         \x20 - type: web\n\
         \x20   name: {name}\n\
         \x20   runtime: docker\n\
         \x20   plan: starter\n"
    );
    if f.health {
        out.push_str("    healthCheckPath: /health/ready\n");
    }
    out.push_str(&format!(
        "    envVars:\n      - key: PORT\n        value: \"{PORT}\"\n      - key: TRUSTED_PROXIES\n        value: \"{PRIVATE_NETWORKS}\"\n"
    ));
    if f.postgres {
        out.push_str(&format!(
            "      - key: DATABASE_URL\n        fromDatabase:\n          name: {name}-db\n          property: connectionString\n"
        ));
    }
    for key in &f.secrets {
        if Facts::is_secret_key(key) {
            out.push_str(&format!(
                "      - key: {key}\n        generateValue: true\n"
            ));
        } else {
            // Asked for in the dashboard when the Blueprint is created.
            out.push_str(&format!("      - key: {key}\n        sync: false\n"));
        }
    }
    if f.redis {
        out.push_str("      - key: REDIS_URL\n        sync: false\n");
    }
    if f.sqlite {
        out.push_str(&format!(
            "    # The SQLite file lives on this disk (paid plans; one instance).\n\
             \x20   disk:\n\
             \x20     name: data\n\
             \x20     mountPath: {DATA_DIR}\n\
             \x20     sizeGB: 1\n"
        ));
    }
    if f.postgres {
        out.push_str(&format!("databases:\n  - name: {name}-db\n"));
    }
    out
}

/// `compose.yaml`.
pub fn compose(f: &Facts) -> String {
    let mut out = String::from(
        "# Run the production image locally or on any server with Docker:\n\
         #   docker compose up --build -d\n\
         # Settings below without a value come from your shell or a .env file here.\n\
         services:\n\
         \x20 app:\n\
         \x20   build: .\n\
         \x20   ports:\n",
    );
    out.push_str(&format!("      - \"{PORT}:{PORT}\"\n"));
    let mut env = vec![format!("PORT: \"{PORT}\"")];
    if f.postgres {
        env.push("DATABASE_URL: postgres://app:app@db:5432/app".into());
    }
    if f.redis {
        env.push("REDIS_URL: redis://redis:6379".into());
    }
    out.push_str("    environment:\n");
    for line in &env {
        out.push_str(&format!("      {line}\n"));
    }
    for key in &f.secrets {
        // No value: taken from the shell or .env, and left unset when neither has it.
        out.push_str(&format!("      {key}:\n"));
    }
    if f.postgres || f.redis {
        out.push_str("    depends_on:\n");
        if f.postgres {
            out.push_str("      db:\n        condition: service_healthy\n");
        }
        if f.redis {
            out.push_str("      redis:\n        condition: service_started\n");
        }
    }
    if f.sqlite {
        out.push_str(&format!("    volumes:\n      - data:{DATA_DIR}\n"));
    }
    out.push_str("    restart: unless-stopped\n");
    if f.postgres {
        out.push_str(
            "\n  db:\n\
             \x20   image: postgres:17-alpine\n\
             \x20   environment:\n\
             \x20     POSTGRES_USER: app\n\
             \x20     POSTGRES_PASSWORD: app\n\
             \x20     POSTGRES_DB: app\n\
             \x20   volumes:\n\
             \x20     - pgdata:/var/lib/postgresql/data\n\
             \x20   healthcheck:\n\
             \x20     test: [\"CMD-SHELL\", \"pg_isready -U app\"]\n\
             \x20     interval: 2s\n\
             \x20     timeout: 5s\n\
             \x20     retries: 15\n\
             \x20   restart: unless-stopped\n",
        );
    }
    if f.redis {
        out.push_str(
            "\n  redis:\n\
             \x20   image: redis:7-alpine\n\
             \x20   restart: unless-stopped\n",
        );
    }
    let volumes: Vec<&str> = [(f.sqlite, "data"), (f.postgres, "pgdata")]
        .into_iter()
        .filter_map(|(on, v)| on.then_some(v))
        .collect();
    if !volumes.is_empty() {
        out.push_str("\nvolumes:\n");
        for v in volumes {
            out.push_str(&format!("  {v}:\n"));
        }
    }
    out
}

/// What to do after writing the file.
pub fn notes(target: Target, f: &Facts) -> Vec<String> {
    let mut notes = Vec::new();
    let secret_cmd = |prefix: &str| {
        f.secrets
            .iter()
            .map(|k| {
                if Facts::is_secret_key(k) {
                    format!("{prefix}{k}=$(openssl rand -base64 48)")
                } else {
                    format!("{prefix}{k}=...")
                }
            })
            .collect::<Vec<_>>()
    };
    match target {
        Target::Fly => {
            let mut lines = vec![
                "deploy to Fly.io:".to_string(),
                "fly launch --copy-config --no-deploy".to_string(),
            ];
            if f.sqlite {
                lines.push("fly volumes create data --size 1".into());
            }
            if f.postgres {
                lines.push("fly secrets set DATABASE_URL=postgres://...   (a Fly Managed Postgres or any other)".into());
            }
            if f.redis {
                lines.push("fly secrets set REDIS_URL=redis://...".into());
            }
            if !f.secrets.is_empty() {
                lines.push(format!("fly secrets set {}", secret_cmd("").join(" ")));
            }
            lines.push("fly deploy".into());
            notes.push(lines.join("\n"));
        }
        Target::Railway => {
            let mut lines = vec![
                "deploy to Railway: `railway up`, or connect the repository in the dashboard"
                    .to_string(),
                format!("set the variable TRUSTED_PROXIES={PRIVATE_NETWORKS} (Railway's proxy), so rate limits see client IPs"),
            ];
            if !f.reads_port {
                lines.push(format!(
                    "src/main.rs does not read PORT: set the service variable PORT={PORT}"
                ));
            }
            if f.sqlite {
                lines.push(format!(
                    "add a volume mounted at {DATA_DIR} (the SQLite file lives there)"
                ));
            }
            if f.postgres {
                lines.push(
                    "add a Postgres database and set DATABASE_URL=${{Postgres.DATABASE_URL}}"
                        .into(),
                );
            }
            if f.redis {
                lines.push("add Redis and set REDIS_URL=${{Redis.REDIS_URL}}".into());
            }
            if !f.secrets.is_empty() {
                lines.push(format!("set the variables {}", f.secrets.join(", ")));
            }
            notes.push(lines.join("\n"));
        }
        Target::Render => {
            let mut lines = vec![
                "deploy to Render: push, then New > Blueprint in the dashboard and pick the repository".to_string(),
            ];
            if f.redis {
                lines.push(
                    "add a Key Value (Redis) instance and set REDIS_URL to its internal URL".into(),
                );
            }
            if f.sqlite {
                lines.push("the SQLite disk needs a paid plan; or switch to Postgres (zapline's `postgres` feature)".into());
            }
            notes.push(lines.join("\n"));
        }
        Target::Compose => {
            let mut lines = vec![
                "run it: docker compose up --build -d, then open http://localhost:8080".to_string(),
            ];
            if !f.secrets.is_empty() {
                lines.push(format!(
                    "put {} in a .env file next to compose.yaml (it is in .dockerignore, so not in the image)",
                    f.secrets.join(", ")
                ));
            }
            notes.push(lines.join("\n"));
        }
    }
    notes
}

/// `zapline g deploy <target>`.
pub fn generate(project: &Project, target: Target, force: bool) -> Result<Report> {
    let root = &project.root;
    let path = root.join(target.file());
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            target.file()
        );
    }
    let mut report = Report::default();
    if !root.join("Dockerfile").exists() {
        let bin = project.default_bin()?;
        let dockerfile = render(
            include_str!("../../templates/api/Dockerfile.tmpl"),
            &[("name", &bin)],
        )?;
        fs::write(root.join("Dockerfile"), dockerfile).context("cannot write Dockerfile")?;
        report.created.push(PathBuf::from("Dockerfile"));
        if !root.join(".dockerignore").exists() {
            fs::write(
                root.join(".dockerignore"),
                include_str!("../../templates/common/dockerignore.tmpl"),
            )
            .context("cannot write .dockerignore")?;
            report.created.push(PathBuf::from(".dockerignore"));
        }
    }
    let facts = Facts::read(project);
    let text = match target {
        Target::Fly => fly(&facts),
        Target::Railway => railway(&facts),
        Target::Render => render_yaml(&facts),
        Target::Compose => compose(&facts),
    };
    let existed = path.exists();
    fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    if existed {
        report.modified.push(PathBuf::from(target.file()));
    } else {
        report.created.push(PathBuf::from(target.file()));
    }
    report.notes = notes(target, &facts);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fullstack() -> Facts {
        Facts {
            name: "shop_app".into(),
            sqlite: true,
            health: true,
            reads_port: true,
            secrets: vec!["SESSION_SECRET".into(), "SMTP_URL".into()],
            ..Facts::default()
        }
    }

    fn postgres() -> Facts {
        Facts {
            sqlite: false,
            postgres: true,
            redis: true,
            ..fullstack()
        }
    }

    #[test]
    fn reads_env_keys() {
        let env = "# comment\nADDR=0.0.0.0:8080\n# SMTP_URL=smtp://x\nexport SESSION_SECRET=x\nbad key=1\nADDR=again\nlower=1\n";
        assert_eq!(env_keys(env), ["ADDR", "SMTP_URL", "SESSION_SECRET"]);
    }

    #[test]
    fn fly_config() {
        let toml_text = fly(&fullstack());
        let v: toml::Value = toml::from_str(&toml_text).unwrap();
        assert_eq!(v["app"].as_str(), Some("shop-app"));
        assert_eq!(v["http_service"]["internal_port"].as_integer(), Some(8080));
        assert_eq!(
            v["http_service"]["checks"][0]["path"].as_str(),
            Some("/health/ready")
        );
        assert_eq!(v["mounts"][0]["destination"].as_str(), Some(DATA_DIR));
        assert_eq!(v["env"]["PORT"].as_str(), Some("8080"));
        assert_eq!(v["env"]["TRUSTED_PROXIES"].as_str(), Some(PRIVATE_NETWORKS));

        let plain: toml::Value = toml::from_str(&fly(&Facts {
            name: "x".into(),
            ..Facts::default()
        }))
        .unwrap();
        assert!(plain.get("mounts").is_none());
        assert!(plain["http_service"].get("checks").is_none());
    }

    #[test]
    fn railway_config_is_json() {
        let v: serde_json::Value = serde_json::from_str(&railway(&fullstack())).unwrap();
        assert_eq!(v["build"]["builder"], "DOCKERFILE");
        assert_eq!(v["deploy"]["healthcheckPath"], "/health/ready");
        let v: serde_json::Value = serde_json::from_str(&railway(&Facts::default())).unwrap();
        assert!(v["deploy"].get("healthcheckPath").is_none());
        assert_eq!(v["deploy"]["restartPolicyType"], "ON_FAILURE");
    }

    #[test]
    fn render_blueprint() {
        let y = render_yaml(&postgres());
        assert!(y.contains("    name: shop-app\n"));
        assert!(y.contains("      - key: SESSION_SECRET\n        generateValue: true\n"));
        assert!(y.contains("      - key: SMTP_URL\n        sync: false\n"));
        assert!(y.contains("          name: shop-app-db\n          property: connectionString\n"));
        assert!(y.ends_with("databases:\n  - name: shop-app-db\n"));
        assert!(!y.contains("disk:"));
        let y = render_yaml(&fullstack());
        assert!(y.contains(&format!("      mountPath: {DATA_DIR}\n")));
        assert!(!y.contains("databases:"));
    }

    #[test]
    fn compose_file() {
        let y = compose(&postgres());
        assert!(y.contains("      DATABASE_URL: postgres://app:app@db:5432/app\n"));
        assert!(y.contains("      REDIS_URL: redis://redis:6379\n"));
        assert!(y.contains("      SESSION_SECRET:\n"));
        assert!(y.contains("  db:\n    image: postgres:17-alpine\n"));
        assert!(y.contains("  redis:\n    image: redis:7-alpine\n"));
        assert!(y.ends_with("volumes:\n  pgdata:\n"));
        let y = compose(&fullstack());
        assert!(y.contains(&format!("      - data:{DATA_DIR}\n")));
        assert!(!y.contains("depends_on"));
        assert!(y.ends_with("volumes:\n  data:\n"));
        let y = compose(&Facts::default());
        assert!(!y.contains("volumes:"));
    }

    #[test]
    fn notes_list_the_secrets() {
        let fly_notes = notes(Target::Fly, &fullstack()).join("\n");
        assert!(fly_notes.contains("fly volumes create data"));
        assert!(fly_notes.contains("SESSION_SECRET=$(openssl rand -base64 48) SMTP_URL=..."));
        let railway_notes = notes(
            Target::Railway,
            &Facts {
                reads_port: false,
                ..postgres()
            },
        )
        .join("\n");
        assert!(railway_notes.contains("PORT=8080"));
        assert!(railway_notes.contains("${{Postgres.DATABASE_URL}}"));
    }
}
