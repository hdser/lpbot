//! Keeps the Grafana dashboards in step with each other, with the data
//! sources and tables they read, and with the home page setting. Reads the
//! files under `deploy/`; no network.
//!
//! The rules (see `docs/DESIGN.md`, "The viewer"):
//! - every dashboard belongs to one subject, shown in its title and its tags;
//! - every dashboard says in its description what it answers;
//! - the same menu comes first on every dashboard;
//! - every `/d/<uid>` a dashboard links to exists, and every `var-<name>` the
//!   link passes is a variable of that dashboard;
//! - every panel reads a provisioned data source, and every table a query
//!   names is one of the tables the viewer knows;
//! - Grafana opens on the Overview.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The subjects, as tags, with the prefix their dashboards' titles take.
/// Overview and Agents are one dashboard each and their title is the name.
const SUBJECTS: [(&str, &str); 5] = [
    ("overview", "Overview"),
    ("agents", "Agents"),
    ("pools", "Pools: "),
    ("runs", "Runs: "),
    ("system", "System: "),
];

const HOME: &str = "lpbot-overview";

fn deploy() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy")
}

struct Dashboard {
    file: String,
    json: Value,
}

impl Dashboard {
    fn uid(&self) -> &str {
        self.json["uid"].as_str().unwrap_or_default()
    }

    fn variables(&self) -> BTreeSet<String> {
        self.json["templating"]["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["name"].as_str().map(str::to_string))
            .collect()
    }

    fn panels(&self) -> Vec<&Value> {
        fn walk<'a>(panels: &'a Value, out: &mut Vec<&'a Value>) {
            for p in panels.as_array().into_iter().flatten() {
                out.push(p);
                walk(&p["panels"], out);
            }
        }
        let mut out = Vec::new();
        walk(&self.json["panels"], &mut out);
        out
    }
}

fn dashboards() -> Vec<Dashboard> {
    let dir = deploy().join("grafana/dashboards");
    let mut out: Vec<Dashboard> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .map(|path| Dashboard {
            file: path.file_name().unwrap().to_string_lossy().into_owned(),
            json: serde_json::from_str(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display())),
        })
        .collect();
    out.sort_by(|a, b| a.file.cmp(&b.file));
    assert!(
        out.len() >= 8,
        "expected the dashboards in {}",
        dir.display()
    );
    out
}

/// Every string anywhere in a JSON value.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// The dashboard links in a text: each `/d/<uid>` with the variables its
/// query string passes, as `var-<name>=` or `${<name>:queryparam}`.
fn links(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("/d/") {
        let after = &rest[at + 3..];
        let uid: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let tail = &after[uid.len()..];
        // A slug after the uid (`/d/<uid>/<slug>`) is allowed and skipped.
        let tail = tail
            .strip_prefix('/')
            .map(|t| t.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(tail);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '<'))
            .unwrap_or(tail.len());
        let query = tail[..end].strip_prefix('?').unwrap_or("");
        let mut vars = Vec::new();
        for part in query.split('&') {
            if let Some(v) = part.strip_prefix("var-") {
                vars.push(v.split('=').next().unwrap_or_default().to_string());
            } else if let Some(v) = part.strip_prefix("${") {
                if let Some(name) = v.strip_suffix(":queryparam}") {
                    vars.push(name.to_string());
                }
            }
        }
        if !uid.is_empty() {
            out.push((uid, vars));
        }
        rest = after;
    }
    out
}

