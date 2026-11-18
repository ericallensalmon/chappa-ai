//! `chappa-ai-mcp` — stdio MCP server for the chappa-ai desktop app.
//! Register with Claude Code by its full path, so an agent finds it whether
//! or not its folder is on `PATH` (an app bundle's is not):
//! `claude mcp add chappa-ai -- "/full/path/to/chappa-ai-mcp"`; `--help`
//! prints that line with this binary's own path.
//! Environment: CHAPPA_AI_CONTROL_URL (default: the address in the app's
//! `control_endpoint` file, or CHAPPA_AI_CONTROL_ENDPOINT_FILE),
//! CHAPPA_AI_CONTROL_TOKEN or CHAPPA_AI_CONTROL_TOKEN_FILE (default: the
//! app-config `control_token`). CHAPPA_PROFILE_DIR moves the app-config
//! folder into a separate profile, the same way it does for the app. All
//! logic lives in the library; this is the loop.
//!
//! `chappa-ai-mcp session-hook` is the other entry point: a Claude Code
//! `SessionStart` hook that reports the session id of the app-spawned agent
//! it runs under (see `session_hook`). It always exits 0.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(chappa_ai_mcp::session_hook::SUBCOMMAND) {
        chappa_ai_mcp::session_hook::main();
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        // This binary's own path, so the registration line works as printed.
        let me = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "chappa-ai-mcp".to_owned());
        eprintln!(
            "chappa-ai-mcp {} — stdio MCP server proxying to the chappa-ai control surface.\n\
             Usage: claude mcp add chappa-ai -- \"{me}\"\n\
             \x20      chappa-ai-mcp session-hook  (Claude Code SessionStart hook; JSON on stdin)\n\
             Env: CHAPPA_AI_CONTROL_URL, CHAPPA_AI_CONTROL_ENDPOINT_FILE,\n\
             CHAPPA_AI_CONTROL_TOKEN, CHAPPA_AI_CONTROL_TOKEN_FILE, CHAPPA_PROFILE_DIR\n\
             Default token file: {}\n\
             Default endpoint file: {}",
            chappa_ai_mcp::SERVER_VERSION,
            chappa_ai_mcp::default_token_path().display(),
            chappa_ai_mcp::default_config_dir()
                .join(chappa_ai_mcp::ENDPOINT_FILE)
                .display()
        );
        return;
    }
    let client = chappa_ai_mcp::Client::from_env();
    chappa_ai_mcp::serve_stdio(&client);
}
