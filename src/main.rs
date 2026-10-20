use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "ocs",
    author = "louis",
    version = "0.2.0",
    about = "Blazing fast OpenCode session finder across projects and directories"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List sessions across all projects (newest first)
    List(ListArgs),
    /// Search session contents by keywords
    Find(FindArgs),
    /// Show full conversation of a session
    Show(ShowArgs),
    /// Print the command to resume a session
    Open(OpenArgs),
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Max sessions to return
    #[arg(short = 'n', long, default_value_t = 40)]
    limit: usize,

    /// Filter by directory path or title keyword
    #[arg(long)]
    dir: Option<String>,

    /// Filter by time elapsed, e.g. 12h, 7d, 2w, 1m
    #[arg(long)]
    since: Option<String>,

    /// Include child / subagent sessions
    #[arg(long)]
    all: bool,

    /// Output as structured JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct FindArgs {
    /// Keywords to search (each may appear in different messages)
    #[arg(required = true)]
    keywords: Vec<String>,

    /// Max matching sessions to return
    #[arg(short = 'n', long, default_value_t = 15)]
    limit: usize,

    /// Filter by time elapsed, e.g. 12h, 7d, 2w, 1m
    #[arg(long)]
    since: Option<String>,

    /// Include child / subagent sessions
    #[arg(long)]
    all: bool,

    /// Output as structured JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// Session ID, prefix, or title substring
    key: String,

    /// Output as Markdown
    #[arg(long)]
    md: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct OpenArgs {
    /// Session ID, prefix, or title substring
    key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionItem {
    id: String,
    parent_id: String,
    title: String,
    directory: String,
    time_created: i64,
    time_updated: i64,
    time_archived: i64,
    resume_cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snippet {
    role: String,
    snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchResult {
    session: SessionItem,
    hit_counts: HashMap<String, usize>,
    snippets: Vec<Snippet>,
    /// How many of the query keywords this session contains at all.
    covered: usize,
    /// Log-compressed total, see cmd_find.
    score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TranscriptTurn {
    time: i64,
    time_str: String,
    role: String,
    text: String,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn candidate_db_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let paths = vec![
        home_dir().map(|h| h.join(".local/share/opencode/opencode.db")),
        Some(PathBuf::from("/root/.local/share/opencode/opencode.db")),
        Some(PathBuf::from(
            "/home/louis/.local/share/opencode/opencode.db",
        )),
    ];

    for p in paths.into_iter().flatten() {
        if !p.exists() {
            continue;
        }
        if let Ok(canon) = fs::canonicalize(&p) {
            if seen.insert(canon) {
                out.push(p);
            }
        }
    }
    out
}

fn open_ro(path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn format_ts(ms: i64) -> String {
    if ms <= 0 {
        return "-".to_string();
    }
    let dt = Local.timestamp_millis_opt(ms);
    match dt {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        _ => "-".to_string(),
    }
}

fn parse_since(since_str: &str) -> Option<i64> {
    let s = since_str.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_part.trim().parse().ok()?;
    // A negative offset would put the cutoff in the future and silently filter
    // out every session, which looks like "no results" rather than a typo.
    if n <= 0 {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let delta_ms = match unit {
        "h" | "H" => n * 3600 * 1000,
        "d" | "D" => n * 86400 * 1000,
        "w" | "W" => n * 7 * 86400 * 1000,
        "m" | "M" => n * 30 * 86400 * 1000,
        _ => return None,
    };
    Some(now - delta_ms)
}

fn short_dir(dir: &str) -> String {
    if let Some(h) = home_dir() {
        let h_str = h.to_string_lossy();
        if dir.starts_with(h_str.as_ref()) {
            return format!("~{}", &dir[h_str.len()..]);
        }
    }
    dir.to_string()
}

fn load_all_sessions(paths: &[PathBuf]) -> Vec<SessionItem> {
    let mut map: HashMap<String, SessionItem> = HashMap::new();

    for p in paths {
        let conn = match open_ro(p) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check session_v2
        let has_v2: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_v2'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if has_v2 {
            let mut stmt = conn
                .prepare(
                    "SELECT id, coalesce(parent_id, ''), coalesce(title, ''), coalesce(directory, ''),
                            time_created, time_updated, coalesce(time_archived, 0)
                     FROM session_v2",
                )
                .ok();

            if let Some(ref mut st) = stmt {
                let rows = st.query_map([], |r| {
                    let id: String = r.get(0)?;
                    let parent_id: String = r.get(1)?;
                    let title: String = r.get(2)?;
                    let directory: String = r.get(3)?;
                    let time_created: i64 = r.get(4)?;
                    let time_updated: i64 = r.get(5)?;
                    let time_archived: i64 = r.get(6)?;
                    let resume_cmd = format!("cd {} && opencode -s {}", directory, id);
                    Ok(SessionItem {
                        id,
                        parent_id,
                        title,
                        directory,
                        time_created,
                        time_updated,
                        time_archived,
                        resume_cmd,
                    })
                });

                if let Ok(mapped) = rows {
                    for item in mapped.flatten() {
                        map.insert(item.id.clone(), item);
                    }
                }
            }
        }

        // Check session (V1 fallback)
        let has_v1: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if has_v1 {
            let mut stmt = conn
                .prepare(
                    "SELECT id, coalesce(parent_id, ''), coalesce(title, ''), coalesce(directory, ''),
                            time_created, time_updated, 0
                     FROM session",
                )
                .ok();

            if let Some(ref mut st) = stmt {
                let rows = st.query_map([], |r| {
                    let id: String = r.get(0)?;
                    let parent_id: String = r.get(1)?;
                    let title: String = r.get(2)?;
                    let directory: String = r.get(3)?;
                    let time_created: i64 = r.get(4)?;
                    let time_updated: i64 = r.get(5)?;
                    let time_archived: i64 = r.get(6)?;
                    let resume_cmd = format!("cd {} && opencode -s {}", directory, id);
                    Ok(SessionItem {
                        id,
                        parent_id,
                        title,
                        directory,
                        time_created,
                        time_updated,
                        time_archived,
                        resume_cmd,
                    })
                });

                if let Ok(mapped) = rows {
                    for item in mapped.flatten() {
                        map.entry(item.id.clone()).or_insert(item);
                    }
                }
            }
        }
    }

    let mut list: Vec<SessionItem> = map.into_values().collect();
    list.sort_by_key(|s| std::cmp::Reverse(s.time_created));
    list
}

fn extract_text_and_role(type_hint: &str, json_str: &str) -> (String, String) {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (type_hint.to_string(), String::new()),
    };

    let role = parsed
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or(type_hint)
        .to_string();

    let mut parts = Vec::new();

    if let Some(arr) = parsed.get("content").and_then(|c| c.as_array()) {
        for item in arr {
            if let Some(t) = item.get("text").and_then(|s| s.as_str()) {
                parts.push(t);
            }
        }
    } else if let Some(txt) = parsed.get("text").and_then(|s| s.as_str()) {
        parts.push(txt);
    } else if let Some(c_str) = parsed.get("content").and_then(|s| s.as_str()) {
        parts.push(c_str);
    }

    (role, parts.join("\n"))
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn make_snippet(text: &str, keyword: &str, width: usize) -> String {
    let lower_text = text.to_lowercase();
    let lower_kw = keyword.to_lowercase();

    if let Some(idx) = floor_search(&lower_text, &lower_kw, text) {
        let half = width / 2;
        let raw_start = idx.saturating_sub(half);
        let start = floor_char_boundary(text, raw_start);
        let raw_end = std::cmp::min(text.len(), idx + keyword.len() + half);
        let end = ceil_char_boundary(text, raw_end);

        let prefix = if start > 0 { "…" } else { "" };
        let suffix = if end < text.len() { "…" } else { "" };
        let cleaned = text[start..end].replace(['\r', '\n'], " ");
        format!("{}{}{}", prefix, cleaned.trim(), suffix)
    } else {
        let raw_end = std::cmp::min(text.len(), width);
        let end = ceil_char_boundary(text, raw_end);
        let cleaned = text[0..end].replace(['\r', '\n'], " ");
        if end < text.len() {
            format!("{}…", cleaned.trim())
        } else {
            cleaned.trim().to_string()
        }
    }
}

// to_lowercase can change byte length, so a hit in the lowercased string may not
// map to the same index in the original. Fall back to a direct case-insensitive scan.
fn floor_search(lower: &str, lower_kw: &str, orig: &str) -> Option<usize> {
    if let Some(i) = lower.find(lower_kw) {
        if i < orig.len() && orig.is_char_boundary(i) {
            return Some(i);
        }
    }
    let ol = orig.to_lowercase();
    ol.find(lower_kw)
        .map(|i| floor_char_boundary(orig, std::cmp::min(i, orig.len())))
}

fn resolve_session<'a>(sessions: &'a [SessionItem], key: &str) -> Option<&'a SessionItem> {
    // 1. Exact id
    for s in sessions {
        if s.id == key {
            return Some(s);
        }
    }
    // 2. Prefix id
    let prefixes: Vec<&SessionItem> = sessions.iter().filter(|s| s.id.starts_with(key)).collect();
    if prefixes.len() == 1 {
        return Some(prefixes[0]);
    }

    // 2b. Id substring, with or without the "ses_" prefix — people paste the
    // middle of an id they spotted in a log or a grep result.
    let body = key.strip_prefix("ses_").unwrap_or(key);
    if !body.is_empty() {
        let subs: Vec<&SessionItem> = sessions.iter().filter(|s| s.id.contains(body)).collect();
        if subs.len() == 1 {
            return Some(subs[0]);
        }
    }

    // 3. Title substring
    let lower_key = key.to_lowercase();
    let titles: Vec<&SessionItem> = sessions
        .iter()
        .filter(|s| s.title.to_lowercase().contains(&lower_key))
        .collect();

    if titles.len() == 1 {
        return Some(titles[0]);
    }

    if !titles.is_empty() {
        eprintln!("匹配到多个 session，请更精确：");
        for s in titles.iter().take(10) {
            eprintln!(
                "  {}  {}  {}",
                s.id,
                format_ts(s.time_created),
                if s.title.is_empty() {
                    "(untitled)"
                } else {
                    &s.title
                }
            );
        }
    } else if prefixes.len() > 1 {
        eprintln!("ID 前缀匹配到多个 session：");
        for s in prefixes.iter().take(10) {
            eprintln!("  {}  {}", s.id, s.title);
        }
    } else {
        eprintln!("未找到匹配的会话：{}", key);
    }
    None
}

fn cmd_list(args: ListArgs) {
    let paths = candidate_db_paths();
    let mut all_sess = load_all_sessions(&paths);

    if !args.all {
        all_sess.retain(|s| s.parent_id.is_empty());
    }

    if let Some(ref d) = args.dir {
        let low = d.to_lowercase();
        all_sess.retain(|s| {
            s.directory.to_lowercase().contains(&low) || s.title.to_lowercase().contains(&low)
        });
    }

    if let Some(ref sincestr) = args.since {
        if let Some(cutoff) = parse_since(sincestr) {
            all_sess.retain(|s| s.time_created >= cutoff);
        }
    }

    let results: Vec<SessionItem> = all_sess.into_iter().take(args.limit).collect();

    if args.json {
        let out = serde_json::to_string_pretty(&results).unwrap();
        println!("{}", out);
        return;
    }

    if results.is_empty() {
        println!("未找到任何 session");
        return;
    }

    println!("{:<34} {:<16} {:<12} TITLE", "SESSION ID", "CREATED", "DIR");
    println!("{}", "-".repeat(95));
    for s in &results {
        let title_clean = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        }
        .replace('\n', " ");
        let short_t = if title_clean.chars().count() > 45 {
            let truncated: String = title_clean.chars().take(43).collect();
            format!("{}…", truncated)
        } else {
            title_clean
        };
        let dir_base = Path::new(&s.directory)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| s.directory.clone());

        println!(
            "{:<34} {:<16} {:<12} {}",
            s.id,
            format_ts(s.time_created),
            dir_base,
            short_t
        );
    }
    println!(
        "\n共显示 {} 条（使用 `ocs open <ID>` 查看续接命令）",
        results.len()
    );
}

fn cmd_find(args: FindArgs) {
    let paths = candidate_db_paths();
    let all_sess = load_all_sessions(&paths);
    let sess_map: HashMap<String, SessionItem> =
        all_sess.iter().map(|s| (s.id.clone(), s.clone())).collect();

    let cutoff_opt = args.since.as_deref().and_then(parse_since);
    let kws_lower: Vec<String> = args.keywords.iter().map(|k| k.to_lowercase()).collect();

    struct Agg {
        counts: Vec<usize>,
        snippets: Vec<Snippet>,
    }

    // One table scan with an OR over all keywords. Asking SQLite once per keyword
    // would cost a full scan each time on a multi-GB database; OR costs one scan,
    // and the ranking below still rewards sessions that cover every keyword.
    let mut agg: HashMap<String, Agg> = HashMap::new();

    for p in &paths {
        let conn = match open_ro(p) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // A migrated database can hold the same conversation in both the V2
        // (session_message) and V1 (part) tables. Scanning both doubles every
        // hit count and prints each snippet twice, so pick one store per DB:
        // V2 when it actually has rows, V1 otherwise.
        let mut stores: Vec<&str> = Vec::new();
        for table in ["session_message", "part"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |r| r.get(0))
                .unwrap_or(-1);
            if n > 0 {
                stores.push(table);
                break;
            }
        }

        for table in stores {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                continue;
            }

            // part is the V1 store: no type column, role lives inside the payload.
            let head = if table == "session_message" {
                "SELECT session_id, type, data FROM session_message WHERE ("
            } else {
                "SELECT session_id, '', data FROM part WHERE ("
            };

            let mut sql = String::from(head);
            sql.push_str(&vec!["data LIKE ?"; kws_lower.len()].join(" OR "));
            sql.push(')');
            if cutoff_opt.is_some() {
                sql.push_str(" AND time_created >= ?");
            }

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for kw in &args.keywords {
                params_vec.push(Box::new(format!("%{}%", kw)));
            }
            if let Some(cutoff) = cutoff_opt {
                params_vec.push(Box::new(cutoff));
            }
            let params_slice: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();

            let rows = stmt.query_map(&params_slice[..], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            });

            let mapped = match rows {
                Ok(m) => m,
                Err(_) => continue,
            };

            for item in mapped.flatten() {
                let (sid, typ, data) = item;
                let (role, text) = extract_text_and_role(&typ, &data);
                if text.trim().is_empty() {
                    continue;
                }
                let low = text.to_lowercase();

                let counts: Vec<usize> = kws_lower
                    .iter()
                    .map(|kw| low.matches(kw.as_str()).count())
                    .collect();
                if counts.iter().all(|&c| c == 0) {
                    continue;
                }

                let entry = agg.entry(sid).or_insert_with(|| Agg {
                    counts: vec![0; args.keywords.len()],
                    snippets: Vec::new(),
                });
                for (i, c) in counts.iter().enumerate() {
                    entry.counts[i] += c;
                }

                if entry.snippets.len() < 2 {
                    let idx = counts.iter().position(|&c| c > 0).unwrap_or(0);
                    entry.snippets.push(Snippet {
                        role,
                        snippet: make_snippet(&text, &args.keywords[idx], 110),
                    });
                }
            }
        }
    }

    let mut results: Vec<SearchResult> = Vec::new();
    for (sid, a) in agg {
        let sess = match sess_map.get(&sid) {
            Some(s) => s,
            None => continue,
        };
        if !args.all && !sess.parent_id.is_empty() {
            continue;
        }

        // Keywords need not land in the same message — "妙搭 局限" should still
        // match a session that covers the topic across several turns. Partial
        // matches stay in the list, ranked below full coverage, because the
        // words a user remembers are often synonyms of what the session said.
        let mut hit_counts = HashMap::new();
        let mut covered = 0usize;
        let mut score = 0f64;
        for (i, kw) in args.keywords.iter().enumerate() {
            let c = a.counts[i];
            if c > 0 {
                hit_counts.insert(kw.clone(), c);
                covered += 1;
                // Log-compressed so one keyword repeated 800 times cannot bury a
                // session that genuinely discusses every keyword a few times.
                score += (1.0 + c as f64).ln();
            }
        }
        if covered == 0 {
            continue;
        }

        results.push(SearchResult {
            session: sess.clone(),
            hit_counts,
            snippets: a.snippets,
            covered,
            score,
        });
    }

    // Coverage first, then rarity-weighted volume, then recency so that when two
    // sessions look equally relevant the fresher one wins.
    results.sort_by(|a, b| {
        b.covered
            .cmp(&a.covered)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.session.time_created.cmp(&a.session.time_created))
    });

    results.truncate(args.limit);

    if args.json {
        let out = serde_json::to_string_pretty(&results).unwrap();
        println!("{}", out);
        return;
    }

    if results.is_empty() {
        println!("未找到包含关键字 {:?} 的会话", args.keywords);
        return;
    }

    println!("\n找到 {} 个匹配会话：\n", results.len());
    for res in &results {
        let s = &res.session;
        let title_clean = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        };
        // HashMap order is random; print in the user's keyword order instead.
        let hit_summary: Vec<String> = args
            .keywords
            .iter()
            .filter_map(|k| res.hit_counts.get(k).map(|v| format!("{}×{}", k, v)))
            .collect();
        let partial = if res.covered < args.keywords.len() {
            "  (部分命中)"
        } else {
            ""
        };

        println!(
            "  {}  {}  {}",
            s.id,
            format_ts(s.time_created),
            short_dir(&s.directory)
        );
        println!("  「{}」", title_clean);
        println!("  命中: {}{}", hit_summary.join("  "), partial);
        for sn in &res.snippets {
            println!("  [{}] {}", sn.role, sn.snippet);
        }
        println!("  续接: {}\n", s.resume_cmd);
    }
}

