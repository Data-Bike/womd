import os
import random
import sys

TARGET_BYTES = 10 * 1024 * 1024
OUT = os.path.join(os.path.dirname(__file__), "..", "test_large.md")

LOREM = (
    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. "
    "Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. "
    "Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris "
    "nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in "
    "reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla "
    "pariatur. Excepteur sint occaecat cupidatat non proident, sunt in "
    "culpa qui officia deserunt mollit anim id est laborum."
)

def emit_code(i, f):
    f.write(f"```rust\n")
    f.write(f"// Block {i} — example code\n")
    f.write(f"fn process_{i}(data: &[u8]) -> Vec<u8> {{\n")
    f.write(f"    let mut result = Vec::with_capacity(data.len());\n")
    # 8 lines of code per block
    for j in range(8):
        f.write(f"    result.push(data[{j}]);\n")
    f.write(f"    result\n")
    f.write(f"}}\n")
    f.write(f"```\n\n")
    return i

def emit_paragraph(i, f):
    marker = f" Block-{i}-marker for uniqueness and traceability across the document."
    f.write(f"Block {i}: {LOREM}{marker}\n\n")
    return i

def emit_list(i, f):
    f.write(f"Block {i} list:\n")
    for j in range(4):
        f.write(f"- Item {j} in block {i}: consectetur adipiscing elit, sed do eiusmod tempor\n")
    f.write(f"\n")
    return i

def main():
    random.seed(42)
    i = 0
    size = 0
    with open(OUT, "w", encoding="utf-8") as f:
        while size < TARGET_BYTES:
            r = i % 3
            if r == 0:
                emit_code(i, f)
            elif r == 1:
                emit_paragraph(i, f)
            else:
                emit_list(i, f)
            i += 1
            size = f.tell()
            if i % 1000 == 0:
                print(f"generated {i} blocks, {size} bytes", file=sys.stderr)
    print(f"Wrote {OUT}: {os.path.getsize(OUT)} bytes, {i} blocks")

if __name__ == "__main__":
    main()
