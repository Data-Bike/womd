//! Machine-readable, versioned MCP contract (ADR-007, docs/mcp.md).
//!
//! `tool_specs()` is the single source of truth for `tools/list`, the
//! `api_manifest` tool, and the `womd://manifest` resource. When the editor
//! gains or loses capabilities, this file is updated together with
//! `tools.rs` and `MCP_API_VERSION` per the versioning policy in docs/mcp.md —
//! a consistency test (`specs_match_dispatch`) fails if a spec has no handler.

use serde_json::{Value, json};

use crate::{MCP_API_VERSION, MCP_PROTOCOL_VERSION, SERVER_NAME};

/// One tool in the versioned contract.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Stable tool name (never reused for a different operation).
    pub name: &'static str,
    /// Human/model-facing description of behaviour and constraints.
    pub description: &'static str,
    /// MCP API version that introduced the tool.
    pub since: &'static str,
    /// Last API version that changed this tool's schema/semantics.
    pub revised_in: Option<&'static str>,
    /// Set when the tool is deprecated: the API version that will remove it.
    pub deprecated_in: Option<&'static str>,
    /// JSON Schema for `arguments`.
    pub input_schema: Value,
}

/// Tools that mutate the filesystem; each gets the per-change commit contract
/// (`commit_message` + `commit`, API 2.0.0, ADR-007).
const MUTATING_TOOLS: &[&str] = &[
    "create_document",
    "write_document",
    "append_document",
    "edit_document",
    "delete_document",
    "move_document",
    "create_directory",
    "markdown_block_edit",
    "markdown_toggle_task",
];

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

/// The full tool contract for this API version.
pub fn tool_specs() -> Vec<ToolSpec> {
    let mut specs = tool_specs_inner();
    for s in &mut specs {
        if MUTATING_TOOLS.contains(&s.name) {
            s.input_schema["properties"]["commit"] = json!({
                "type": "boolean",
                "description": "Create a separate git commit for this change. Default true.",
                "default": true
            });
            s.input_schema["properties"]["commit_message"] = json!({
                "type": "string",
                "maxLength": 16384,
                "description": "Commit message YOU choose for this change (required unless commit:false; max 16 KiB, no control chars). A 'Generated-By: womd-mcp' trailer is appended automatically."
            });
            // Write-policy hardening (API 3.0.0): executable file types,
            // credential files/dirs, build/app/CI config and editor/agent
            // config dirs are denied on every mutating tool. Do NOT
            // overwrite a newer revision a tool already carries (e.g.
            // write_document's `expected_content` landed in 3.1.0).
            if s.revised_in.is_none() {
                s.revised_in = Some("3.0.0");
            }
        }
    }
    specs
}

