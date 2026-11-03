# Raspberry Pi downstream patch archive

This is the `codex/rpi5-patches` maintenance branch of `aditi-ab/punktfunk-rpi5`.
It contains patches and maintenance tools, not a Punktfunk source checkout.
Keep this branch in its own directory. **Do not merge it into a source branch.**
Original upstream release tags stay unchanged. Applied source changes live on
the Pi fork's source branch; this archive is their separately stored replay form.

## Contents

`CURRENT` selects the latest snapshot. `series/<full-source-commit>/` is immutable
and contains a manifest, an ordered `series` file and five binary-safe patches:

1. Decoder, pixel transfer, dependencies and frame recovery.
2. Wayland pacing, Vulkan presentation and overlay damage.
3. Console rendering, transitions and UI controls.
4. Bluetooth, SSH and system-service integration.
5. Kiosk packaging, Weston, documentation and build/release integration.

The groups partition the complete upstream-to-fork tree difference. Apply the
whole series in order; individual groups are not independent buildable features.
The manifest lists every affected path and pins both commits, the target Git tree
and each patch's SHA256. Binary files, additions, deletions and executable modes
are included. The Weston source patch and build recipe are preserved alongside
its binary patch. Existing license notices remain part of their respective files.

## Capture every source change

Set `source_checkout` to the Pi source checkout and `upstream_tag` to its explicit
upstream release. Commit source changes first, then run in this patch checkout:

```sh
python tools/patchset.py export --source "$source_checkout" --base "$upstream_tag"
python tools/patchset.py verify --source "$source_checkout" --expect-head
```

Verification applies all patches to a temporary Git index and compares the result
with the exact committed source tree. It does not modify the source working tree
or its normal index. New snapshots are verified before `CURRENT` changes.
Existing snapshots are never overwritten.

Push the source commit and the patch snapshot before creating a release tag.
The source release workflow checks out this branch and refuses to build a source
commit without its matching, verified snapshot. Earlier release snapshots remain
available even after `CURRENT` advances. No source merge is needed to update this
archive. Changes left uncommitted in a working directory are not archived.

## Reapply or port

Start with a clean, separate checkout of the upstream release:

```sh
python /path/to/patch-checkout/tools/patchset.py apply --source "$clean_checkout"
```

This selects `CURRENT`, checks the exact upstream commit, preflights the complete
series, then applies and stages it without committing. Use `--snapshot <commit>`
to select an older source snapshot.

For a newer upstream release, add `--allow-new-base`. This only permits a different
base; it does not ignore conflicts. If any patch fails its applicability check,
the tool stops before changing files. Resolve the port in a disposable source
branch, keeping upstream fixes where they supersede ours. `git apply --3way` can
assist manual work when the old blobs are available, but may leave conflicts.
Never use an unconditional file overlay to hide these conflicts.

After review, run the Pi-feature ARM build/tests, image verification and physical
checks: streaming cadence, actual menu page transitions, controller input,
Bluetooth, SSH, system information and Weston stability. A clean patch application
does not prove compatibility or performance. Commit the port, export a new snapshot
with its new upstream base, and push both branches before tagging.

## Verify this branch

```sh
python -m unittest discover -s tests -v
python tools/patchset.py verify --source "$source_checkout" --expect-head
```

CI tests the tools and reconstructs every archived snapshot from the public source
commits. See the source branch's `docs/rpi5-patch-rationale.md` for patch rationale.
Tooling is MIT OR Apache-2.0; patch contents retain their original licenses.