fn load_session_turns(paths: &[PathBuf], session_id: &str) -> Vec<TranscriptTurn> {
    let mut turns = Vec::new();

    for p in paths {
        let conn = match open_ro(p) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // 1. session_message
        let has_sm: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_message'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if has_sm {
            let mut stmt = conn
                .prepare(
                    "SELECT type, time_created, data FROM session_message
                     WHERE session_id = ? ORDER BY seq ASC",
                )
                .ok();

            if let Some(ref mut st) = stmt {
                let rows = st.query_map([session_id], |r| {
                    let typ: String = r.get(0)?;
                    let tc: i64 = r.get(1)?;
                    let data: String = r.get(2)?;
                    Ok((typ, tc, data))
                });

                if let Ok(mapped) = rows {
                    for item in mapped.flatten() {
                        let (typ, tc, data) = item;
                        let (role, text) = extract_text_and_role(&typ, &data);
                        if !text.trim().is_empty() {
                            turns.push(TranscriptTurn {
                                time: tc,
                                time_str: format_ts(tc),
                                role,
                                text,
                            });
                        }
                    }
                }
            }
        }

        // 2. part + message
        if turns.is_empty() {
            let has_part: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='part'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if has_part {
                let sql =
                    "SELECT p.time_created, coalesce(json_extract(m.data,'$.role'),'user'), p.data
                           FROM part p LEFT JOIN message m ON m.id = p.message_id
                           WHERE p.session_id = ? ORDER BY p.time_created ASC";
                let mut stmt = conn.prepare(sql).ok();
                if let Some(ref mut st) = stmt {
                    let rows = st.query_map([session_id], |r| {
                        let tc: i64 = r.get(0)?;
                        let role_hint: String = r.get(1)?;
                        let data: String = r.get(2)?;
                        Ok((tc, role_hint, data))
                    });

                    if let Ok(mapped) = rows {
                        for item in mapped.flatten() {
                            let (tc, role_hint, data) = item;
                            let (role, text) = extract_text_and_role(&role_hint, &data);
                            if !text.trim().is_empty() {
                                turns.push(TranscriptTurn {
                                    time: tc,
                                    time_str: format_ts(tc),
                                    role,
                                    text,
                                });
                            }
                        }
                    }
                }
            }
        }
        if !turns.is_empty() {
            break;
        }
    }

    turns.sort_by_key(|t| t.time);
    turns
}

