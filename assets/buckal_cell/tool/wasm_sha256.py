"""Write the lowercase hex SHA-256 of a file, with no newline: `wasm_sha256.py SRC OUT`."""

import hashlib
import sys


def main() -> None:
    src, out = sys.argv[1:]
    digest = hashlib.sha256()
    with open(src, "rb") as module:
        for block in iter(lambda: module.read(1 << 20), b""):
            digest.update(block)
    with open(out, "w", encoding="ascii") as witness:
        witness.write(digest.hexdigest())


if __name__ == "__main__":
    main()
