//! `dunkingsheep-mcp`: the MCP server over stdio (also reachable as `dunkingsheep mcp`).

fn main() {
    std::process::exit(ds_mcp::run_stdio());
}
