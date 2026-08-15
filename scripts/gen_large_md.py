#!/usr/bin/env python3
"""Generate a large Markdown file (>100 MB) for virtualization testing.

The file contains a mix of block types (headings, paragraphs, lists, code,
tables, block quotes) to exercise the parser and renderer. Content is
deterministic so byte offsets are reproducible.

Usage:
    python scripts/gen_large_md.py [output_path] [target_mb]

Defaults: output=test_large.md, target=120 MB
"""
import os
import sys

def gen_block(i: int) -> str:
    """Generate one Markdown block (~1-2 KB) with varied content."""
    lines = []
    # Every 50th block is a heading.
    if i % 50 == 0:
        level = (i // 50) % 6 + 1
        lines.append(f"{'#' * level} Section {i} — Lorem Ipsum Dolor")
        lines.append("")
    # Every 7th block is a code block.
    if i % 7 == 0:
        lines.append("```rust")
        lines.append(f"// Block {i} — example code")
        lines.append(f"fn process_{i}(data: &[u8]) -> Vec<u8> {{")
        lines.append(f"    let mut result = Vec::with_capacity(data.len());")
        lines.append(f"    for (idx, &byte) in data.iter().enumerate() {{")
        lines.append(f"        result.push(byte ^ (idx as u8 & 0xFF));")
        lines.append(f"    }}")
        lines.append(f"    result")
        lines.append(f"}}")
        lines.append("```")
        lines.append("")
    # Every 11th block is a list.
    elif i % 11 == 0:
        for j in range(8):
            lines.append(f"- Item {j} in block {i}: consectetur adipiscing elit, sed do eiusmod tempor")
        lines.append("")
    # Every 13th block is a block quote.
    elif i % 13 == 0:
        lines.append(f"> Quotation from block {i}: Ut enim ad minim veniam,")
        lines.append(f"> quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo")
        lines.append(f"> consequat. Duis aute irure dolor in reprehenderit in voluptate velit.")
        lines.append("")
    # Every 23rd block is a table.
    elif i % 23 == 0:
        lines.append(f"| Column A | Column B | Column C |")
        lines.append(f"|----------|----------|----------|")
        for j in range(5):
            lines.append(f"| Row {j}-{i} | Data {(j*100+i):06d} | Value {j*i} |")
        lines.append("")
    # Every 29th block is a thematic break.
    elif i % 29 == 0:
        lines.append("---")
        lines.append("")
    # Default: a paragraph with lorem ipsum text (~800 chars).
    else:
        para = (
            f"Block {i}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. "
            f"Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. "
            f"Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris "
            f"nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in "
            f"reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla "
            f"pariatur. Excepteur sint occaecat cupidatat non proident, sunt in "
            f"culpa qui officia deserunt mollit anim id est laborum. "
            f"Block-{i}-marker for uniqueness and traceability across the document."
        )
        lines.append(para)
        lines.append("")
    return "\n".join(lines)


def main():
    output_path = sys.argv[1] if len(sys.argv) > 1 else "test_large.md"
    target_mb = int(sys.argv[2]) if len(sys.argv) > 2 else 120
    target_bytes = target_mb * 1024 * 1024

    print(f"Generating {target_mb} MB ({target_bytes:,} bytes) Markdown file: {output_path}")
    block_count = 0
    written = 0

    # Write in streaming mode — never holds the full file in memory.
    with open(output_path, "w", encoding="utf-8", buffering=1024 * 1024) as f:
        i = 0
        while written < target_bytes:
            chunk = gen_block(i)
            f.write(chunk)
            written += len(chunk.encode("utf-8"))
            i += 1
            block_count += 1
            if i % 1000 == 0:
                print(f"  ...{written / (1024*1024):.1f} MB, {block_count} blocks", flush=True)

    actual_mb = os.path.getsize(output_path) / (1024 * 1024)
    print(f"\nDone: {actual_mb:.1f} MB, {block_count} blocks")
    print(f"File: {os.path.abspath(output_path)}")


if __name__ == "__main__":
    main()
