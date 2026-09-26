// Optional large-file stress test (§95). Parses the generated ~100 MiB file
// (test-data/womd-stress-100mb.md — produced by crates/editor-ui/e2e/gen-stress-md.mjs,
// gitignored) through the real parser and verifies byte-identical serialization.
// Skips when the file isn't present; ignored by default.
// Run: cargo test -p editor-markdown --test stress_large_file -- --ignored --nocapture
use std::time::Instant;

#[test]
#[ignore = "requires generated 100 MiB file; run explicitly"]
fn stress_file_parses_and_roundtrips() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data/womd-stress-100mb.md");
    if !std::path::Path::new(path).exists() {
        eprintln!("stress file not generated (node e2e/gen-stress-md.mjs); skipping");
        return;
    }
    let t = Instant::now();
    let src = std::fs::read(path).unwrap();
    eprintln!("read: {:?} ({} bytes)", t.elapsed(), src.len());

    let t = Instant::now();
    let doc = editor_markdown::parse(&src, editor_domain::profile::MarkdownProfile::Gfm)
        .expect("stress file must parse");
    eprintln!("parse: {:?} — {} blocks", t.elapsed(), doc.blocks.len());

    let t = Instant::now();
    let out = editor_markdown::serialize(&doc, &src);
    eprintln!("serialize: {:?}", t.elapsed());
    assert_eq!(out, src, "round-trip must be byte-identical");
}
