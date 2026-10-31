use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fms_core_engine::ipc::{IpcRequest, IpcResponse, send_ipc_request};
use fms_core_engine::{EngineConfig, LlmSession, SearchEngine, SearchResponse, SearchScope};
use fms_system1_predictor::QueryContext;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum WireFraming {
    NewlineJson,
    ContentLength,
}

/// Runs the Fast Mac Search Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdin`/`stdout`.
/// Connects to the warm `searchd` Unix Domain Socket (`~15 µs` IPC) when running, with automatic
/// fallback to opening the encrypted `SearchEngine` directly in-process.
pub fn run_mcp_stdio_server(home: Option<PathBuf>, sock_path: PathBuf) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut fallback_engine: Option<SearchEngine> = None;

    while let Some((msg, framing)) = read_mcp_frame(&mut reader)? {
        // JSON-RPC notifications do not carry an `"id"` and must NOT emit a response frame.
        let Some(req_id) = msg.get("id").cloned() else {
            continue;
        };

        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        let reply = match method {
            "initialize" => {
                let client_proto = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                let negotiated = match client_proto {
                    "2025-06-18" | "2025-03-26" | "2024-11-05" => client_proto,
                    _ => "2024-11-05",
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "protocolVersion": negotiated,
                        "capabilities": {
                            "tools": { "listChanged": false }
                        },
                        "serverInfo": {
                            "name": "fast-mac-search",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "instructions": "Fast Mac Search gives you sub-millisecond encrypted search across local/external Mac drives AND past AI conversations (Claude Code, Claude Desktop, OpenAI Codex, Gemini/Antigravity, ChatGPT, LM Studio). Use `search_llm_chats` + `get_llm_conversation` to recall prior discussions or code fixes, and `search_all` to search files and chats together."
                    }
                })
            }

            "ping" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {}
            }),

            "resources/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": { "resources": [] }
            }),

            "prompts/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": { "prompts": [] }
            }),

            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": tool_definitions()
                }
            }),

            "tools/call" => {
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

                match dispatch_tool_call(
                    tool_name,
                    &args,
                    &sock_path,
                    home.as_deref(),
                    &mut fallback_engine,
                ) {
                    Ok(text_out) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{ "type": "text", "text": text_out }],
                            "isError": false
                        }
                    }),
                    Err(err_msg) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("Error: {err_msg}") }],
                            "isError": true
                        }
                    }),
                }
            }

            other => json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {other}")
                }
            }),
        };

        write_mcp_frame(&mut writer, &reply, framing)?;
    }

    Ok(())
}

fn read_mcp_frame(reader: &mut impl BufRead) -> Result<Option<(Value, WireFraming)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Framing A: Standard MCP newline-delimited JSON-RPC (`2024-11-05`+)
        if trimmed.starts_with('{') {
            let val: Value = serde_json::from_str(trimmed)
                .with_context(|| "invalid newline JSON-RPC message on MCP stdin")?;
            return Ok(Some((val, WireFraming::NewlineJson)));
        }

        // Framing B: Legacy `Content-Length: <N>\r\n\r\n` header framing
        if let Some(rest) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
        {
            let content_len: usize = rest
                .trim()
                .parse()
                .context("invalid Content-Length header on MCP stdin")?;

            // Drain remaining header lines up to the blank separator line
            let mut hdr = String::new();
            loop {
                hdr.clear();
                let hn = reader.read_line(&mut hdr)?;
                if hn == 0 || hdr.trim().is_empty() {
                    break;
                }
            }

            let mut body = vec![0u8; content_len];
            reader.read_exact(&mut body)?;
            let val: Value =
                serde_json::from_slice(&body).context("invalid Content-Length JSON-RPC payload")?;
            return Ok(Some((val, WireFraming::ContentLength)));
        }
    }
}