fn cmd_show(args: ShowArgs) {
    let paths = candidate_db_paths();
    let sessions = load_all_sessions(&paths);
    let target = match resolve_session(&sessions, &args.key) {
        Some(s) => s,
        None => return,
    };

    let turns = load_session_turns(&paths, &target.id);

    if args.json {
        let out = serde_json::to_string_pretty(&turns).unwrap();
        println!("{}", out);
        return;
    }

    println!("# {}\n", target.title);
    println!("- Session ID: `{}`", target.id);
    println!("- 目录: `{}`", target.directory);
    println!("- 创建时间: {}", format_ts(target.time_created));
    println!("- 续接命令: `{}`\n", target.resume_cmd);
    println!("{}", "=".repeat(80));

    for turn in turns {
        if args.md {
            println!("### {} · {}\n\n{}\n", turn.role, turn.time_str, turn.text);
        } else {
            println!("\n== {} · {} ==\n{}", turn.role, turn.time_str, turn.text);
        }
    }
}

fn cmd_open(args: OpenArgs) {
    let paths = candidate_db_paths();
    let sessions = load_all_sessions(&paths);
    let target = match resolve_session(&sessions, &args.key) {
        Some(s) => s,
        None => return,
    };
    println!("{}", target.resume_cmd);
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::List(args)) => cmd_list(args),
        Some(Commands::Find(args)) => cmd_find(args),
        Some(Commands::Show(args)) => cmd_show(args),
        Some(Commands::Open(args)) => cmd_open(args),
        None => {
            cmd_list(ListArgs {
                limit: 40,
                dir: None,
                since: None,
                all: false,
                json: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, title: &str) -> SessionItem {
        SessionItem {
            id: id.to_string(),
            parent_id: String::new(),
            title: title.to_string(),
            directory: "/home/louis".to_string(),
            time_created: 1_700_000_000_000,
            time_updated: 1_700_000_000_000,
            time_archived: 0,
            resume_cmd: format!("opencode -s {}", id),
        }
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn parse_since_understands_units() {
        let day = 86_400_000i64;
        for (input, expect_days) in [("7d", 7i64), ("2w", 14), ("1m", 30), ("24h", 1)] {
            let cutoff = parse_since(input).expect(input);
            let age_days = (now_ms() - cutoff) / day;
            assert_eq!(age_days, expect_days, "input={input}");
        }
    }

    #[test]
    fn parse_since_rejects_junk() {
        for bad in ["", "  ", "abc", "7", "7x", "d", "-3d"] {
            assert!(parse_since(bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn parse_since_accepts_uppercase_and_padding() {
        assert!(parse_since(" 3D ").is_some());
        assert!(parse_since("5W").is_some());
    }

    #[test]
    fn snippet_keeps_multibyte_boundaries() {
        // Every char here is 3 bytes; a naive byte slice would panic.
        let text = "前面这段是为了把关键词推到很后面去的填充文字，妙搭的限制在这里说明。尾巴";
        let snip = make_snippet(text, "限制", 40);
        assert!(snip.contains("限制"), "got {snip}");
        assert!(snip.starts_with('…'), "expected leading ellipsis: {snip}");
    }

    #[test]
    fn snippet_without_keyword_takes_head() {
        let snip = make_snippet("no match anywhere in this sentence", "zzz", 10);
        assert!(snip.starts_with("no match"));
        assert!(snip.ends_with('…'));
    }

    #[test]
    fn snippet_flattens_newlines() {
        let snip = make_snippet("line one\nline two\r\nline three", "two", 30);
        assert!(!snip.contains('\n') && !snip.contains('\r'), "got {snip:?}");
    }

    #[test]
    fn extract_reads_content_parts() {
        let (role, text) = extract_text_and_role(
            "user",
            r#"{"role":"assistant","content":[{"type":"text","text":"第一段"},{"type":"tool_use","name":"bash"},{"type":"text","text":"第二段"}]}"#,
        );
        assert_eq!(role, "assistant");
        assert_eq!(text, "第一段\n第二段");
    }

    #[test]
    fn extract_reads_plain_text_field() {
        let (role, text) = extract_text_and_role("user", r#"{"text":"裸文本"}"#);
        assert_eq!(role, "user");
        assert_eq!(text, "裸文本");
    }

    #[test]
    fn extract_falls_back_on_bad_json() {
        let (role, text) = extract_text_and_role("assistant", "{not json");
        assert_eq!(role, "assistant");
        assert_eq!(text, "");
    }

    #[test]
    fn resolve_matches_id_forms() {
        let items = vec![sess("ses_aa11", "找妙搭"), sess("ses_bb22", "别的会话")];
        assert_eq!(resolve_session(&items, "ses_aa11").unwrap().id, "ses_aa11");
        assert_eq!(resolve_session(&items, "ses_aa").unwrap().id, "ses_aa11");
        // The middle of an id, as people paste it out of a log.
        assert_eq!(resolve_session(&items, "aa11").unwrap().id, "ses_aa11");
        assert_eq!(resolve_session(&items, "找妙搭").unwrap().id, "ses_aa11");
    }

    #[test]
    fn resolve_refuses_ambiguity() {
        let items = vec![sess("ses_aa11", "同名"), sess("ses_bb22", "同名")];
        assert!(resolve_session(&items, "同名").is_none());
        assert!(resolve_session(&items, "不存在").is_none());
    }

    #[test]
    fn char_boundary_helpers_clamp() {
        let s = "妙搭";
        assert_eq!(floor_char_boundary(s, 4), 3);
        assert_eq!(ceil_char_boundary(s, 4), 6);
        assert_eq!(floor_char_boundary(s, 99), 6);
        assert_eq!(ceil_char_boundary(s, 99), 6);
    }
}
