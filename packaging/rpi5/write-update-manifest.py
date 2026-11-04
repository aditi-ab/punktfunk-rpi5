"""Describe the app archive and the OS ABI it can run on."""
import hashlib
import json
from pathlib import Path
import re
import sys

archive = Path(sys.argv[1])
version = sys.argv[2].removeprefix("v")
commit = sys.argv[3]
assert re.fullmatch(r"\d+\.\d+\.\d+-rpi5\.\d+", version)
assert re.fullmatch(r"[0-9a-f]{40}", commit)
assert archive.name == f"punktfunk-{version}-linux-arm64.tar.gz"
with archive.open("rb") as source:
    digest = hashlib.file_digest(source, "sha256").hexdigest()
manifest = {"format": 1, "platform": "rpi5-arm64", "runtime": "raspios-trixie-arm64-v1",
            "version": version, "commit": commit, "archive": archive.name,
            "sha256": digest, "size": archive.stat().st_size}
archive.with_name(archive.name + ".update.json").write_text(json.dumps(manifest, indent=2) + "\n")
