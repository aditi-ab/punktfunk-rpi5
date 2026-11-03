# Patch maintenance

This branch contains downstream patch snapshots, not upstream application source.
Never merge it into the source branch. Keep the checkout separate.

- Capture the full delta against an explicitly selected upstream release.
- Export only committed source state; never edit generated patches manually.
- Keep old snapshots immutable. Regenerate a new snapshot after resolving a port.
- Verify exact Git tree equality, including binaries and executable modes.
- Run tool tests and replay checks before pushing.
- Push the source commit and patch snapshot before tagging a release.
- Do not claim that clean application proves compatibility or physical performance.
