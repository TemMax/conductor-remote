//! Reading a workspace's Run configurations from its repository.
//!
//! A line reader for exactly the shape Conductor writes, like `read_value` in `reads/models.rs`:
//! `[scripts.run.<id>]` tables with a one-line `command = "..."` and an optional `hide = true`.

use std::fs;
use std::path::Path;

use super::DevRunConfig;

struct Entry {
    id: String,
    command: String,
    hide: bool,
}

/// The Run scripts of the repository at `repo_root`, in the order Conductor lists them.
pub fn run_configs(repo_root: &Path) -> Vec<DevRunConfig> {
    let mut entries: Vec<Entry> = Vec::new();
    for file in ["settings.toml", "settings.local.toml"] {
        let Ok(source) = fs::read_to_string(repo_root.join(".conductor").join(file)) else {
            continue;
        };
        for entry in read_entries(&source) {
            match entries.iter_mut().find(|known| known.id == entry.id) {
                Some(known) => *known = entry,
                None => entries.push(entry),
            }
        }
    }
    entries
        .into_iter()
        .filter(|entry| !entry.hide)
        .map(|entry| DevRunConfig {
            name: display_name(&entry.id),
            id: entry.id,
            command: entry.command,
        })
        .collect()
}

/// A task's display name: its id with `-` as spaces and its first letter in capitals.
pub fn display_name(id: &str) -> String {
    let spaced = id.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The tables of `source` that have a `command`, in file order; a repeated id keeps its first
/// place and its last content.
fn read_entries(source: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    // The table being read: `Some` only under a `[scripts.run.<id>]` header.
    let mut current: Option<Table> = None;
    for line in source.lines() {
        if let Some(parts) = header_parts(line) {
            finish(current.take(), &mut entries);
            current = match parts.as_slice() {
                [scripts, run, id] if scripts == "scripts" && run == "run" && !id.is_empty() => {
                    Some(Table {
                        id: id.clone(),
                        command: None,
                        hide: false,
                    })
                }
                _ => None,
            };
        } else if let Some(table) = current.as_mut() {
            if table.command.is_none() {
                table.command = assignment(line, "command");
            }
            if let Some(hide) = boolean(line, "hide") {
                table.hide = hide;
            }
        }
    }
    finish(current, &mut entries);
    entries
}

struct Table {
    id: String,
    command: Option<String>,
    hide: bool,
}

fn finish(table: Option<Table>, entries: &mut Vec<Entry>) {
    let Some(Table {
        id,
        command: Some(command),
        hide,
    }) = table
    else {
        return;
    };
    let entry = Entry { id, command, hide };
    match entries.iter_mut().find(|known| known.id == entry.id) {
        Some(known) => *known = entry,
        None => entries.push(entry),
    }
}

/// The dotted parts of a `[a.b."c"]` table header, unquoted and trimmed; `[[array]]` headers give
/// an empty-named part list that no config matches. `None` when the line is not a header.
fn header_parts(line: &str) -> Option<Vec<String>> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (array, rest) = match rest.strip_prefix('[') {
        Some(inner) => (true, inner),
        None => (false, rest),
    };
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = rest.char_indices();
    let end = loop {
        let (index, c) = chars.next()?;
        match quote {
            Some(q) => {
                part.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    part.push(c);
                }
                '.' => parts.push(std::mem::take(&mut part)),
                ']' => {
                    parts.push(std::mem::take(&mut part));
                    break index;
                }
                _ => part.push(c),
            },
        }
    };
    let mut after = &rest[end + 1..];
    if array {
        after = after.strip_prefix(']')?;
    }
    let after = after.trim_start();
    if !after.is_empty() && !after.starts_with('#') {
        return None;
    }
    if array {
        return Some(vec![String::new()]);
    }
    Some(
        parts
            .iter()
            .map(|part| unquote(part.trim()).to_string())
            .collect(),
    )
}

fn unquote(part: &str) -> &str {
    for quote in ['"', '\''] {
        if part.len() >= 2 && part.starts_with(quote) && part.ends_with(quote) {
            return &part[1..part.len() - 1];
        }
    }
    part
}

/// What follows `key =` on the line, when the line is that assignment.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.trim_start().strip_prefix(key)?;
    Some(rest.trim_start().strip_prefix('=')?.trim_start())
}

/// The string value of `key = "value"` on one line: double quotes with `\"` and `\\` escapes, or
/// single quotes taken literally; a trailing `# comment` is allowed.
fn assignment(line: &str, key: &str) -> Option<String> {
    let rest = value_of(line, key)?;
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &rest[1..];
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((index, c)) = chars.next() {
        if c == quote {
            return ends_line(&body[index + 1..]).then_some(value);
        }
        if quote == '"' && c == '\\' {
            match chars.next()? {
                (_, '"') => value.push('"'),
                (_, '\\') => value.push('\\'),
                (_, other) => {
                    value.push('\\');
                    value.push(other);
                }
            }
        } else {
            value.push(c);
        }
    }
    None
}

/// `key = true` or `key = false` on one line, a trailing comment allowed.
fn boolean(line: &str, key: &str) -> Option<bool> {
    let rest = value_of(line, key)?;
    for (word, value) in [("true", true), ("false", false)] {
        if let Some(after) = rest.strip_prefix(word) {
            return ends_line(after).then_some(value);
        }
    }
    None
}

fn ends_line(after: &str) -> bool {
    let after = after.trim_start();
    after.is_empty() || after.starts_with('#')
}
