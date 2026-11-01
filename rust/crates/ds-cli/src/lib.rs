//! The `dunkingsheep` command line. T42 is the parser only: the argparse tree as data
//! (`cli_table`, generated), a clap `Command` built from it (`args`), and the argparse
//! `Namespace` shape recovered from the matches. Command bodies land in T43 onwards.
pub mod args;
pub mod cli_table;
pub mod context_cmd;
pub mod github_cmd;
pub mod local;
pub mod render;
pub mod render_fleet;
pub mod render_print;
pub mod spec;
pub mod usage_cmd;