fn write_mcp_frame(writer: &mut impl Write, payload: &Value, framing: WireFraming) -> Result<()> {
    let bytes = serde_json::to_vec(payload)?;
    match framing {
        WireFraming::NewlineJson => {
            writer.write_all(&bytes)?;
            writer.write_all(b"\n")?;
        }
        WireFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
            writer.write_all(&bytes)?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search_all",
            "description": "Sub-millisecond search across encrypted Mac files AND past AI conversations (Claude, Codex, Gemini/Antigravity, ChatGPT, LM Studio). Uses ARM NEON fuzzy matching, Tantivy BM25 full-text, and 128d semantic vectors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (filenames, code symbols, keywords, natural language concepts, or @claude / @codex / @gemini prefixes)"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["all", "files", "chats"],
                        "description": "Filter results to 'all' (default), 'files' only, or 'chats' (LLM conversations only)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 12, max: 50)"
                    }
                },
                "required": ["query"]
            }
        },
        {
            "name": "search_llm_chats",
            "description": "Search past local & imported AI conversations across Claude Code, Claude Desktop, OpenAI Codex, Gemini/Antigravity, ChatGPT, and LM Studio. Leave `query` empty to list the most recent conversations.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords, error message, feature name, or concept discussed in a past AI chat (optional; empty returns recent chats)"
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["claude", "codex", "gemini", "chatgpt", "lmstudio"],
                        "description": "Optional LLM provider filter"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum conversations to return (default: 10, max: 40)"
                    }
                }
            }
        },
        {
            "name": "get_llm_conversation",
            "description": "Fetch the full clean dialogue transcript (user & assistant turns, workspace, model, source path) of a conversation returned by `search_llm_chats` or `search_all`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Exact `session_id` from search results (e.g. 'claude:...', 'codex:...', 'gemini:...', 'chatgpt:...')"
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Maximum dialogue turns to return (default: 60)"
                    }
                },
                "required": ["session_id"]
            }
        },
        {
            "name": "search_status",
            "description": "Inspect Fast Mac Search encrypted drive shards, indexed file counts, Secure Enclave / key backend status, and indexed LLM chat counts by provider.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}

fn ensure_engine<'a>(
    home: Option<&Path>,
    slot: &'a mut Option<SearchEngine>,
) -> std::result::Result<&'a SearchEngine, String> {
    if slot.is_none() {
        let mut cfg = EngineConfig::default();
        if let Some(h) = home {
            cfg.app_support_dir = h.to_path_buf();
        }
        let eng = SearchEngine::open(cfg).map_err(|e| e.to_string())?;
        if eng.llm_memory().session_count() == 0 {
            let _ = eng.sync_llm_chats();
        }
        *slot = Some(eng);
    }
    Ok(slot.as_ref().unwrap())
}

fn dispatch_tool_call(
    tool_name: &str,
    args: &Value,
    sock_path: &Path,
    home: Option<&Path>,
    fallback_engine: &mut Option<SearchEngine>,
) -> std::result::Result<String, String> {
    match tool_name {
        "search_all" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let scope_str = args.get("scope").and_then(Value::as_str).unwrap_or("all");
            let scope = SearchScope::from_tag(scope_str);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(12)
                .clamp(1, 50) as usize;

            let resp = run_query(sock_path, home, fallback_engine, &query, scope, limit)?;
            Ok(format_search_response(&resp))
        }

        "search_llm_chats" => {
            let raw_q = args.get("query").and_then(Value::as_str).unwrap_or("");
            let provider = args.get("provider").and_then(Value::as_str).unwrap_or("");
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 40) as usize;

            let effective_q = if provider.is_empty() {
                raw_q.to_string()
            } else {
                format!("@{} {}", provider.trim_start_matches('@'), raw_q)
            };

            let resp = run_query(
                sock_path,
                home,
                fallback_engine,
                &effective_q,
                SearchScope::Chats,
                limit,
            )?;
            Ok(format_search_response(&resp))
        }

        "get_llm_conversation" => {
            let session_id = args
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required `session_id` parameter".to_string())?;
            let max_turns = args
                .get("max_turns")
                .and_then(Value::as_u64)
                .unwrap_or(60)
                .clamp(1, 120) as usize;

            let session = if sock_path.exists() {
                match send_ipc_request(
                    sock_path,
                    &IpcRequest::GetLlmConversation {
                        session_id: session_id.to_string(),
                    },
                ) {
                    Ok(IpcResponse::LlmConversation { session }) => session,
                    Ok(IpcResponse::Error { message }) => return Err(message),
                    _ => {
                        let eng = ensure_engine(home, fallback_engine)?;
                        eng.get_llm_session(session_id)
                            .ok_or_else(|| format!("LLM session not found: {session_id}"))?
                    }
                }
            } else {
                let eng = ensure_engine(home, fallback_engine)?;
                eng.get_llm_session(session_id)
                    .ok_or_else(|| format!("LLM session not found: {session_id}"))?
            };

            Ok(format_llm_session(&session, max_turns))
        }

        "search_status" => {
            let status = if sock_path.exists()
                && let Ok(IpcResponse::Status(s)) = send_ipc_request(sock_path, &IpcRequest::Status)
            {
                s
            } else {
                let eng = ensure_engine(home, fallback_engine)?;
                eng.status()
            };
            serde_json::to_string_pretty(&status).map_err(|e| e.to_string())
        }

        other => Err(format!("unknown tool: {other}")),
    }
}

