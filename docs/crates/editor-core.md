# `editor-core`

Editor core: text edits, edit transactions, undo/redo, selection model and the `DocumentBuffer` facade.

## Responsibilities

- Provide the `TextEdit` primitive — a replacement of a byte range with new bytes.
- Group `TextEdit`s into `EditTransaction`s with before/after selection and undo metadata.
- Implement `UndoManager` for undo and redo.
- Implement `SelectionModel` for multiple carets/ranges and conversions between byte offsets and user coordinates.
- Compose the `PieceTable`, `SyntaxModel` and selection into the `DocumentBuffer` facade used by the UI and Tauri commands.
- Dispatch semantic commands (insert text, toggle heading, wrap selection, etc.) to one or more `TextEdit`s against the piece table.

## Key types

- `DocumentBuffer` — the main facade: open, apply, undo, redo, serialize, get syntax metadata.
- `TextEdit` and `EditTransaction` — the only mutation primitives.
- `UndoManager` — the history stack.
- `SelectionModel` / `Selection` — caret/range bookkeeping.

## Design notes

The UI never mutates strings directly. Every edit is translated to a `Vec<TextEdit>` against the `PieceTable`, then an `EditTransaction` is pushed to the undo stack and applied. This makes all edits testable in Rust without a browser or UI. `DocumentBuffer` is the narrow waist between the editing engine and the Tauri frontend.
