//! `zapline generate migration <name> [field:type ...]`: a reversible SQL
//! migration whose up and down SQL follow from its name, Rails style:
//! `create_<table>`, `add_<what>_to_<table>`, `remove_<what>_from_<table>`.
//! Any other name gets an empty `.up.sql` + `.down.sql` pair.

use std::fs;
use std::path::Path;

use anyhow::{bail, Result};

use super::db_resource::{Dialect, Field, FieldType};
use super::Report;
use crate::db::{self, Style};
use crate::naming::snake_case;
use crate::project::Project;

/// What the migration's name asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// `create_<table>`.
    CreateTable(String),
    /// `add_<what>_to_<table>`.
    AddColumns(String),
    /// `remove_<what>_from_<table>`.
    RemoveColumns(String),
    /// Anything else: SQL you write yourself.
    Blank,
}

impl Change {
    /// Reads the intent from a migration name (any case style).
    pub fn parse(name: &str) -> Change {
        let slug = snake_case(name);
        let table = |t: &str| {
            let ok = t.starts_with(|c: char| c.is_ascii_alphabetic())
                && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            ok.then(|| t.to_string())
        };
        if let Some(t) = slug.strip_prefix("create_").and_then(table) {
            return Change::CreateTable(t);
        }
        if let Some(rest) = slug.strip_prefix("add_") {
            if let Some(t) = rest.rsplit_once("_to_").and_then(|(_, t)| table(t)) {
                return Change::AddColumns(t);
            }
        }
        if let Some(rest) = slug.strip_prefix("remove_") {
            if let Some(t) = rest.rsplit_once("_from_").and_then(|(_, t)| table(t)) {
                return Change::RemoveColumns(t);
            }
        }
        Change::Blank
    }
}

/// `TYPE`, plus `NOT NULL DEFAULT <zero>` for a required column added to a
/// table that may already have rows.
fn column(field: &Field, with_default: bool) -> String {
    let mut sql = format!("{} {}", field.name, field.ty.sql());
    if !field.optional {
        sql.push_str(" NOT NULL");
        if with_default || field.ty == FieldType::Bool {
            sql.push_str(match field.ty {
                FieldType::String | FieldType::Text => " DEFAULT ''",
                FieldType::Int | FieldType::Float => " DEFAULT 0",
                FieldType::Bool => " DEFAULT FALSE",
            });
        }
    }
    sql
}

/// The up and down SQL for migration `name` with `fields`.
pub fn sql(name: &str, fields: &[Field], dialect: Dialect) -> Result<(String, String)> {
    let header = format!("-- Created by `zapline generate migration {name}`.\n");
    let add =
        |table: &str, f: &Field| format!("ALTER TABLE {table} ADD COLUMN {};\n", column(f, true));
    let drop = |table: &str, f: &Field| format!("ALTER TABLE {table} DROP COLUMN {};\n", f.name);
    let needs_fields = |table: &str| {
        if fields.is_empty() {
            bail!(
                "`{name}` changes columns of `{table}`: list them, e.g. \
                 `zapline g migration {name} email:string`"
            );
        }
        Ok(())
    };
    Ok(match Change::parse(name) {
        Change::CreateTable(table) => {
            let id = match dialect {
                Dialect::Sqlite => "id INTEGER PRIMARY KEY AUTOINCREMENT",
                Dialect::Postgres => "id BIGSERIAL PRIMARY KEY",
            };
            let mut cols = vec![format!("    {id}")];
            cols.extend(fields.iter().map(|f| format!("    {}", column(f, false))));
            (
                format!("{header}CREATE TABLE {table} (\n{}\n);\n", cols.join(",\n")),
                format!("-- Reverts {name}.\nDROP TABLE {table};\n"),
            )
        }
        Change::AddColumns(table) => {
            needs_fields(&table)?;
            let up: String = fields.iter().map(|f| add(&table, f)).collect();
            let down: String = fields.iter().rev().map(|f| drop(&table, f)).collect();
            (
                format!("{header}{up}"),
                format!("-- Reverts {name}.\n{down}"),
            )
        }
        Change::RemoveColumns(table) => {
            needs_fields(&table)?;
            let up: String = fields.iter().map(|f| drop(&table, f)).collect();
            let down: String = fields.iter().rev().map(|f| add(&table, f)).collect();
            (
                format!("{header}{up}"),
                format!(
                    "-- Reverts {name}: the columns come back empty (with their defaults).\n{down}"
                ),
            )
        }
        Change::Blank => {
            if !fields.is_empty() {
                bail!(
                    "fields only make sense for `create_<table>`, `add_<what>_to_<table>` \
                     or `remove_<what>_from_<table>` migrations; `{name}` is none of those"
                );
            }
            let (up, down) = db::blank_sql(name, Style::Reversible);
            (up, down.unwrap_or_default())
        }
    })
}

fn dialect_of(root: &Path) -> Dialect {
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    if manifest.contains("\"postgres\"") && !manifest.contains("\"sqlite\"") {
        Dialect::Postgres
    } else {
        Dialect::Sqlite
    }
}