fn run_query(
    sock_path: &Path,
    home: Option<&Path>,
    fallback_engine: &mut Option<SearchEngine>,
    query: &str,
    scope: SearchScope,
    limit: usize,
) -> std::result::Result<SearchResponse, String> {
    let ctx = QueryContext {
        active_app_bundle_id: None,
        active_workspace_dir: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        now_unix_secs: None,
    };

    if sock_path.exists()
        && let Ok(IpcResponse::Search(r)) = send_ipc_request(
            sock_path,
            &IpcRequest::Query {
                query: query.to_string(),
                limit,
                context: ctx.clone(),
                l0_only: false,
                scope,
            },
        )
    {
        return Ok(r);
    }

    let eng = ensure_engine(home, fallback_engine)?;
    Ok(eng.search_scoped(query, limit, &ctx, false, scope))
}

fn format_search_response(resp: &SearchResponse) -> String {
    let mut out = format!(
        "Fast Mac Search — {:.2} ms ({} hits across {} indexed items)\n\n",
        resp.total_elapsed_us as f64 / 1000.0,
        resp.hits.len(),
        resp.total_indexed_paths
    );

    if resp.hits.is_empty() {
        out.push_str("No matching results found.\n");
        return out;
    }

    for (idx, h) in resp.hits.iter().enumerate() {
        if h.is_llm_chat {
            let prov = h.llm_provider.as_deref().unwrap_or("AI Chat");
            let sid = h.llm_session_id.as_deref().unwrap_or("");
            let ws = h
                .workspace
                .as_deref()
                .map(|w| format!(" · workspace: `{w}`"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}. **[{} Chat]** {}\n   - `session_id`: `{}`{}\n",
                idx + 1,
                prov,
                h.filename,
                sid,
                ws
            ));
            if !h.snippet.is_empty() {
                out.push_str(&format!("   - Snippet: {}\n", h.snippet));
            }
        } else {
            let status_tag = if h.volume_online {
                h.volume_name.clone()
            } else {
                format!("OFFLINE · {}", h.volume_name)
            };
            out.push_str(&format!(
                "{}. **[File · {}]** `{}`\n",
                idx + 1,
                status_tag,
                h.path
            ));
            if !h.snippet.is_empty() {
                out.push_str(&format!("   - Snippet: {}\n", h.snippet));
            }
        }
    }

    out
}

fn format_llm_session(session: &LlmSession, max_turns: usize) -> String {
    let mut out = format!(
        "# [{}] {}\n- **Session ID**: `{}`\n",
        session.provider.as_str(),
        session.title,
        session.session_id
    );
    if let Some(ref ws) = session.workspace {
        out.push_str(&format!("- **Workspace**: `{ws}`\n"));
    }
    if let Some(ref m) = session.model {
        out.push_str(&format!("- **Model**: `{m}`\n"));
    }
    out.push_str(&format!(
        "- **Source**: `{}`\n- **Turns**: {} (showing up to {})\n\n---\n\n",
        session.source_path.display(),
        session.turns.len(),
        max_turns
    ));

    for (idx, turn) in session.turns.iter().take(max_turns).enumerate() {
        let speaker = if turn.role == "user" {
            "USER"
        } else {
            "ASSISTANT"
        };
        let ts = turn
            .timestamp
            .as_deref()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default();
        out.push_str(&format!("### [{speaker} #{}{ts}]\n{}\n\n", idx + 1, turn.text));
    }

    out
}

/// Safely registers `fast-mac-search` inside Claude Desktop's `claude_desktop_config.json`
/// without overwriting any existing user MCP servers, and returns the resolved binary path.
pub fn install_mcp_into_claude_desktop() -> Result<(PathBuf, PathBuf)> {
    let app_helper = PathBuf::from("/Applications/Fast Mac Search.app/Contents/Helpers/fms");
    let fms_bin = if app_helper.exists() {
        app_helper
    } else {
        std::env::current_exe().context("cannot determine current fms executable path")?
    };

    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let config_path = PathBuf::from(home)
        .join("Library/Application Support/Claude/claude_desktop_config.json");

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut root_obj: Value = if config_path.exists() {
        let raw = fs::read_to_string(&config_path).unwrap_or_default();
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
        }
    } else {
        json!({})
    };

    if !root_obj.is_object() {
        root_obj = json!({});
    }
    let root_map = root_obj.as_object_mut().unwrap();
    let servers = root_map
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }

    servers.as_object_mut().unwrap().insert(
        "fast-mac-search".to_string(),
        json!({
            "command": fms_bin.to_string_lossy(),
            "args": ["mcp"]
        }),
    );

    let formatted = serde_json::to_vec_pretty(&root_obj)?;
    let tmp_path = config_path.with_extension("json.tmp");
    fs::write(&tmp_path, formatted)?;
    fs::rename(&tmp_path, &config_path)?;

    Ok((fms_bin, config_path))
}
