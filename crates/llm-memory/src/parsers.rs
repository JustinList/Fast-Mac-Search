use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ahash::AHashMap;
use serde::{Deserialize, Serialize};

/// Maximum characters kept per individual turn in the encrypted reader cache (prevents giant pasted logs from bloating RAM).
const MAX_TURN_CHARS: usize = 6_000;
/// Maximum turns stored per conversation for the reader preview (full text up to 96 KB is still indexed in L1/L2).
const MAX_STORED_TURNS: usize = 120;
/// Threshold (48 MiB) above which streaming JSONL parsers jump to the last 16 MiB once head turns are captured.
const HUGE_JSONL_HEAD_LIMIT: u64 = 48 * 1024 * 1024;
const HUGE_JSONL_TAIL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LlmProvider {
    Claude,
    Codex,
    Gemini,
    ChatGpt,
    LmStudio,
    Other,
}

impl LlmProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
            Self::ChatGpt => "ChatGPT",
            Self::LmStudio => "LM Studio",
            Self::Other => "AI Chat",
        }
    }

    pub fn matches_tag(&self, tag: &str) -> bool {
        let lower = tag.trim_start_matches('@').to_ascii_lowercase();
        match self {
            Self::Claude => lower == "claude" || lower == "anthropic",
            Self::Codex => lower == "codex",
            Self::Gemini => lower == "gemini" || lower == "antigravity" || lower == "agy",
            Self::ChatGpt => lower == "chatgpt" || lower == "openai" || lower == "gpt",
            Self::LmStudio => lower == "lmstudio" || lower == "local",
            Self::Other => lower == "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTurn {
    /// `"user"` or `"assistant"`
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSession {
    pub session_id: String,
    pub provider: LlmProvider,
    pub title: String,
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub updated_unix_secs: i64,
    pub source_path: PathBuf,
    pub turns: Vec<LlmTurn>,
}

impl LlmSession {
    /// Concatenates conversation title, workspace, and dialogue turns into clean searchable text for L1 BM25 & L2 embeddings.
    pub fn searchable_body(&self, max_bytes: usize) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str(&self.title);
        out.push('\n');
        if let Some(ref ws) = self.workspace {
            out.push_str(ws);
            out.push('\n');
        }
        for turn in &self.turns {
            if out.len() >= max_bytes {
                break;
            }
            out.push_str(if turn.role == "user" { "User: " } else { "Assistant: " });
            let rem = max_bytes.saturating_sub(out.len());
            out.push_str(truncate_utf8(&turn.text, rem));
            out.push('\n');
        }
        out
    }

    /// Virtual display label stored in L0 Arena for <0.2ms fuzzy title/provider/workspace lookup.
    pub fn display_label(&self) -> String {
        match &self.workspace {
            Some(ws) if !ws.is_empty() => {
                let short_ws = Path::new(ws)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(ws);
                format!("[{}] {} — {}", self.provider.as_str(), self.title, short_ws)
            }
            _ => format!("[{}] {}", self.provider.as_str(), self.title),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeSessionMeta {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub updated_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum DiscoveredTranscript {
    ClaudeJsonl(PathBuf),
    CodexJsonl(PathBuf),
    AntigravityJsonl(PathBuf),
    LmStudioJson(PathBuf),
    ExportJson(PathBuf),
}

impl DiscoveredTranscript {
    pub fn path(&self) -> &Path {
        match self {
            Self::ClaudeJsonl(p)
            | Self::CodexJsonl(p)
            | Self::AntigravityJsonl(p)
            | Self::LmStudioJson(p)
            | Self::ExportJson(p) => p,
        }
    }
}

/// Discovers local LLM transcript files and sidecar Claude session metadata under a user's home directory.
pub fn discover_local_llm_files(
    home_dir: &Path,
    imports_dir: &Path,
) -> (Vec<DiscoveredTranscript>, AHashMap<String, ClaudeSessionMeta>) {
    let mut files = Vec::new();
    let mut claude_meta = AHashMap::new();

    // 1. Enrich metadata from Claude Desktop session JSON files (`title`, `cwd`, `model`)
    let claude_app_dir = home_dir.join("Library/Application Support/Claude");
    for sub in ["claude-code-sessions", "local-agent-mode-sessions"] {
        let dir = claude_app_dir.join(sub);
        if dir.is_dir() {
            collect_claude_meta_and_jsonl(&dir, 0, &mut claude_meta, &mut files);
        }
    }

    // 2. Claude Code CLI transcripts (`~/.claude/projects/**/*.jsonl`)
    let claude_projects = home_dir.join(".claude/projects");
    if claude_projects.is_dir() {
        walk_with_ext(&claude_projects, "jsonl", 4, &mut |p| {
            files.push(DiscoveredTranscript::ClaudeJsonl(p));
        });
    }

    // 3. OpenAI Codex transcripts (`~/.codex/sessions/**/*.jsonl`)
    let codex_sessions = home_dir.join(".codex/sessions");
    if codex_sessions.is_dir() {
        walk_with_ext(&codex_sessions, "jsonl", 6, &mut |p| {
            files.push(DiscoveredTranscript::CodexJsonl(p));
        });
    }

    // 4. Antigravity / Gemini transcripts (`~/.gemini/antigravity/brain/*/.system_generated/logs/transcript.jsonl`)
    let agy_brain = home_dir.join(".gemini/antigravity/brain");
    if agy_brain.is_dir()
        && let Ok(entries) = fs::read_dir(&agy_brain)
    {
        for entry in entries.flatten() {
            let tpath = entry
                .path()
                .join(".system_generated/logs/transcript.jsonl");
            if tpath.is_file() {
                files.push(DiscoveredTranscript::AntigravityJsonl(tpath));
            }
        }
    }

    // 5. LM Studio local conversations (`~/.lmstudio/conversations/**/*.json`)
    let lmstudio = home_dir.join(".lmstudio/conversations");
    if lmstudio.is_dir() {
        walk_with_ext(&lmstudio, "json", 4, &mut |p| {
            files.push(DiscoveredTranscript::LmStudioJson(p));
        });
    }

    // 6. Imported ChatGPT / Claude Web export JSON files (`<app_support>/imports/*.json`)
    if imports_dir.is_dir() {
        walk_with_ext(imports_dir, "json", 3, &mut |p| {
            files.push(DiscoveredTranscript::ExportJson(p));
        });
    }

    (files, claude_meta)
}

fn collect_claude_meta_and_jsonl(
    dir: &Path,
    depth: usize,
    meta_map: &mut AHashMap<String, ClaudeSessionMeta>,
    transcripts: &mut Vec<DiscoveredTranscript>,
) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.')
            || name_str == "node_modules"
            || name_str == "scratch-workspaces"
            || name_str == "worktrees"
            || name_str == "target"
        {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_claude_meta_and_jsonl(&p, depth + 1, meta_map, transcripts);
        } else if ft.is_file() {
            if name_str.ends_with(".jsonl") {
                transcripts.push(DiscoveredTranscript::ClaudeJsonl(p));
            } else if name_str.ends_with(".json") && name_str != "manifest.json" {
                if let Ok(meta) = fs::metadata(&p)
                    && meta.len() <= 64 * 1024
                    && let Ok(raw) = fs::read_to_string(&p)
                    && let Ok(parsed) = serde_json::from_str::<RawClaudeSidecarJson>(&raw)
                {
                    let info = ClaudeSessionMeta {
                        title: parsed.title.filter(|s| !s.trim().is_empty()),
                        cwd: parsed.cwd.or(parsed.origin_cwd),
                        model: parsed.model,
                        updated_ms: parsed.last_activity_at.or(parsed.created_at),
                    };
                    if let Some(id) = parsed.cli_session_id {
                        meta_map.insert(id, info.clone());
                    }
                    if let Some(id) = parsed.session_id {
                        meta_map.insert(id, info);
                    }
                }
            }
        }
    }
}

fn walk_with_ext(dir: &Path, ext: &str, max_depth: usize, cb: &mut impl FnMut(PathBuf)) {
    if max_depth == 0 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("._") || name_str == "node_modules" || name_str == ".git" {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_with_ext(&p, ext, max_depth - 1, cb);
        } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some(ext) {
            cb(p);
        }
    }
}

/// Parses a single discovered transcript or export archive into zero or more `LlmSession`s.
pub fn parse_discovered_transcript(
    item: &DiscoveredTranscript,
    claude_meta: &AHashMap<String, ClaudeSessionMeta>,
) -> Vec<LlmSession> {
    match item {
        DiscoveredTranscript::ClaudeJsonl(p) => parse_claude_jsonl(p, claude_meta).into_iter().collect(),
        DiscoveredTranscript::CodexJsonl(p) => parse_codex_jsonl(p).into_iter().collect(),
        DiscoveredTranscript::AntigravityJsonl(p) => parse_antigravity_jsonl(p).into_iter().collect(),
        DiscoveredTranscript::LmStudioJson(p) => parse_export_or_lmstudio_json(p, Some(LlmProvider::LmStudio)),
        DiscoveredTranscript::ExportJson(p) => parse_export_or_lmstudio_json(p, None),
    }
}

#[derive(Deserialize)]
struct RawClaudeSidecarJson {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "cliSessionId")]
    cli_session_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "originCwd")]
    origin_cwd: Option<String>,
    model: Option<String>,
    title: Option<String>,
    #[serde(rename = "lastActivityAt")]
    last_activity_at: Option<i64>,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
}

