//! `chappa-ai-mcp` — stdio MCP server for the chappa-ai desktop app.
//! Register with Claude Code: `claude mcp add chappa-ai -- chappa-ai-mcp`.
//! Environment: CHAPPA_AI_CONTROL_URL (default http://127.0.0.1:8324),
//! CHAPPA_AI_CONTROL_TOKEN or CHAPPA_AI_CONTROL_TOKEN_FILE (default: the
//! app-config `control_token`). All logic lives in the library; this is the
//! loop.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "chappa-ai-mcp {} — stdio MCP server proxying to the chappa-ai control surface.\n\
             Usage: claude mcp add chappa-ai -- chappa-ai-mcp\n\
             Env: CHAPPA_AI_CONTROL_URL, CHAPPA_AI_CONTROL_TOKEN, CHAPPA_AI_CONTROL_TOKEN_FILE\n\
             Default token file: {}",
            chappa_ai_mcp::SERVER_VERSION,
            chappa_ai_mcp::default_token_path().display()
        );
        return;
    }
    let client = chappa_ai_mcp::Client::from_env();
    chappa_ai_mcp::serve_stdio(&client);
}
