//! `womd-mcp` — MCP stdio server exposing sandboxed WoMD document tools.
//!
//! stdout is reserved for JSON-RPC frames; logs go to stderr.
//!
//! Usage:
//!   womd-mcp --root <dir>     serve MCP over stdio, sandboxed to <dir>
//!   womd-mcp --manifest       print the versioned API manifest (JSON) and exit
//!   womd-mcp --version        print server + API version and exit
//!
//! The root may also come from `WOMD_MCP_ROOT`.

#![forbid(unsafe_code)]

use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use editor_mcp::manifest::manifest_json;
use editor_mcp::sandbox::Workspace;
use editor_mcp::server::McpServer;
use editor_mcp::tools::Tools;
use editor_mcp::{MCP_API_VERSION, MCP_PROTOCOL_VERSION, SERVER_NAME};

fn usage() -> &'static str {
    "womd-mcp --root <dir> | --manifest [--root <dir>] | --version\n\
     MCP (Model Context Protocol) stdio server for the WoMD document workspace.\n\
     Tools are confined to --root; system dirs, home root, Desktop, .git and\n\
     credential folders are denied. No program execution."
}

fn main() -> ExitCode {
    let mut root: Option<PathBuf> = std::env::var_os("WOMD_MCP_ROOT").map(PathBuf::from);
    let mut print_manifest = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => match args.next() {
                Some(v) => root = Some(PathBuf::from(v)),
                None => {
                    eprintln!("--root requires a directory argument");
                    return ExitCode::from(2);
                }
            },
            "--manifest" => print_manifest = true,
            "--version" | "-V" => {
                println!(
                    "{SERVER_NAME} {} (MCP API {MCP_API_VERSION}, protocol {MCP_PROTOCOL_VERSION})",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            s if s.starts_with("--root=") => {
                root = Some(PathBuf::from(&s["--root=".len()..]));
            }
            other => {
                eprintln!("unknown argument '{other}'\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    // `--manifest` is informational and works without a root.
    if print_manifest {
        let root_display = match &root {
            Some(r) => match Workspace::new(r) {
                Ok(w) => w.root().display().to_string(),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            },
            None => "<unspecified>".to_string(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&manifest_json(&root_display)).unwrap()
        );
        return ExitCode::SUCCESS;
    }

    let root = match root {
        Some(r) => r,
        None => {
            eprintln!(
                "no workspace root. Pass --root <dir> or set WOMD_MCP_ROOT.\n{}",
                usage()
            );
            return ExitCode::from(2);
        }
    };

    let ws = match Workspace::new(&root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "{SERVER_NAME} {} serving {} (MCP API {MCP_API_VERSION})",
        env!("CARGO_PKG_VERSION"),
        ws.root().display()
    );
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let server = McpServer::new(Tools::new(ws));
    if let Err(e) = server.run_stdio(BufReader::new(stdin.lock()), stdout.lock()) {
        let _ = writeln!(std::io::stderr(), "stdio loop failed: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
