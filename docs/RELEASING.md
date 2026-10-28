# Releasing

## Why the tag, not main

crates.io versions are immutable. `0.1.0` can be published once, then only yanked — never
replaced. A workflow that publishes on every push to main therefore has two possible outcomes:

- every push after the first fails with "crate version already uploaded", or
- every commit invents a new version on the registry, most of them not worth a version number.

So the tag is the version boundary, and the pipeline publishes when the tag moves.

What main still does on every push: `cargo package --locked --no-verify`. That catches a
package that cannot be built (missing file, bad manifest) at the commit, not at the tag, where
a failed publish leaves a version half-released.

## One-time setup: crates.io trusted publisher

No long-lived token is stored in this repository. The workflow exchanges a GitHub OIDC token
for a 30-minute crates.io credential.

On https://crates.io/crates/riggate/settings → Trusted Publishing, add:

| Field | Value |
|---|---|
| Repository | `prashant-anand/riggate` |
| Workflow filename | `release.yml` |
| Environment | `crates-io` (optional; matching the job's `environment:`) |

If you would rather use a classic API token, set `gh secret set CARGO_REGISTRY_TOKEN` and
replace the `rust-lang/crates-io-auth-action` step with:

```yaml
      - name: cargo publish
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
        run: cargo publish --locked
```

## Cut a release

```bash
# 1. version bump, committed and green in CI
sed -i '' 's/^version = "0.1.0"/version = "0.1.1"/' Cargo.toml
git commit -am "release 0.1.1"

# 2. tag the commit CI just verified
git tag -a v0.1.1 -m "riggate 0.1.1"
git push origin main v0.1.1
```

The `release` workflow then runs three jobs:

| Job | What it does |
|---|---|
| `build` | compiles both targets, writes `.sha256` for each |
| `publish-crate` | checks the tag equals the crate version, skips if already published, publishes |
| `publish` | creates the GitHub release with both binaries and checksums |

`publish-crate` and `publish` are independent: a crates.io misconfiguration does not block the
binaries, and a failed binary build does not leave the crate unpublished.

## Guards

- Tag must equal the `Cargo.toml` version, or the job fails before publishing anything.
- An already-published version is skipped, so re-running a tag is safe.
- `workflow_dispatch` runs the build matrix without publishing, for testing a toolchain change.

## Versions on every commit, if you ever want them

Pre-release versions (`0.1.1-dev.<commit count>`) can be published automatically. `cargo
install riggate` ignores pre-releases by default, so nobody would receive them. Not worth it.
