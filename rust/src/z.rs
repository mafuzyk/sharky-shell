use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 20_000;
const COMPACT_LIMIT: usize = 10_000;
const HALF_LIFE: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone)]
struct Entry {
    at: i64,
    path: String,
}

pub struct Z {
    entries: Vec<Entry>,
    path: PathBuf,
}

impl Z {
    pub fn new() -> Option<Self> {
        let path = default_path()?;
        let entries = std::fs::read_to_string(&path)
            .map(|text| text.lines().filter_map(parse_entry).collect())
            .unwrap_or_default();
        let mut store = Z { entries, path };
        if store.entries.len() > MAX_ENTRIES {
            store.compact();
        }
        Some(store)
    }

    pub fn record(&mut self, path: &str) {
        let entry = Entry {
            at: now(),
            path: path.to_string(),
        };

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let line = format!(
                "{{\"t\":{},\"p\":\"{}\"}}\n",
                entry.at,
                escape(&entry.path)
            );
            let _ = file.write_all(line.as_bytes());
        }
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));

        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.compact();
        }
    }

    pub fn query(&self, tokens: &[String]) -> Option<String> {
        self.matches(tokens).into_iter().next()
    }

    pub fn matches(&self, tokens: &[String]) -> Vec<String> {
        let tokens: Vec<String> = tokens
            .iter()
            .flat_map(|token| token.split_whitespace())
            .map(str::to_lowercase)
            .filter(|token| !token.is_empty())
            .collect();
        let now = now() as f64;

        let mut grouped: HashMap<&str, Vec<i64>> = HashMap::new();
        for entry in &self.entries {
            grouped.entry(&entry.path).or_default().push(entry.at);
        }

        let mut ranked: Vec<(&str, f64, i64)> = grouped
            .into_iter()
            .filter_map(|(path, visits)| {
                let lower = path.to_lowercase();
                if !tokens.iter().all(|token| lower.contains(token)) {
                    return None;
                }
                let latest = *visits.iter().max()?;
                let score = score(path, &visits, &tokens, now);
                Some((path, score, latest))
            })
            .collect();

        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(right.2.cmp(&left.2))
                .then(left.0.cmp(right.0))
        });

        ranked
            .into_iter()
            .map(|(path, _, _)| path.to_string())
            .collect()
    }

    fn compact(&mut self) {
        let now = now() as f64;
        let mut grouped: HashMap<String, Vec<i64>> = HashMap::new();
        for entry in &self.entries {
            grouped.entry(entry.path.clone()).or_default().push(entry.at);
        }

        let mut ranked: Vec<(String, f64, i64)> = grouped
            .into_iter()
            .map(|(path, visits)| {
                let latest = *visits.iter().max().unwrap_or(&0);
                let score = score(&path, &visits, &[], now);
                (path, score, latest)
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(right.2.cmp(&left.2))
                .then(left.0.cmp(&right.0))
        });

        let kept: Vec<Entry> = ranked
            .into_iter()
            .take(COMPACT_LIMIT)
            .map(|(path, _, at)| Entry { at, path })
            .collect();
        self.entries = kept;
        self.write_all();
    }

    fn write_all(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&format!(
                "{{\"t\":{},\"p\":\"{}\"}}\n",
                entry.at,
                escape(&entry.path)
            ));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)
        {
            let _ = file.write_all(text.as_bytes());
        }
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
    }
}

fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SHARKY_Z") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(dir).join("sharky/z.jsonl"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/sharky/z.jsonl"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn score(path: &str, visits: &[i64], tokens: &[String], now: f64) -> f64 {
    let recency: f64 = visits
        .iter()
        .map(|at| {
            let age = (now - *at as f64).max(0.0);
            0.5_f64.powf(age / HALF_LIFE)
        })
        .sum();
    recency + component_bonus(path, tokens)
}

fn component_bonus(path: &str, tokens: &[String]) -> f64 {
    let components: Vec<String> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut position = 0;
    let mut bonus = 0.0;

    for token in tokens {
        while position < components.len() {
            let component = &components[position];
            position += 1;
            if component == token {
                bonus += 8.0;
                break;
            }
            if component.starts_with(token) {
                bonus += 4.0;
                break;
            }
            if component.contains(token) {
                bonus += 1.0;
                break;
            }
        }
    }
    bonus
}

fn parse_entry(line: &str) -> Option<Entry> {
    let start = line.find("\"p\":\"")? + 5;
    let mut path = String::new();
    let mut chars = line[start..].chars();

    while let Some(char) = chars.next() {
        match char {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => path.push('\n'),
                Some('t') => path.push('\t'),
                Some(other) => path.push(other),
                None => break,
            },
            other => path.push(other),
        }
    }
    if path.is_empty() {
        return None;
    }

    let time_start = line.find("\"t\":")? + 4;
    let time_end = line[time_start..]
        .find(|char: char| !char.is_ascii_digit() && char != '-')
        .map(|index| time_start + index)
        .unwrap_or(line.len());
    let at = line[time_start..time_end].parse().ok()?;

    Some(Entry { at, path })
}

fn escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}
