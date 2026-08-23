# `editor-ui`

Tauri 2 desktop application shell and Vue/Vite frontend for WoMD.

## Responsibilities

- Provide the main window, menu bar, status bar, command palette and keyboard shortcuts.
- Render the document as a virtualized list of Markdown blocks in the rendered view.
- Provide the raw source view as a full-document `<textarea>`.
- Forward user input (typing, formatting commands, find/replace, Git actions) to the Rust core via Tauri commands.
- Apply the theme through CSS custom properties generated from `editor-domain` tokens.
- Display Git status, diff panels, file tree and other side panels.

## Key types

- Tauri commands in `src/main.rs` (`open_document`, `get_document_text`, `search_document`, `replace_match_in_document`, etc.).
- `MarkdownEditor.vue`, `MenuBar.vue`, `ContextMenu.vue`, `FindBar.vue` — main frontend components.
- `src/lib/byteOffset.js`, `src/lib/textEditing.js`, `src/lib/search.js` — pure JavaScript helpers.

## Design notes

The UI is deliberately thin: all editing logic lives in Rust. The frontend only renders what the core provides and dispatches commands. Block rendering is virtualized, so only the visible slice of a document is in the DOM at once. Source view uses a single `<textarea>`; rendered view uses a per-block edit-in-place model. This is a foundation (vanilla JS/Vue) intended to be evolved into richer views later.