// =========================================================================
// 1. Claude Code & Claude Desktop Agent JSONL Streaming Parser
// =========================================================================

#[derive(Deserialize)]
struct RawClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    summary: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    message: Option<RawClaudeMessage>,
}

#[derive(Deserialize)]
struct RawClaudeMessage {
    role: Option<String>,
    model: Option<String>,
    content: Option<RawClaudeContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawClaudeContent {
    Text(String),
    Blocks(Vec<RawContentBlock>),
}

#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<String>,
}

#[inline]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let max_pos = haystack.len() - needle.len();
    let mut i = 0usize;
    while i <= max_pos {
        if let Some(rel) = haystack[i..=max_pos].iter().position(|&b| b == first) {
            let pos = i + rel;
            if &haystack[pos..pos + needle.len()] == needle {
                return true;
            }
            i = pos + 1;
        } else {
            break;
        }
    }
    false
}

pub fn parse_claude_jsonl(
    path: &Path,
    meta_lookup: &AHashMap<String, ClaudeSessionMeta>,
) -> Option<LlmSession> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mtime = file_mtime_secs(&file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("claude")
        .to_string();

    let mut session_id = stem.clone();
    let mut title_override: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut turns: Vec<LlmTurn> = Vec::new();

    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut raw_line = Vec::with_capacity(8192);
    let mut bytes_read = 0u64;
    let mut jumped_to_tail = false;

    while let Ok(n) = reader.read_until(b'\n', &mut raw_line) {
        if n == 0 {
            break;
        }
        bytes_read += n as u64;

        // If a transcript is >64 MiB and we already captured opening turns, jump to the final 16 MiB
        if !jumped_to_tail
            && file_len > HUGE_JSONL_HEAD_LIMIT + HUGE_JSONL_TAIL_BYTES
            && bytes_read >= HUGE_JSONL_HEAD_LIMIT
            && turns.len() >= 16
        {
            jumped_to_tail = true;
            if reader
                .seek(SeekFrom::Start(file_len - HUGE_JSONL_TAIL_BYTES))
                .is_ok()
            {
                raw_line.clear();
                let _ = reader.read_until(b'\n', &mut raw_line); // discard partial line
                raw_line.clear();
                continue;
            }
        }

        // Skip absurdly huge >2MB lines (e.g. inline base64 screenshots) before UTF-8/JSON work
        if raw_line.len() > 2 * 1024 * 1024 {
            raw_line.clear();
            continue;
        }

        if !(contains_bytes(&raw_line, b"\"user\"")
            || contains_bytes(&raw_line, b"\"assistant\"")
            || contains_bytes(&raw_line, b"\"summary\""))
        {
            raw_line.clear();
            continue;
        }

        if let Ok(line_str) = std::str::from_utf8(&raw_line)
            && let Ok(parsed) = serde_json::from_str::<RawClaudeLine>(line_str)
        {
            if let Some(sid) = parsed.session_id
                && !sid.is_empty()
            {
                session_id = sid;
            }
            if cwd.is_none() && parsed.cwd.is_some() {
                cwd = parsed.cwd;
            }
            if parsed.kind.as_deref() == Some("summary")
                && let Some(sum) = parsed.summary
                && !sum.trim().is_empty()
            {
                title_override = Some(sum.trim().to_string());
            }
            if let Some(msg) = parsed.message {
                if model.is_none() && msg.model.is_some() {
                    model = msg.model;
                }
                let role = msg.role.unwrap_or_else(|| parsed.kind.unwrap_or_default());
                if (role == "user" || role == "assistant")
                    && let Some(content) = msg.content
                {
                    let extracted = extract_claude_content(content);
                    let cleaned = clean_prompt_wrappers(&extracted);
                    if !cleaned.is_empty() {
                        if turns.len() >= MAX_STORED_TURNS {
                            // Keep opening 60 turns + rolling latest 60 turns
                            turns.remove(MAX_STORED_TURNS / 2);
                        }
                        turns.push(LlmTurn {
                            role,
                            text: truncate_utf8(&cleaned, MAX_TURN_CHARS).to_string(),
                            timestamp: parsed.timestamp,
                        });
                    }
                }
            }
        }
        raw_line.clear();
    }

    if turns.is_empty() {
        return None;
    }

    if let Some(sidecar) = meta_lookup.get(&session_id).or_else(|| meta_lookup.get(&stem)) {
        if title_override.is_none() {
            title_override = sidecar.title.clone();
        }
        if cwd.is_none() {
            cwd = sidecar.cwd.clone();
        }
        if model.is_none() {
            model = sidecar.model.clone();
        }
    }

    // Subagent transcripts (`agent-*.jsonl`) inherit parent `sessionId` inside lines; keep them distinct!
    let unique_id = if stem.starts_with("agent-") && stem != session_id {
        format!("{session_id}:{stem}")
    } else {
        session_id
    };

    let title = title_override.unwrap_or_else(|| derive_title_from_turns(&turns, "Claude Session"));
    Some(LlmSession {
        session_id: format!("claude:{unique_id}"),
        provider: LlmProvider::Claude,
        title,
        workspace: cwd,
        model,
        updated_unix_secs: mtime,
        source_path: path.to_path_buf(),
        turns,
    })
}