fn tool_specs_inner() -> Vec<ToolSpec> {
    let path_prop = json!({
        "type": "string",
        "description": "Path relative to the workspace root. Absolute paths and '..' escapes are rejected."
    });
    vec![
        ToolSpec {
            name: "list_directory",
            description: "List files and folders under a workspace-relative path, \
                          recursively up to `depth`. Repository internals (.git) and \
                          temp files are never listed.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": {"type": "string", "description": "Subdirectory to list (default: root).", "default": "."},
                    "depth": {"type": "integer", "description": "Recursion depth (default 1, max 6).", "default": 1, "minimum": 0, "maximum": 6},
                    "include_hidden": {"type": "boolean", "description": "Include dotfiles (except .git). Default false.", "default": false}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "read_document",
            description: "Read a document as UTF-8 text with 1-based line numbering. \
                          Use start_line/max_lines to page through large files.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "start_line": {"type": "integer", "description": "First line to return (1-based, default 1).", "default": 1, "minimum": 1},
                    "max_lines": {"type": "integer", "description": "Maximum lines to return (default 400).", "default": 400, "minimum": 1, "maximum": 20000}
                }),
                &["path"],
            ),
        },
        ToolSpec {
            name: "document_info",
            description: "Return metadata for a path: kind, size in bytes, line count, \
                          last-modified time, and whether the content is UTF-8 text.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(json!({"path": path_prop.clone()}), &["path"]),
        },
        ToolSpec {
            name: "create_document",
            description: "Create a new document (parent folders are created as needed). \
                          Fails if the file exists unless `overwrite` is true.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "content": {"type": "string", "description": "Initial UTF-8 content (default empty).", "default": ""},
                    "overwrite": {"type": "boolean", "description": "Replace an existing file. Default false.", "default": false}
                }),
                &["path"],
            ),
        },
        ToolSpec {
            name: "write_document",
            description: "Replace the whole content of a document (created if missing) \
                          via an atomic temp-file+rename save. Prefer `edit_document` \
                          for targeted changes — it produces smaller diffs. Pass \
                          `expected_content` to refuse overwriting a file that changed \
                          since your read (compare-and-save).",
            since: "1.0.0",
            revised_in: Some("3.1.0"),
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "content": {"type": "string", "description": "New complete UTF-8 content."},
                    "expected_content": {"type": "string",
                        "description": "Optional CAS precondition (API 3.1.0): if the file exists, refuse to overwrite unless its current content equals this string."}
                }),
                &["path", "content"],
            ),
        },
        ToolSpec {
            name: "append_document",
            description: "Append text to the end of a document without rewriting the \
                          existing bytes (minimal-diff save).",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "content": {"type": "string", "description": "Text appended verbatim."},
                    "create_if_missing": {"type": "boolean", "default": true}
                }),
                &["path", "content"],
            ),
        },
        ToolSpec {
            name: "edit_document",
            description: "Apply a list of structured edits atomically (validated against \
                          the current content, then saved once). Edit types: \
                          `replace` {search, replace, occurrence?, expected_count?}, \
                          `replace_lines` {start_line, end_line, content}, \
                          `insert_lines` {line, content} (before 1-based line; \
                          line = total+1 appends), `delete_lines` {start_line, end_line}, \
                          `replace_bytes` {start, end, content}. Set `dry_run` to preview \
                          the result without saving.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "edits": {"type": "array", "items": {"type": "object"}, "minItems": 1},
                    "dry_run": {"type": "boolean", "default": false}
                }),
                &["path", "edits"],
            ),
        },
        ToolSpec {
            name: "delete_document",
            description: "Delete a file, or a directory when `recursive` is true. The \
                          workspace root itself can never be deleted.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "recursive": {"type": "boolean", "description": "Required to delete a non-empty directory.", "default": false}
                }),
                &["path"],
            ),
        },
        ToolSpec {
            name: "move_document",
            description: "Rename or move a file/directory within the workspace.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "from": path_prop.clone(),
                    "to": path_prop.clone(),
                    "overwrite": {"type": "boolean", "default": false}
                }),
                &["from", "to"],
            ),
        },
        ToolSpec {
            name: "create_directory",
            description: "Create a directory (including missing parents) inside the workspace.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(json!({"path": path_prop.clone()}), &["path"]),
        },
        ToolSpec {
            name: "search_documents",
            description: "Search document contents. `query` is a literal substring by \
                          default; set `regex` for a regular expression. `file_pattern` \
                          filters names with `*` wildcards (e.g. \"*.md\").",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "query": {"type": "string", "description": "Literal text or regex pattern."},
                    "regex": {"type": "boolean", "default": false},
                    "case_sensitive": {"type": "boolean", "default": true},
                    "path": {"type": "string", "description": "Subdirectory to search (default: whole root).", "default": "."},
                    "file_pattern": {"type": "string", "description": "Filename glob, e.g. '*.md' or 'notes-*'."},
                    "context_lines": {"type": "integer", "default": 0, "maximum": 5},
                    "max_results": {"type": "integer", "default": 100, "maximum": 1000}
                }),
                &["query"],
            ),
        },
        ToolSpec {
            name: "find_files",
            description: "Find files and directories by name pattern (`*` wildcards).",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "pattern": {"type": "string", "description": "Name glob, e.g. '*.md' or 'report-??'.", "default": "*"},
                    "path": {"type": "string", "default": "."},
                    "max_results": {"type": "integer", "default": 200, "maximum": 2000}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "markdown_outline",
            description: "Parse a Markdown document and return its block-level outline: \
                          block index, kind, byte span, line range, heading level/text, \
                          list/code details. Use the block indices with markdown_block_edit.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(json!({"path": path_prop.clone()}), &["path"]),
        },
        ToolSpec {
            name: "markdown_block_edit",
            description: "Edit a Markdown document at block granularity (lossless: only \
                          the affected byte range is rewritten). `op` is one of: \
                          {type:'replace_block', index, content}, \
                          {type:'delete_block', index}, \
                          {type:'insert_block', at, content} (before block `at`; \
                          at = block_count appends), \
                          {type:'replace_block_range', start, end, content} (end exclusive).",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "op": {"type": "object", "description": "Block edit operation."}
                }),
                &["path", "op"],
            ),
        },
        ToolSpec {
            name: "markdown_toggle_task",
            description: "Toggle a GFM task-list checkbox ([ ] <-> [x]) on the item \
                          containing `line` (1-based). Single-byte patch.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": path_prop.clone(),
                    "line": {"type": "integer", "minimum": 1}
                }),
                &["path", "line"],
            ),
        },
        ToolSpec {
            name: "git_status",
            description: "Read-only repository status for the workspace (branch, staged, \
                          modified, untracked, conflicts). Errors when the root is not \
                          inside a git work tree.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(json!({}), &[]),
        },
        ToolSpec {
            name: "git_diff",
            description: "Read-only unified diff. With `path`: working-tree + staged \
                          diff of that file vs HEAD. Without: whole-tree diff vs HEAD.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative file path."},
                    "staged": {"type": "boolean", "description": "Diff the index instead of the working tree.", "default": false}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "git_log",
            description: "Read-only commit history (optionally for one file).",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative file path."},
                    "max_count": {"type": "integer", "default": 20, "maximum": 200}
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "api_manifest",
            description: "Return the complete versioned MCP contract: API version, \
                          protocol version, tool/resource/prompt catalog with `since` \
                          versions, the security policy summary, and the versioning \
                          rules. Call this first if unsure about capabilities.",
            since: "1.0.0",
            revised_in: None,
            deprecated_in: None,
            input_schema: schema(json!({}), &[]),
        },
    ]
}

