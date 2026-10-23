//! Carries a schema change into a resource made by `zapline generate
//! resource`: after `zapline generate migration add_<columns>_to_<table>`
//! (or `remove_<columns>_from_<table>`), `src/routes/<table>.rs`, its pages
//! and its test read and write the new columns.
//!
//! The module's [`RECIPE_MARKER`] line says which fields it was generated
//! with. The resource is generated twice, with the old and with the new
//! fields, and the difference is merged into the files on disk (a three-way
//! merge, like `git merge-file`), so edits made since are kept. A patched
//! file is therefore what `generate resource` would write today with the new
//! fields, plus the user's own changes.
//!
//! Nothing is half done: when the module's merge conflicts with an edit, no
//! file is written; when a page or the test conflicts, that file is left
//! alone. Either way the exact lines to change are printed.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::db_resource::{self, Field, RECIPE_MARKER};
use super::{format_files, Report};
use crate::project::Project;

/// What a migration does to a resource's columns.
#[derive(Debug, Clone)]
pub enum Change {
    /// New columns, appended after the existing ones.
    Add(Vec<Field>),
    /// Columns to drop, by name.
    Remove(Vec<String>),
}

/// The fields `src/routes/<table>.rs` was generated with, when it has a
/// [`RECIPE_MARKER`] line that parses.
pub fn recorded_fields(project: &Project, table: &str) -> Option<Vec<Field>> {
    let text = fs::read_to_string(project.root.join(module_path(table))).ok()?;
    db_resource::parse_recipe(&text)?.ok().map(|(_, f)| f)
}

fn is_page(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|e| e == "html")
}

fn module_path(table: &str) -> PathBuf {
    PathBuf::from(format!("src/routes/{table}.rs"))
}

/// Applies `change` to the resource for `table`. Returns `false`, having
/// changed nothing, when there is no generated module with a recipe line to
/// work from (the caller then explains what to do by hand).
pub fn update_resource(
    project: &Project,
    table: &str,
    change: &Change,
    report: &mut Report,
) -> Result<bool> {
    let module = module_path(table);
    let Ok(text) = fs::read_to_string(project.root.join(&module)) else {
        return Ok(false);
    };
    let Some(recipe) = db_resource::parse_recipe(&text) else {
        return Ok(false);
    };
    let (names, old) = recipe.with_context(|| {
        format!(
            "cannot read the `{RECIPE_MARKER}` line of {}",
            module.display()
        )
    })?;
    if names.plural != table {
        return Ok(false);
    }
    let mut new = old.clone();
    match change {
        Change::Add(fields) => {
            for f in fields {
                if old.iter().any(|o| o.name == f.name) {
                    report.notes.push(format!(
                        "{} already has a `{}` field; it was left as it is",
                        module.display(),
                        f.name
                    ));
                } else {
                    new.push(f.clone());
                }
            }
        }
        Change::Remove(columns) => {
            for c in columns {
                if !old.iter().any(|o| &o.name == c) {
                    report.notes.push(format!(
                        "{} has no `{c}` field; nothing to take out",
                        module.display()
                    ));
                }
            }
            new.retain(|f| !columns.contains(&f.name));
            if new.is_empty() {
                report.notes.push(format!(
                    "{} would be left without fields, so it was not changed; \
                     delete the resource or add a column first",
                    module.display()
                ));
                return Ok(true);
            }
        }
    }
    if new == old {
        return Ok(true);
    }

    let before = render(project, &names, old);
    let after = render(project, &names, new);
    let mut writes: Vec<(PathBuf, String)> = Vec::new();
    let mut conflicts: Vec<(PathBuf, String)> = Vec::new();
    for (path, base) in &before {
        let Some((_, theirs)) = after.iter().find(|(p, _)| p == path) else {
            continue;
        };
        let Ok(ours) = fs::read_to_string(project.root.join(path)) else {
            continue;
        };
        let html = is_page(path);
        match merge3(base, &ours, theirs, html) {
            Some(merged) if merged == ours => {}
            Some(merged) => writes.push((path.clone(), merged)),
            None => conflicts.push((path.clone(), changed_lines(base, theirs, html))),
        }
    }

    let module_conflicts = conflicts.iter().any(|(p, _)| *p == module);
    if module_conflicts {
        // Writing the pages or the test without the module would leave them
        // using columns the module doesn't have (or the other way round).
        for (path, base) in &before {
            if let Some((_, theirs)) = after.iter().find(|(p, _)| p == path) {
                if !conflicts.iter().any(|(p, _)| p == path) && base != theirs {
                    conflicts.push((path.clone(), changed_lines(base, theirs, is_page(path))));
                }
            }
        }
        writes.clear();
    }
    let mut formatted = Vec::new();
    for (path, text) in writes {
        let full = project.root.join(&path);
        fs::write(&full, text).with_context(|| format!("cannot write {}", full.display()))?;
        if path.extension().is_some_and(|e| e == "rs") {
            formatted.push(path.clone());
        }
        report.modified.push(path);
    }
    format_files(project, &formatted);
    for (path, lines) in conflicts {
        report.notes.push(format!(
            "{} was not changed: it was edited where the new code goes{}; make these changes yourself:\n{lines}",
            path.display(),
            if module_conflicts && path != module {
                " (or the module was, and the files must change together)"
            } else {
                ""
            }
        ));
    }
    Ok(true)
}

