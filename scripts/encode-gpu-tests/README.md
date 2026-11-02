# GPU test pass sets
One file per box (`nvidia`, `amd`, `intel`): the result line of every ignored pf-encode test on that box.
The files do not exist yet. The maintainer runs `sh scripts/encode-gpu-tests.sh <box>` on each box, which records its file on the first run, and commits it.
A later run exits 1 only for a test the file lists as `ok` that fails or does not report. After a rename or a removal, delete that line, or the file to record it again.