/// Static resources (plus dynamic `womd://file/<path>` handled in server).
pub fn resource_specs() -> Value {
    json!([
        {
            "uri": "womd://manifest",
            "name": "WoMD MCP contract",
            "mimeType": "application/json",
            "description": "Full versioned API manifest (same payload as the api_manifest tool)."
        },
        {
            "uri": "womd://tree",
            "name": "Workspace tree",
            "mimeType": "application/json",
            "description": "Directory listing of the workspace root (depth 3, capped)."
        },
        {
            "uri": "womd://docs/interaction-guide",
            "name": "Interaction guide",
            "mimeType": "text/markdown",
            "description": "How an AI agent should use this server: workflow, safety rules, versioning."
        }
    ])
}

/// URI templates advertised by `resources/templates/list` — the only dynamic
/// resource this server serves is workspace files.
pub fn resource_template_specs() -> Value {
    json!([
        {
            "uriTemplate": "womd://file/{path}",
            "name": "Workspace file",
            "mimeType": "text/plain",
            "description": "Raw contents of a file inside the workspace (path is URI-encoded, workspace-relative; sandbox policy applies)."
        }
    ])
}

/// Prompt templates exposed via `prompts/*`.
pub fn prompt_specs() -> Value {
    json!([
        {
            "name": "womd_interaction_guide",
            "description": "Recommended workflow for authoring and editing documents through this server.",
            "arguments": []
        },
        {
            "name": "womd_editing_playbook",
            "description": "Recipes for common editing tasks (minimal-diff edits, block surgery, batch changes).",
            "arguments": [
                {"name": "task", "description": "What you want to accomplish", "required": false}
            ]
        }
    ])
}