/// The tables a SQL query reads: the names after `from` and `join` that are
/// not a function, a column (`r.window_from`), a subquery or a name the query
/// defines with `with`. String literals are left out first.
fn tables(sql: &str) -> BTreeSet<String> {
    let mut plain = String::with_capacity(sql.len());
    let mut quoted = false;
    for c in sql.chars() {
        if c == '\'' {
            quoted = !quoted;
            plain.push(' ');
        } else if !quoted {
            plain.push(c.to_ascii_lowercase());
        }
    }
    let words: Vec<&str> = plain
        .split(|c: char| {
            !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '(' || c == '$')
        })
        .filter(|w| !w.is_empty())
        .collect();
    let mut defined = BTreeSet::new();
    for pair in words.windows(2) {
        if (pair[0] == "with" || pair[0].ends_with(',')) && pair[1] != "(" {
            defined.insert(pair[1].to_string());
        }
    }
    // `with problems as (` and `, other as (` define names.
    let mut tokens = plain.split_whitespace().peekable();
    while let Some(t) = tokens.next() {
        if let Some(&next) = tokens.peek() {
            if next == "as" {
                let name = t.trim_start_matches(',').trim_start_matches("with");
                if !name.is_empty() {
                    defined.insert(name.to_string());
                }
            }
        }
    }
    // `extract(epoch from case when ...)` and the like.
    const EXPRESSIONS: [&str; 4] = ["case", "select", "lateral", "current_date"];
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = BTreeSet::new();
    let bytes: Vec<char> = plain.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if !is_word(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word(bytes[i]) {
            i += 1;
        }
        let word: String = bytes[start..i].iter().collect();
        if word != "from" && word != "join" {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_whitespace() {
            j += 1;
        }
        let name_start = j;
        while j < bytes.len() && is_word(bytes[j]) {
            j += 1;
        }
        let name: String = bytes[name_start..j].iter().collect();
        let mut k = j;
        while k < bytes.len() && bytes[k].is_whitespace() {
            k += 1;
        }
        // A function call, a column of an alias or a cast is not a table.
        let next = bytes.get(k).copied().unwrap_or(' ');
        if name.is_empty()
            || matches!(next, '(' | '.' | ':')
            || defined.contains(&name)
            || EXPRESSIONS.contains(&name.as_str())
        {
            continue;
        }
        out.insert(name);
    }
    out
}

fn known_tables() -> BTreeSet<String> {
    lpbot_sim::RESULT_TABLES
        .iter()
        .chain(lpbot_sim::POOL_TABLES.iter())
        .chain(lpbot_sim::LIVE_TABLES.iter())
        .chain(lpbot_sim::AGENT_TABLES.iter())
        .chain(lpbot_sim::SUPERVISOR_TABLES.iter())
        .map(|t| t.to_string())
        .collect()
}

