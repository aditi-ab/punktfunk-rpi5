# `punktfunk-gamescope` — nixpkgs' gamescope carrying punktfunk's `pipewire-hdr` patches, exposed
# under its own name so it sits BESIDE the system gamescope instead of replacing it.
#
# An override rather than a from-scratch derivation on purpose: gamescope vendors wlroots,
# vkroots, libliftoff, libdisplay-info, SPIRV-Headers and reshade as git submodules plus two meson
# wraps, and nixpkgs already solves all of that. What we add is two patches and a rename.
#
# `gamescope` in nixpkgs is a wrapper (it wires the WSI layer + capabilities); the buildable
# derivation is `gamescope.unwrapped` — patching the wrapper would be a no-op, so this asserts on
# it rather than silently shipping an unpatched binary.
#
# Version drift: the patches are applied to whatever gamescope your nixpkgs pins, NOT to the
# commit `packaging/gamescope/build-punktfunk-gamescope.sh` names. Both hunks sit in code that has
# been stable across the 3.16 series (`src/pipewire.cpp`'s format builders, `paint_pipewire()` in
# `src/steamcompmgr.cpp`), so this normally just works — and when it does not, the build fails
# loudly at `patchPhase` rather than producing a gamescope that quietly cannot do HDR.
{
  lib,
  gamescope,
  patchDir,
}:
let
  unwrapped =
    gamescope.unwrapped or (throw ''
      punktfunk-gamescope needs `gamescope.unwrapped` (the buildable derivation behind nixpkgs'
      gamescope wrapper) and this nixpkgs does not expose it. Update nixpkgs, or build the
      compositor with packaging/gamescope/build-punktfunk-gamescope.sh instead.
    '');
in
unwrapped.overrideAttrs (old: {
  pname = "punktfunk-gamescope";

  patches = (old.patches or [ ]) ++ [
    "${patchDir}/0001-pipewire-offer-10-bit-BT.2020-PQ-capture-formats-HDR.patch"
    "${patchDir}/0002-punktfunk-stamp-the-version-banner-with-pfhdr1.patch"
  ];

  # nixpkgs builds from a `fetchFromGitHub` src, so there is no `.git` for `git describe` and the
  # banner would read `+pfhdr1 (gcc …)` with no version at all — which the host's diagnostic
  # version gate then misreads (it takes the first X.Y.Z triple it finds, i.e. the compiler's).
  # Substituting the real version in keeps `--version` honest AND keeps our marker.
  postPatch = (old.postPatch or "") + ''
    substituteInPlace src/meson.build \
      --replace-fail \
        "vcs_tag = run_command(vcs_tag_cmd, check: false).stdout().strip()" \
        "vcs_tag = '${old.version}'"
  '';

  # Ship ONLY the compositor, renamed. Everything else nixpkgs installs (gamescopectl,
  # gamescopereaper, gamescopestream, the WSI layer, .desktop files) belongs to the real gamescope
  # package — duplicating it here would put two of each on PATH. The host only execs the
  # compositor.
  postInstall = (old.postInstall or "") + ''
    find $out -mindepth 1 -maxdepth 1 ! -name bin -exec rm -rf {} +
    find $out/bin -mindepth 1 ! -name gamescope -delete
    mv $out/bin/gamescope $out/bin/punktfunk-gamescope
  '';

  # `gamescope --version` exits non-zero on some builds; the grep is the real assertion.
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    $out/bin/punktfunk-gamescope --version 2>&1 | grep -q '+pfhdr' \
      || { echo "punktfunk-gamescope: the +pfhdr marker is missing — the patches did not take"; exit 1; }
    runHook postInstallCheck
  '';

  meta = (old.meta or { }) // {
    description = "gamescope with 10-bit BT.2020/PQ PipeWire capture, for punktfunk HDR streaming";
    mainProgram = "punktfunk-gamescope";
  };
})