/// The module, pages and test `generate resource` would write today for
/// `fields`, formatted like the generator formats them.
fn render(
    project: &Project,
    names: &crate::naming::ResourceNames,
    mut fields: Vec<Field>,
) -> Vec<(PathBuf, String)> {
    // Notes about parents were given when the resource was generated.
    let mut quiet = Report::default();
    for f in &mut fields {
        if let Some(parent) = f.parent.as_mut() {
            super::resolve_parent(project, parent, &mut quiet);
        }
    }
    let settings = super::ResourceSettings::of(project);
    // Only the migration's file names; the migration itself is not compared.
    let migration = crate::db::MigrationFiles::new("0_resource", crate::db::Style::Reversible);
    let plan = settings.plan(names, &fields, migration);
    db_resource::files(&plan)
        .into_iter()
        .filter(|f| !f.path.starts_with(crate::db::MIGRATIONS_DIR))
        .map(|f| {
            let text = if f.path.extension().is_some_and(|e| e == "rs") {
                rustfmt(project, &f.contents)
            } else {
                f.contents
            };
            (f.path, text)
        })
        .collect()
}

/// `text` formatted by rustfmt with the project's settings, or unchanged
/// when rustfmt is not available (generated files are then unformatted too).
fn rustfmt(project: &Project, text: &str) -> String {
    let child = Command::new("rustfmt")
        .arg("--edition")
        .arg(&project.edition)
        .current_dir(&project.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return text.to_string();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => {
            String::from_utf8(out.stdout).unwrap_or_else(|_| text.to_string())
        }
        _ => text.to_string(),
    }
}

// ---- three-way merge ---------------------------------------------------------------