fn datasources() -> BTreeSet<String> {
    let path = deploy().join("grafana/datasources/datasources.yml");
    let yaml: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    yaml["datasources"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|d| d["uid"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn every_dashboard_has_a_unique_uid() {
    let mut seen = BTreeMap::new();
    for d in dashboards() {
        assert!(!d.uid().is_empty(), "{} has no uid", d.file);
        if let Some(other) = seen.insert(d.uid().to_string(), d.file.clone()) {
            panic!("{} and {other} share the uid {}", d.file, d.uid());
        }
    }
}

#[test]
fn every_dashboard_belongs_to_one_subject_and_says_what_it_answers() {
    for d in dashboards() {
        let tags: BTreeSet<&str> = d.json["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert!(tags.contains("lpbot"), "{}: no tag lpbot", d.file);
        let subjects: Vec<_> = SUBJECTS
            .iter()
            .filter(|(tag, _)| tags.contains(tag))
            .collect();
        assert_eq!(
            subjects.len(),
            1,
            "{}: needs exactly one subject tag of {:?}, has {tags:?}",
            d.file,
            SUBJECTS.map(|s| s.0)
        );
        let title = d.json["title"].as_str().unwrap_or_default();
        let prefix = subjects[0].1;
        if prefix.ends_with(": ") {
            assert!(
                title.starts_with(prefix) && title.len() > prefix.len(),
                "{}: the title {title:?} should start with {prefix:?}",
                d.file
            );
        } else {
            assert_eq!(title, prefix, "{}: the title should be {prefix:?}", d.file);
        }
        let description = d.json["description"].as_str().unwrap_or_default();
        assert!(
            description.len() > 20,
            "{}: the description should say what it answers",
            d.file
        );
    }
}

#[test]
fn the_same_menu_comes_first_on_every_dashboard() {
    let all = dashboards();
    let home = all
        .iter()
        .find(|d| d.uid() == HOME)
        .expect("the Overview dashboard");
    let menu = &home.json["links"].as_array().expect("the Overview's links")[..5];
    let titles: Vec<_> = menu.iter().map(|l| l["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Overview", "Agents", "Pools", "Runs", "System"]);
    for d in &all {
        let links = d.json["links"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        assert!(
            links.len() >= 5 && links[..5] == *menu,
            "{}: its first five links should be the menu, as on the Overview",
            d.file
        );
    }
    // Every dropdown of the menu lists at least one dashboard.
    for item in menu.iter().filter(|l| l["type"] == "dashboards") {
        let tag = item["tags"][0].as_str().unwrap();
        assert!(
            all.iter()
                .any(|d| d.json["tags"].as_array().unwrap().iter().any(|t| t == tag)),
            "no dashboard carries the tag {tag} the menu lists"
        );
    }
}

#[test]
fn every_link_reaches_a_dashboard_and_passes_its_variables() {
    let all = dashboards();
    let by_uid: BTreeMap<String, &Dashboard> =
        all.iter().map(|d| (d.uid().to_string(), d)).collect();
    let mut checked = 0;
    for d in &all {
        let mut texts = Vec::new();
        strings(&d.json, &mut texts);
        for text in texts {
            for (uid, vars) in links(&text) {
                let target = by_uid.get(&uid).unwrap_or_else(|| {
                    panic!("{}: links to /d/{uid}, which no dashboard is", d.file)
                });
                let known = target.variables();
                for var in vars {
                    assert!(
                        known.contains(&var),
                        "{}: passes var-{var} to /d/{uid}, which has no such variable ({known:?})",
                        d.file
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(
        checked > 30,
        "only {checked} links found: is the parser broken?"
    );
}

#[test]
fn every_panel_reads_a_provisioned_source_and_a_known_table() {
    let sources = datasources();
    let known = known_tables();
    for d in dashboards() {
        for panel in d.panels() {
            let refs = std::iter::once(&panel["datasource"]).chain(
                panel["targets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| &t["datasource"]),
            );
            for r in refs {
                let Some(uid) = r["uid"].as_str() else {
                    continue;
                };
                // A variable, or one of Grafana's own sources.
                if uid.starts_with('$')
                    || uid.starts_with("--")
                    || uid == "__expr__"
                    || uid == "grafana"
                {
                    continue;
                }
                assert!(
                    sources.contains(uid),
                    "{}: panel {:?} reads the data source {uid}, which is not provisioned",
                    d.file,
                    panel["title"]
                );
            }
            for target in panel["targets"].as_array().into_iter().flatten() {
                let Some(sql) = target["rawSql"].as_str() else {
                    continue;
                };
                for table in tables(sql) {
                    assert!(
                        known.contains(&table),
                        "{}: panel {:?} reads {table:?}, which is not one of the viewer's tables",
                        d.file,
                        panel["title"]
                    );
                }
            }
        }
        for v in d.json["templating"]["list"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(sql) = v["query"].as_str() else {
                continue;
            };
            if v["type"] != "query" {
                continue;
            }
            for table in tables(sql) {
                assert!(
                    known.contains(&table),
                    "{}: variable {:?} reads {table:?}, which is not one of the viewer's tables",
                    d.file,
                    v["name"]
                );
            }
        }
    }
}

#[test]
fn grafana_opens_on_the_overview() {
    let path = deploy().join("docker-compose.yml");
    let compose: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let home = compose["services"]["grafana"]["environment"]["GF_USERS_HOME_PAGE"]
        .as_str()
        .expect("GF_USERS_HOME_PAGE in the grafana service");
    assert_eq!(home, format!("/d/{HOME}"));
    assert!(dashboards().iter().any(|d| d.uid() == HOME));
}

#[test]
fn the_link_parser_reads_the_forms_the_dashboards_use() {
    assert_eq!(
        links("/d/lpbot-run?var-experiment=${experiment}&var-run=${__value.raw}&from=1"),
        [(
            "lpbot-run".to_string(),
            vec!["experiment".to_string(), "run".to_string()]
        )]
    );
    assert_eq!(
        links("see [Runs](/d/lpbot-runs) and /d/lpbot-pool/lpbot-pool?var-pool=${pool}"),
        [
            ("lpbot-runs".to_string(), vec![]),
            ("lpbot-pool".to_string(), vec!["pool".to_string()]),
        ]
    );
    assert_eq!(
        links("/d/lpbot-runs?${experiment:queryparam}&${agent:queryparam}"),
        [(
            "lpbot-runs".to_string(),
            vec!["experiment".to_string(), "agent".to_string()]
        )]
    );
}

#[test]
fn the_table_reader_skips_functions_columns_literals_and_its_own_names() {
    let sql = "with problems as (select 1 from agent_status s join runs r on r.run_id = s.run_id \
               where extract(epoch from now()) > 0 and x = 'taken from the node') \
               select (extract(epoch from r.window_from::date)) from problems, live_pools";
    assert_eq!(
        tables(sql),
        ["agent_status", "runs"]
            .map(String::from)
            .into_iter()
            .collect()
    );
}
