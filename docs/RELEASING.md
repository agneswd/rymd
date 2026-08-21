# Releasing Rymd

Rymd updates itself from the GitHub Releases of `agneswd/rymd`. The release
workflow and the in-app updater share one naming scheme. If you change one,
change the other; `cargo test` fails when they disagree.

## Asset naming

`.github/workflows/release.yml` publishes exactly these files for a tag
such as `v0.2.0`:

| File | Purpose | Updater |
| --- | --- | --- |
| `rymd-v0.2.0-linux-x86_64.AppImage` | Self-contained Linux build | Replaces itself |
| `rymd-v0.2.0-linux-x86_64.tar.gz` | Portable Linux archive | Manual only |
| `rymd-v0.2.0-windows-x86_64.exe` | Portable Windows binary | Replaces itself |
| `rymd-v0.2.0-windows-x86_64-setup.exe` | Windows installer | Re-runs the installer |
| `SHA256SUMS` | SHA-256 of every artifact above | Required |

`SHA256SUMS` is not optional. The updater refuses to install an artifact
that has no digest in it.

The names come from `InstallKind::asset_name` in
`src/update/installer.rs`. That function is the only place in Rymd that
knows an asset name.

## Cutting a release

1. Set the new version in `Cargo.toml` and run `cargo build` so
   `Cargo.lock` follows.
2. Commit the bump.
3. Tag the commit with `v<version>`, using the same version as
   `Cargo.toml`.
4. Push the tag.

```sh
git tag v0.2.0
git push origin v0.2.0
```

The workflow refuses to release when the tag and the Cargo version
disagree. This is deliberate: a build that reports `0.1.0` under a `v0.2.0`
tag would offer its own version as an update forever.

Rymd never reads its version from source code. It comes from
`CARGO_PKG_VERSION`.

## What the workflow does

1. Compares the tag against the version Cargo reports, and stops if they
   differ.
2. Runs `cargo test --release` on Linux and on Windows.
3. Builds the release binaries.
4. Packages the AppImage, the tarball, the portable `.exe` and the
   installer.
5. Writes `SHA256SUMS` and verifies every installable artifact has an entry.
6. Uploads everything to the GitHub Release.

## Prereleases

Tag a prerelease with a semver prerelease, such as `v0.3.0-rc.1`, and mark
the GitHub Release as a prerelease. Stable Rymd builds ignore it. A build
that is itself a prerelease accepts both prereleases and stable releases.

Draft releases are always ignored.

## Adding signatures later

The updater verifies a SHA-256 digest today. Signing is a drop-in addition:

1. Sign `SHA256SUMS` in the workflow and upload the signature next to it.
2. Embed only the public key in Rymd.
3. Verify the signature in `src/update/github.rs::fetch_update`, before the
   digest is read from `SHA256SUMS`.

Keep the private key in GitHub Actions secrets. Never in the repository.
