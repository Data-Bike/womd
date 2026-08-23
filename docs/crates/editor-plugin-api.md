# `editor-plugin-api`

Stable plugin API: traits, manifest schema, capabilities, extension points and the event bus contract.

## Responsibilities

- Define the traits that plugins implement against a versioned SDK.
- Declare the plugin manifest format (TOML) including declared API version and requested capabilities.
- Provide the capability/permission model (`filesystem`, `network`, `git_credentials`, `clipboard`, `process`, etc.).
- Specify extension points: commands, toolbar/menu items, shortcuts, Markdown syntax extensions, block/inline renderers, themes, exporters/importers, repository providers, asset handlers, sidebar panels and status bar items.
- Define the `EventBus` contract so plugins can observe editor events without direct coupling to core.

## Key types

- `Plugin` — the main plugin lifecycle trait.
- `Manifest` and `Capability` — manifest parsing and permission declarations.
- `ExtensionPoint` and related traits — the hooks a plugin can register.
- `EventBus` / `Event` — typed event stream.

## Design notes

This crate depends only on `editor-domain`, making it safe for plugins to implement without pulling in the whole editor. Core never depends on a plugin; plugins implement these traits and are loaded by `editor-plugin-host`. The capability model defaults to deny so a plugin cannot use an authority it has not declared.