fn extract_claude_content(content: RawClaudeContent) -> String {
    match content {
        RawClaudeContent::Text(s) => s,
        RawClaudeContent::Blocks(blocks) => {
            let mut buf = String::new();
            for b in blocks {
                let kind = b.block_type.as_deref().unwrap_or("text");
                if (kind == "text" || kind == "input_text" || kind == "output_text")
                    && let Some(t) = b.text
                    && !t.trim().is_empty()
                {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t.trim());
                }
            }
            buf
        }
    }
}

// =========================================================================
// 2. OpenAI Codex JSONL Streaming Parser (`~/.codex/sessions/**/*.jsonl`)
// =========================================================================

#[derive(Deserialize)]
struct RawCodexLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    timestamp: Option<String>,
    payload: Option<RawCodexPayload>,
}

#[derive(Deserialize)]
struct RawCodexPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    session_id: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    role: Option<String>,
    content: Option<RawClaudeContent>,
}

pub fn parse_codex_jsonl(path: &Path) -> Option<LlmSession> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mtime = file_mtime_secs(&file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("codex")
        .to_string();

    let mut session_id = stem;
    let mut cwd: Option<String> = None;
    let mut turns: Vec<LlmTurn> = Vec::new();

    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut raw_line = Vec::with_capacity(8192);
    let mut bytes_read = 0u64;
    let mut jumped_to_tail = false;

    while let Ok(n) = reader.read_until(b'\n', &mut raw_line) {
        if n == 0 {
            break;
        }
        bytes_read += n as u64;

        if !jumped_to_tail
            && file_len > HUGE_JSONL_HEAD_LIMIT + HUGE_JSONL_TAIL_BYTES
            && bytes_read >= HUGE_JSONL_HEAD_LIMIT
            && turns.len() >= 12
        {
            jumped_to_tail = true;
            if reader
                .seek(SeekFrom::Start(file_len - HUGE_JSONL_TAIL_BYTES))
                .is_ok()
            {
                raw_line.clear();
                let _ = reader.read_until(b'\n', &mut raw_line);
                raw_line.clear();
                continue;
            }
        }

        if raw_line.len() > 2 * 1024 * 1024 {
            raw_line.clear();
            continue;
        }
        // Skip 98% of massive Codex compaction / encrypted_content / world_state blobs before UTF-8/JSON!
        if !(contains_bytes(&raw_line, b"\"session_meta\"")
            || contains_bytes(&raw_line, b"\"message\""))
        {
            raw_line.clear();
            continue;
        }

        if let Ok(line_str) = std::str::from_utf8(&raw_line)
            && let Ok(parsed) = serde_json::from_str::<RawCodexLine>(line_str)
            && let Some(p) = parsed.payload
        {
            match parsed.line_type.as_deref() {
                Some("session_meta") => {
                    if let Some(sid) = p.session_id.or(p.id)
                        && !sid.is_empty()
                    {
                        session_id = sid;
                    }
                    if cwd.is_none() && p.cwd.is_some() {
                        cwd = p.cwd;
                    }
                }
                Some("response_item") if p.payload_type.as_deref() == Some("message") => {
                    let role = p.role.unwrap_or_default();
                    if (role == "user" || role == "assistant")
                        && let Some(content) = p.content
                    {
                        let raw_text = extract_claude_content(content);
                        let cleaned = clean_codex_boilerplate(&raw_text);
                        if !cleaned.is_empty() {
                            if turns.len() >= MAX_STORED_TURNS {
                                turns.remove(MAX_STORED_TURNS / 2);
                            }
                            turns.push(LlmTurn {
                                role,
                                text: truncate_utf8(&cleaned, MAX_TURN_CHARS).to_string(),
                                timestamp: parsed.timestamp,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        raw_line.clear();
    }

    if turns.is_empty() {
        return None;
    }

    let title = derive_title_from_turns(&turns, "Codex Session");
    Some(LlmSession {
        session_id: format!("codex:{session_id}"),
        provider: LlmProvider::Codex,
        title,
        workspace: cwd,
        model: Some("codex".into()),
        updated_unix_secs: mtime,
        source_path: path.to_path_buf(),
        turns,
    })
}

// =========================================================================
// 3. Antigravity / Gemini Transcript Streaming Parser (`transcript.jsonl`)
// =========================================================================

#[derive(Deserialize)]
struct RawAntigravityStep {
    #[serde(rename = "type")]
    step_type: Option<String>,
    created_at: Option<String>,
    content: Option<String>,
}

pub fn parse_antigravity_jsonl(path: &Path) -> Option<LlmSession> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mtime = file_mtime_secs(&file);

    // Extract brain conversation UUID from `.../brain/<conv-id>/.system_generated/logs/transcript.jsonl`
    let conv_id = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("gemini")
        .to_string();

    let mut turns: Vec<LlmTurn> = Vec::new();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut raw_line = Vec::with_capacity(8192);
    let mut bytes_read = 0u64;
    let mut jumped_to_tail = false;

    while let Ok(n) = reader.read_until(b'\n', &mut raw_line) {
        if n == 0 {
            break;
        }
        bytes_read += n as u64;

        if !jumped_to_tail
            && file_len > HUGE_JSONL_HEAD_LIMIT + HUGE_JSONL_TAIL_BYTES
            && bytes_read >= HUGE_JSONL_HEAD_LIMIT
            && turns.len() >= 12
        {
            jumped_to_tail = true;
            if reader
                .seek(SeekFrom::Start(file_len - HUGE_JSONL_TAIL_BYTES))
                .is_ok()
            {
                raw_line.clear();
                let _ = reader.read_until(b'\n', &mut raw_line);
                raw_line.clear();
                continue;
            }
        }

        if raw_line.len() > 2 * 1024 * 1024 {
            raw_line.clear();
            continue;
        }
        if !(contains_bytes(&raw_line, b"\"USER_INPUT\"")
            || contains_bytes(&raw_line, b"\"PLANNER_RESPONSE\""))
        {
            raw_line.clear();
            continue;
        }

        if let Ok(line_str) = std::str::from_utf8(&raw_line)
            && let Ok(step) = serde_json::from_str::<RawAntigravityStep>(line_str)
            && let Some(raw_content) = step.content
        {
            match step.step_type.as_deref() {
                Some("USER_INPUT") => {
                    let cleaned = extract_antigravity_user_request(&raw_content);
                    if !cleaned.is_empty() {
                        if turns.len() >= MAX_STORED_TURNS {
                            turns.remove(MAX_STORED_TURNS / 2);
                        }
                        turns.push(LlmTurn {
                            role: "user".into(),
                            text: truncate_utf8(&cleaned, MAX_TURN_CHARS).to_string(),
                            timestamp: step.created_at,
                        });
                    }
                }
                Some("PLANNER_RESPONSE") => {
                    let trimmed = raw_content.trim();
                    if !trimmed.is_empty() {
                        if turns.len() >= MAX_STORED_TURNS {
                            turns.remove(MAX_STORED_TURNS / 2);
                        }
                        turns.push(LlmTurn {
                            role: "assistant".into(),
                            text: truncate_utf8(trimmed, MAX_TURN_CHARS).to_string(),
                            timestamp: step.created_at,
                        });
                    }
                }
                _ => {}
            }
        }
        raw_line.clear();
    }

    if turns.is_empty() {
        return None;
    }

    let title = derive_title_from_turns(&turns, "Gemini / Antigravity Session");
    Some(LlmSession {
        session_id: format!("gemini:{conv_id}"),
        provider: LlmProvider::Gemini,
        title,
        workspace: None,
        model: Some("Gemini".into()),
        updated_unix_secs: mtime,
        source_path: path.to_path_buf(),
        turns,
    })
}

// =========================================================================
// 4. ChatGPT Export (`conversations.json`), Claude Web Export, & LM Studio
// =========================================================================

pub fn parse_export_or_lmstudio_json(
    path: &Path,
    forced_provider: Option<LlmProvider>,
) -> Vec<LlmSession> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mtime = file_mtime_secs(&file);
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };

    // A) Try array-based export (ChatGPT `conversations.json` or Claude Web `conversations.json`)
    if raw.trim_start().starts_with('[') {
        if let Ok(chatgpt_list) = serde_json::from_str::<Vec<RawChatGptConversation>>(&raw)
            && chatgpt_list.iter().any(|c| c.mapping.is_some())
        {
            return chatgpt_list
                .into_iter()
                .filter_map(|c| convert_chatgpt_export(c, path, mtime))
                .collect();
        }
        if let Ok(claude_web_list) = serde_json::from_str::<Vec<RawClaudeWebConversation>>(&raw)
            && claude_web_list.iter().any(|c| c.chat_messages.is_some())
        {
            return claude_web_list
                .into_iter()
                .filter_map(|c| convert_claude_web_export(c, path, mtime))
                .collect();
        }
    }

    // B) Single-conversation JSON (LM Studio / OpenAI `{ name/title, messages: [...] }`)
    if let Ok(single) = serde_json::from_str::<RawGenericConversationJson>(&raw) {
        let mut turns = Vec::new();
        for m in single.messages.unwrap_or_default() {
            let role = m.role.unwrap_or_default();
            if role != "user" && role != "assistant" {
                continue;
            }
            let txt = match m.content {
                Some(RawGenericMsgContent::Text(s)) => s,
                Some(RawGenericMsgContent::Parts(parts)) => parts
                    .into_iter()
                    .filter_map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => m.text.unwrap_or_default(),
            };
            if !txt.trim().is_empty() && turns.len() < MAX_STORED_TURNS {
                turns.push(LlmTurn {
                    role,
                    text: truncate_utf8(txt.trim(), MAX_TURN_CHARS).to_string(),
                    timestamp: None,
                });
            }
        }
        if !turns.is_empty() {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("conv")
                .to_string();
            let provider = forced_provider.unwrap_or(LlmProvider::LmStudio);
            let title = single
                .name
                .or(single.title)
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| derive_title_from_turns(&turns, provider.as_str()));
            return vec![LlmSession {
                session_id: format!("{}:{stem}", provider.as_str().to_ascii_lowercase().replace(' ', "")),
                provider,
                title,
                workspace: None,
                model: None,
                updated_unix_secs: mtime,
                source_path: path.to_path_buf(),
                turns,
            }];
        }
    }

    Vec::new()
}

#[derive(Deserialize)]
struct RawChatGptConversation {
    id: Option<String>,
    title: Option<String>,
    create_time: Option<f64>,
    update_time: Option<f64>,
    mapping: Option<HashMap<String, RawChatGptNode>>,
}

#[derive(Deserialize)]
struct RawChatGptNode {
    message: Option<RawChatGptMessage>,
}

#[derive(Deserialize)]
struct RawChatGptMessage {
    author: Option<RawChatGptAuthor>,
    create_time: Option<f64>,
    content: Option<RawChatGptContent>,
}

#[derive(Deserialize)]
struct RawChatGptAuthor {
    role: Option<String>,
}

#[derive(Deserialize)]
struct RawChatGptContent {
    parts: Option<Vec<serde_json::Value>>,
}

fn convert_chatgpt_export(
    conv: RawChatGptConversation,
    path: &Path,
    fallback_mtime: i64,
) -> Option<LlmSession> {
    let mapping = conv.mapping?;
    let mut timed_turns: Vec<(f64, LlmTurn)> = Vec::new();

    for node in mapping.into_values() {
        let Some(msg) = node.message else { continue };
        let role = msg.author.and_then(|a| a.role).unwrap_or_default();
        if role != "user" && role != "assistant" {
            continue;
        }
        let mut text = String::new();
        if let Some(parts) = msg.content.and_then(|c| c.parts) {
            for part in parts {
                if let Some(s) = part.as_str()
                    && !s.trim().is_empty()
                {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(s.trim());
                }
            }
        }
        if !text.is_empty() {
            let ts = msg.create_time.unwrap_or(0.0);
            timed_turns.push((
                ts,
                LlmTurn {
                    role,
                    text: truncate_utf8(&text, MAX_TURN_CHARS).to_string(),
                    timestamp: None,
                },
            ));
        }
    }

    if timed_turns.is_empty() {
        return None;
    }
    timed_turns.sort_by(|a, b| a.0.total_cmp(&b.0));
    let turns: Vec<LlmTurn> = timed_turns
        .into_iter()
        .take(MAX_STORED_TURNS)
        .map(|(_, t)| t)
        .collect();

    let id = conv.id.unwrap_or_else(|| {
        let h = blake3::hash(turns[0].text.as_bytes());
        h.to_hex()[..16].to_string()
    });
    let updated = conv
        .update_time
        .or(conv.create_time)
        .map(|t| t as i64)
        .unwrap_or(fallback_mtime);
    let title = conv
        .title
        .filter(|t| !t.trim().is_empty() && t != "New chat")
        .unwrap_or_else(|| derive_title_from_turns(&turns, "ChatGPT Conversation"));

    Some(LlmSession {
        session_id: format!("chatgpt:{id}"),
        provider: LlmProvider::ChatGpt,
        title,
        workspace: None,
        model: Some("ChatGPT".into()),
        updated_unix_secs: updated,
        source_path: path.to_path_buf(),
        turns,
    })
}

#[derive(Deserialize)]
struct RawClaudeWebConversation {
    uuid: Option<String>,
    name: Option<String>,
    chat_messages: Option<Vec<RawClaudeWebMsg>>,
}

#[derive(Deserialize)]
struct RawClaudeWebMsg {
    sender: Option<String>,
    text: Option<String>,
    created_at: Option<String>,
}

fn convert_claude_web_export(
    conv: RawClaudeWebConversation,
    path: &Path,
    mtime: i64,
) -> Option<LlmSession> {
    let msgs = conv.chat_messages?;
    let mut turns = Vec::new();
    for m in msgs {
        let sender = m.sender.unwrap_or_default();
        let role = if sender == "human" || sender == "user" {
            "user"
        } else if sender == "assistant" {
            "assistant"
        } else {
            continue;
        };
        let txt = m.text.unwrap_or_default();
        if !txt.trim().is_empty() && turns.len() < MAX_STORED_TURNS {
            turns.push(LlmTurn {
                role: role.into(),
                text: truncate_utf8(txt.trim(), MAX_TURN_CHARS).to_string(),
                timestamp: m.created_at,
            });
        }
    }
    if turns.is_empty() {
        return None;
    }
    let id = conv.uuid.unwrap_or_else(|| {
        let h = blake3::hash(turns[0].text.as_bytes());
        h.to_hex()[..16].to_string()
    });
    let title = conv
        .name
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| derive_title_from_turns(&turns, "Claude Chat"));

    Some(LlmSession {
        session_id: format!("claude-web:{id}"),
        provider: LlmProvider::Claude,
        title,
        workspace: None,
        model: Some("Claude".into()),
        updated_unix_secs: mtime,
        source_path: path.to_path_buf(),
        turns,
    })
}

#[derive(Deserialize)]
struct RawGenericConversationJson {
    name: Option<String>,
    title: Option<String>,
    messages: Option<Vec<RawGenericMsg>>,
}

#[derive(Deserialize)]
struct RawGenericMsg {
    role: Option<String>,
    content: Option<RawGenericMsgContent>,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawGenericMsgContent {
    Text(String),
    Parts(Vec<RawContentBlock>),
}

// =========================================================================
// Helpers: Boilerplate Stripping & Title Extraction
// =========================================================================

fn extract_antigravity_user_request(raw: &str) -> String {
    if let Some(start) = raw.find("<USER_REQUEST>")
        && let Some(end) = raw[start + 14..].find("</USER_REQUEST>")
    {
        return raw[start + 14..start + 14 + end].trim().to_string();
    }
    if raw.contains("<SYSTEM_MESSAGE>") && !raw.contains("<USER_REQUEST>") {
        return String::new();
    }
    clean_prompt_wrappers(raw)
}

fn clean_codex_boilerplate(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("# AGENTS.md instructions")
    {
        return String::new();
    }
    clean_prompt_wrappers(trimmed)
}

fn clean_prompt_wrappers(raw: &str) -> String {
    let mut s = raw.trim();
    while let Some(start) = s.find("<system-reminder>") {
        if let Some(end) = s[start..].find("</system-reminder>") {
            let after = &s[start + end + 18..];
            if start == 0 {
                s = after.trim();
                continue;
            }
        }
        break;
    }
    s.trim().to_string()
}

fn derive_title_from_turns(turns: &[LlmTurn], fallback: &str) -> String {
    for turn in turns {
        if turn.role == "user" {
            let first_line = turn
                .text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('<') && !l.starts_with("```"))
                .unwrap_or("");
            if !first_line.is_empty() {
                let capped = truncate_utf8(first_line, 76);
                return if capped.len() < first_line.len() {
                    format!("{capped}…")
                } else {
                    capped.to_string()
                };
            }
        }
    }
    fallback.to_string()
}

fn file_mtime_secs(file: &File) -> i64 {
    file.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[inline]
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