/// For each line of `a`, the line of `b` it is matched with in a longest
/// common subsequence.
fn matching(a: &[&str], b: &[&str]) -> Vec<Option<usize>> {
    let mut out = vec![None; a.len()];
    // Common ends are matched directly; the table is for the middle.
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        out[pre] = Some(pre);
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        out[a.len() - 1 - suf] = Some(b.len() - 1 - suf);
        suf += 1;
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (am.len(), bm.len());
    // lcs[i][j]: LCS length of am[i..] and bm[j..].
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * (m + 1) + j] = if am[i] == bm[j] {
                lcs[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if am[i] == bm[j] {
            out[pre + i] = Some(pre + j);
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The pieces a file is merged in: its lines, and in a page also each
/// table cell, since a generated row is one line. They concatenate back to
/// `text`.
fn units(text: &str, html: bool) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.split_inclusive('\n') {
        if !html {
            out.push(line);
            continue;
        }
        let mut start = 0;
        for (i, _) in line.char_indices() {
            let rest = &line[i..];
            if i > start && ["<td", "<th", "</tr"].iter().any(|t| rest.starts_with(t)) {
                out.push(&line[start..i]);
                start = i;
            }
        }
        out.push(&line[start..]);
    }
    out
}

/// Merges the change from `base` to `theirs` into `ours`, line by line (a
/// page cell by cell); `None` when both changed the same lines differently.
pub(crate) fn merge3(base: &str, ours: &str, theirs: &str, html: bool) -> Option<String> {
    let o = units(base, html);
    let a = units(ours, html);
    let b = units(theirs, html);
    let ma = matching(&o, &a);
    let mb = matching(&o, &b);
    let mut out = String::new();
    let (mut i, mut j, mut k) = (0, 0, 0);
    loop {
        if i < o.len() && ma[i] == Some(j) && mb[i] == Some(k) {
            out.push_str(o[i]);
            i += 1;
            j += 1;
            k += 1;
            continue;
        }
        // The next base line both sides still have marks the chunk's end.
        let next = (i..o.len()).find_map(|p| Some((p, ma[p]?, mb[p]?)));
        let (p, x, y) = next.unwrap_or((o.len(), a.len(), b.len()));
        let (oc, ac, bc) = (&o[i..p], &a[j..x], &b[k..y]);
        let taken = if ac == oc {
            bc
        } else if bc == oc || ac == bc {
            ac
        } else {
            return None;
        };
        out.extend(taken.iter().copied());
        if next.is_none() {
            return Some(out);
        }
        (i, j, k) = (p, x, y);
    }
}

/// The lines that change from `base` to `theirs`, each group under the line
/// it follows, for someone to make the change by hand.
pub(crate) fn changed_lines(base: &str, theirs: &str, html: bool) -> String {
    let o: Vec<&str> = units(base, html).into_iter().map(str::trim_end).collect();
    let b: Vec<&str> = units(theirs, html).into_iter().map(str::trim_end).collect();
    let m = matching(&o, &b);
    let mut out = String::new();
    let (mut i, mut k) = (0, 0);
    while i < o.len() || k < b.len() {
        if i < o.len() && m[i] == Some(k) {
            i += 1;
            k += 1;
            continue;
        }
        let next = (i..o.len()).find_map(|p| Some((p, m[p]?)));
        let (p, y) = next.unwrap_or((o.len(), b.len()));
        let after = (0..i)
            .rev()
            .map(|n| o[n].trim())
            .find(|l| !l.is_empty())
            .unwrap_or("the start of the file");
        out.push_str(&format!("  after `{after}`:\n"));
        for line in &o[i..p] {
            out.push_str(&format!("  - {}\n", line.trim()));
        }
        for line in &b[k..y] {
            out.push_str(&format!("  + {}\n", line.trim()));
        }
        (i, k) = (p, y);
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{MigrationFiles, Style};
    use crate::naming::ResourceNames;
    use db_resource::{parse_field_list, Dialect, Plan};

    #[test]
    fn merges_take_the_change_and_keep_edits() {
        let base = "a\nb\nc\nd\n";
        // Theirs adds after b; ours edited d.
        let theirs = "a\nb\nb2\nc\nd\n";
        let ours = "a\nb\nc\nd edited\n";
        assert_eq!(
            merge3(base, ours, theirs, false).as_deref(),
            Some("a\nb\nb2\nc\nd edited\n")
        );
        // Both changed the same line differently.
        assert_eq!(merge3(base, "a\nB\nc\nd\n", "a\nbee\nc\nd\n", false), None);
        // The same change on both sides is no conflict.
        assert_eq!(merge3(base, theirs, theirs, false).as_deref(), Some(theirs));
        assert_eq!(merge3(base, base, base, false).as_deref(), Some(base));
        assert_eq!(merge3("", "", "x\n", false).as_deref(), Some("x\n"));
        // A page merges cell by cell: an edited cell and a new one.
        let row = "<tr><td>{{ a }}</td><td>{{ b }}</td></tr>\n";
        assert_eq!(
            merge3(
                row,
                "<tr><td><b>{{ a }}</b></td><td>{{ b }}</td></tr>\n",
                "<tr><td>{{ a }}</td><td>{{ b }}</td><td>{{ c }}</td></tr>\n",
                true
            )
            .as_deref(),
            Some("<tr><td><b>{{ a }}</b></td><td>{{ b }}</td><td>{{ c }}</td></tr>\n")
        );
    }

    #[test]
    fn changed_lines_name_their_place() {
        let shown = changed_lines("a\nb\nc\n", "a\nb\nnew\nc\n", false);
        assert_eq!(shown, "  after `b`:\n  + new");
        let shown = changed_lines("a\nold\nc\n", "a\nc\n", false);
        assert_eq!(shown, "  after `a`:\n  - old");
    }

    fn generated(fields: &[&str]) -> Vec<(PathBuf, String)> {
        let names = ResourceNames::parse("post", None).unwrap();
        let specs: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let fields = parse_field_list(&specs).unwrap();
        let plan = Plan {
            names: &names,
            fields: &fields,
            dialect: Dialect::Sqlite,
            html: true,
            flash: true,
            test_crate: Some("blog"),
            migration: MigrationFiles::new("1_create_posts", Style::Reversible),
        };
        db_resource::files(&plan)
            .into_iter()
            .filter(|f| !f.path.starts_with("migrations"))
            .map(|f| (f.path, f.contents))
            .collect()
    }

    #[test]
    fn the_recipe_line_reads_back_into_the_same_fields() {
        let files = generated(&["title", "body:text?", "views:int", "due:date?"]);
        let module = &files[0].1;
        assert!(
            module.contains("// zapline:resource post title:string body:text? views:int due:date?"),
            "{module}"
        );
        let (names, fields) = db_resource::parse_recipe(module).unwrap().unwrap();
        assert_eq!(names.plural, "posts");
        let specs: Vec<String> = fields.iter().map(Field::spec).collect();
        assert_eq!(
            specs,
            ["title:string", "body:text?", "views:int", "due:date?"]
        );

        let names = ResourceNames::parse("person", Some("persons")).unwrap();
        let line = db_resource::recipe(&names, &fields[..1]);
        assert_eq!(
            line,
            "// zapline:resource person --plural persons title:string"
        );
        let (back, _) = db_resource::parse_recipe(&line).unwrap().unwrap();
        assert_eq!(back.plural, "persons");
        assert!(db_resource::parse_recipe("fn main() {}").is_none());
    }

    /// Patching a resource generated with `before` (and edited) gives what
    /// generating it with `after` gives, edits included.
    #[test]
    fn adding_and_removing_columns_matches_generating_them() {
        let before = generated(&["title", "body:text?"]);
        let after = generated(&["title", "body:text?", "views:int", "published:bool"]);
        for ((path, base), (_, theirs)) in before.iter().zip(&after) {
            // An edit at the top of each file survives the merge.
            let ours = format!("// my note\n{base}");
            let html = is_page(path);
            let merged = merge3(base, &ours, theirs, html).unwrap_or_else(|| panic!("{path:?}"));
            assert_eq!(merged, format!("// my note\n{theirs}"), "{path:?}");
            // And removing them again gives back the original.
            assert_eq!(
                merge3(theirs, &merged, base, html).unwrap(),
                ours,
                "{path:?}"
            );
        }
    }

    #[test]
    fn an_edit_where_the_new_code_goes_is_a_conflict() {
        let before = generated(&["title"]);
        let after = generated(&["title", "views:int"]);
        let (base, theirs) = (&before[0].1, &after[0].1);
        let ours = base.replace(
            "pub title: String,",
            "pub title: String,\n    pub mine: i32,",
        );
        assert!(merge3(base, &ours, theirs, false).is_none());
        let shown = changed_lines(base, theirs, false);
        assert!(shown.contains("+ pub views: i64,"), "{shown}");
        assert!(shown.contains(RECIPE_MARKER), "{shown}");
    }
}