/// `zapline generate migration`.
pub fn generate(project: &Project, name: &str, fields: &[String]) -> Result<Report> {
    let mut parsed: Vec<Field> = Vec::new();
    for spec in fields {
        let field = Field::parse(spec)?;
        if parsed.iter().any(|f| f.name == field.name) {
            bail!("field `{}` is listed twice", field.name);
        }
        parsed.push(field);
    }
    let (up, down) = sql(name, &parsed, dialect_of(&project.root))?;
    let written = db::create_migration(&project.root, name, &up, Some(&down))?;
    let mut report = Report::default();
    for path in written {
        report.created.push(
            path.strip_prefix(&project.root)
                .unwrap_or(&path)
                .to_path_buf(),
        );
    }
    report.notes.push(
        "apply it with `zapline db migrate` (or start the app); `zapline db rollback` runs the \
         .down.sql file"
            .to_string(),
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(specs: &[&str]) -> Vec<Field> {
        specs.iter().map(|s| Field::parse(s).unwrap()).collect()
    }

    #[test]
    fn names_say_what_to_write() {
        assert_eq!(
            Change::parse("CreateComments"),
            Change::CreateTable("comments".into())
        );
        assert_eq!(
            Change::parse("add_email_and_name_to_users"),
            Change::AddColumns("users".into())
        );
        assert_eq!(
            Change::parse("remove-legacy-from-posts"),
            Change::RemoveColumns("posts".into())
        );
        assert_eq!(Change::parse("backfill_slugs"), Change::Blank);
        assert_eq!(Change::parse("add_index"), Change::Blank);
    }

    #[test]
    fn create_table_drops_it_on_the_way_down() {
        let (up, down) = sql(
            "create_comments",
            &fields(&["body:text", "post_id:int", "note:string?", "done:bool"]),
            Dialect::Postgres,
        )
        .unwrap();
        assert!(
            up.contains("CREATE TABLE comments (\n    id BIGSERIAL PRIMARY KEY,\n"),
            "{up}"
        );
        assert!(up.contains("    body TEXT NOT NULL,\n"), "{up}");
        assert!(up.contains("    note TEXT,\n"), "{up}");
        assert!(
            up.contains("    done BOOLEAN NOT NULL DEFAULT FALSE\n);"),
            "{up}"
        );
        assert!(down.ends_with("DROP TABLE comments;\n"), "{down}");
    }

    #[test]
    fn add_and_remove_columns_are_mirror_images() {
        let cols = fields(&["email", "visits:int", "bio:text?"]);
        let (up, down) = sql("add_profile_to_users", &cols, Dialect::Sqlite).unwrap();
        assert!(up.contains("ALTER TABLE users ADD COLUMN email TEXT NOT NULL DEFAULT '';\n"));
        assert!(up.contains("ALTER TABLE users ADD COLUMN visits BIGINT NOT NULL DEFAULT 0;\n"));
        assert!(up.contains("ALTER TABLE users ADD COLUMN bio TEXT;\n"));
        let drops: Vec<&str> = down.lines().filter(|l| l.contains("DROP COLUMN")).collect();
        assert_eq!(
            drops,
            [
                "ALTER TABLE users DROP COLUMN bio;",
                "ALTER TABLE users DROP COLUMN visits;",
                "ALTER TABLE users DROP COLUMN email;"
            ]
        );

        let (up, down) = sql("remove_bio_from_users", &cols[2..], Dialect::Sqlite).unwrap();
        assert!(up.contains("ALTER TABLE users DROP COLUMN bio;"));
        assert!(down.contains("ALTER TABLE users ADD COLUMN bio TEXT;"));

        let err = sql("add_email_to_users", &[], Dialect::Sqlite).unwrap_err();
        assert!(err.to_string().contains("list them"), "{err}");
    }

    #[test]
    fn other_names_get_an_empty_pair() {
        let (up, down) = sql("backfill_slugs", &[], Dialect::Sqlite).unwrap();
        assert!(up.starts_with("-- backfill_slugs"));
        assert!(down.contains("Reverts backfill_slugs"));
        assert!(sql("backfill_slugs", &fields(&["x"]), Dialect::Sqlite).is_err());
    }

    #[test]
    fn writes_the_pair_into_migrations() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nzapline = { version = \"0.1\", features = [\"postgres\"] }\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let project = Project::discover(tmp.path()).unwrap();
        let report = generate(&project, "create_tags", &["label".into()]).unwrap();
        let created: Vec<String> = report
            .created
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(created.len(), 2);
        assert!(
            created[0].starts_with("migrations/") && created[0].ends_with("_create_tags.up.sql")
        );
        assert!(created[1].ends_with("_create_tags.down.sql"));
        let up = fs::read_to_string(tmp.path().join(&created[0])).unwrap();
        assert!(up.contains("BIGSERIAL"), "{up}");
    }
}