/// The complete machine-readable manifest (`api_manifest` / `womd://manifest`).
pub fn manifest_json(root_display: &str) -> Value {
    let tools: Vec<Value> = tool_specs()
        .iter()
        .map(|t| {
            let mut o = json!({
                "name": t.name,
                "description": t.description,
                "since": t.since,
                "inputSchema": t.input_schema,
            });
            if let Some(rev) = t.revised_in {
                o["revised_in"] = json!(rev);
            }
            if let Some(dep) = t.deprecated_in {
                o["deprecated"] = json!(true);
                o["deprecated_in"] = json!(dep);
            }
            o
        })
        .collect();
    json!({
        "server": SERVER_NAME,
        "api_version": MCP_API_VERSION,
        "protocol_version": MCP_PROTOCOL_VERSION,
        "workspace_root": root_display,
        "capabilities": {
            "tools": tools,
            "resources": resource_specs(),
            "resource_templates": resource_template_specs(),
            "prompts": prompt_specs(),
        },
        "security_policy": {
            "root_confinement": "All paths are relative to the workspace root; absolute paths, '..' escapes and symlink escapes are rejected. Canonicalized paths are re-checked (8.3 aliases, odd casing).",
            "denied_locations": "OS system directories, filesystem/drive roots, the home directory itself, Desktop/Documents/Downloads, credential dirs (~/.ssh, ~/.aws, ~/.config, .cargo, …), and VCS/tooling internals (.git, .svn, .hg, .bzr, .terraform).",
            "no_execution": "No tool executes arbitrary programs or shell commands. Executable file types (exe/dll/bat/cmd/ps1/vbs/js/msi/jar/app/dmg/pkg/deb/rpm/…) and interpreter scripts (sh/py/rb/pl/php/lua/applescript/…) cannot be created or modified, so no runnable code can be planted — including a fake git binary.",
            "denied_writes": "Files that configure other programs are read-only: build scripts (build.rs, Makefile, CMakeLists.txt, setup.py), package manifests with install hooks (package.json, Cargo.toml, pyproject.toml), CI definitions (.github/workflows/*, .gitlab-ci.yml, Jenkinsfile, …), container specs (Dockerfile, compose, .devcontainer), git behavior config (.gitattributes, .gitmodules, .gitignore — controls commit visibility), git-hook runners (.husky/, lefthook, .pre-commit-config.yaml), and editor/agent config dirs (.vscode, .idea, .claude, .cursor, .devin, .agents, …).",
            "denied_reads": "Secret-bearing names are neither readable nor writable: .env*/.envrc, private keys (id_rsa/id_ed25519, *.pem/*.key/*.p12/*.pfx/*.kdbx/*.kdb/*.ppk/*.ovpn), token stores (.netrc/.npmrc/.yarnrc/.pgpass/.kubeconfig/.htpasswd), IaC secret carriers (*.tfstate/*.tfvars), and credential dirs (.ssh/.aws/.azure/.gnupg/.kube/.docker/.config/.gcloud/.cargo).",
            "names": "Windows device names (CON, NUL, COM1-9, LPT1-9), NTFS-illegal characters, trailing dots/spaces, ADS separators and control characters are rejected in every component.",
            "traversal": "Bulk operations (list/find/search) never follow symlinks; only direct path access via resolve canonicalizes them (inside→inside links work, escapes fail).",
            "concurrency": "Read-modify-write tools (edit_document, append_document, markdown_block_edit, markdown_toggle_task) verify the file is unchanged since it was read (compare-and-save) — an interleaved human or MCP save fails the call instead of being silently overwritten.",
            "limits": "64 MiB per file read/write, 72 MiB per protocol frame, capped traversal (50k entries / 20k files searched / 5k listed).",
            "writes": "Writes go through atomic temp-file+rename saves.",
            "git": "Every mutating tool creates its own path-scoped git commit (commit_message chosen by the agent); commit:false opts out. The workspace is git-init'd on first commit if needed. Only read-only git tools are exposed, and their output is scoped to the workspace subtree. The git binary is resolved to an absolute PATH entry, never through the workspace directory. All git invocations run hardened: no repository hooks, no external fsmonitor command, no external diff program, no filter drivers or gpg signing — merged-config keys that name external programs or credential channels are cleared per-exec, diffs run with --no-ext-diff, and the inherited environment is scrubbed (GIT_DIR/GIT_INDEX_FILE/GIT_CONFIG_*/GIT_EXTERNAL_DIFF/GIT_EXEC_PATH/GIT_SSH*/GIT_TRACE*/BASH_ENV/LD_PRELOAD/pathspec vars), so neither repository config nor parent env can execute code or redirect the repo.",
        },
        "versioning": {
            "scheme": "semver",
            "rules": [
                "MAJOR: tool removed/renamed or incompatible schema change",
                "MINOR: tool/resource/prompt added, or optional parameter added",
                "PATCH: implementation/description fixes only",
                "Deprecated tools keep working for one MAJOR version and are marked with 'deprecated'",
            ],
            "negotiation": "Client discovers capabilities via initialize + tools/list; call api_manifest for the full contract including 'since' versions.",
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mutating-tools mutator injects commit params and a 3.0.0 revision
    /// marker — it must NOT clobber a newer per-tool `revised_in`
    /// (write_document's `expected_content` landed in 3.1.0).
    #[test]
    fn revised_in_reflects_the_latest_change_not_the_oldest() {
        let specs = tool_specs();
        let get = |n: &str| specs.iter().find(|s| s.name == n).unwrap();
        assert_eq!(get("write_document").revised_in, Some("3.1.0"));
        assert_eq!(get("edit_document").revised_in, Some("3.0.0"));
        // Commit params were injected into every mutating schema.
        assert!(get("write_document").input_schema["properties"]["commit"].is_object());
        assert!(get("write_document").input_schema["properties"]["commit_message"].is_object());
        // Read-only tools carry neither commit params nor the revision.
        assert!(
            get("read_document").input_schema["properties"]
                .get("commit")
                .is_none()
        );
        assert_eq!(get("read_document").revised_in, None);
    }

    /// Every mutating tool must offer the per-change-commit contract.
    #[test]
    fn all_mutating_tools_have_commit_params() {
        for s in tool_specs() {
            let has_commit = s.input_schema["properties"].get("commit").is_some();
            assert_eq!(has_commit, MUTATING_TOOLS.contains(&s.name), "{}", s.name);
        }
    }
}
