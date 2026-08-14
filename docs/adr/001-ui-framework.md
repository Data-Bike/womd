# ADR-001: UI framework

## Context
The editor must be cross-platform (Windows/macOS/Linux), Word-like (not IDE-like), with
rich text WYSIWYG editing, IME, accessibility, high-DPI, theming, plugin UI contributions,
fast startup, and small memory footprint (§75–76, §105–107). Rust must remain the core
regardless of UI choice. The editing surface must NOT be a textarea (§76); a custom
editing engine is required.

## Options
1. **Native Rust UI (iced / egui / Slint).**
   - Pros: pure Rust, no WebView, small binary.
   - Cons: rich text editing + IME + accessibility are immature for a Word-like document
     surface; plugin UI contributions would need a bespoke component protocol; high-DPI
     and text shaping vary. Building a production WYSIWYG text engine on top is a multi-year
     effort and risks Invariant 7 (typing latency) and accessibility (§97).
2. **Tauri 2 (Rust core + system WebView) with a custom editing surface.**
   - Pros: cross-platform, small footprint (system WebView, not Chromium bundle), mature
     IME/clipboard/accessibility/high-DPI via the platform WebView, CSS-token theming
     (§30), plugin UI contributions via web components, fast startup. Rust stays the core;
     the WebView is a thin view that forwards input events to Rust and renders snapshots
     produced by the Rust document model.
   - Cons: WebView inconsistency across platforms (Edge WebView2 / WKWebView / WebKitGTK);
     must avoid `contenteditable` and build a custom rendering/input surface (canvas or
     controlled DOM) driven by Rust — this is required anyway by §76.
3. **Hybrid: native shell (winit + wgpu) + embedded HTML editing surface.**
   - Pros: maximal control.
   - Cons: reimplements windowing, accessibility, IME, clipboard, theming — duplicates what
     Tauri already provides; highest risk and cost.

## Decision
**Tauri 2** as the application shell, with a **custom editing surface** (not textarea, not
`contenteditable`) rendered from Rust-produced document snapshots and forwarding raw input
events to the Rust editing engine. All editing logic (selection, commands, undo, IME
composition handling) lives in `editor-core`/`editor-text`/`editor-markdown`; the UI layer
is a view only.

## Consequences
- `editor-ui` depends on Tauri + a thin rendering crate; no other crate depends on UI.
- The editing engine is UI-framework-agnostic and fully testable in Rust (no browser needed
  for core logic). This protects Invariants 1, 2, 7.
- Theming uses CSS custom properties generated from `Theme` tokens (§30) — single design
  system (Invariant 9).
- Plugin UI contributions are constrained to the WebView sandbox and the registered
  extension points; no ambient DOM access to editor internals.
- We accept WebView variance; mitigated by a strict rendering contract (Rust emits a
  declarative viewport description; UI only applies it).
