# Releasing MikMik

Two artefacts ship separately: the GitHub release (binaries) and the VS Code extension. Only the first is automated today.

## Order

1. Cut the GitHub release, which stamps the version itself. The installers read its assets.
2. Publish the VS Code extension. Independent of the release.

## Version stamping

`--release` stamps the version as part of cutting a release, so the script below is only needed when stamping outside that flow. It fails loudly if an expected pattern is missing, which also makes it a cheap check that no surface drifted after a rename or a restructure: run it with the current version and it rewrites nothing.

```bash
python scripts/bump-version.py vX.Y.Z
```

Versioning is forward-only; the release workflow refuses a tag less than or equal to the highest existing tag. Never edit `src-rust/Cargo.lock` by hand. The human-readable `CHANGELOG.md` and the release body are owned by the project-scoped `version-update` skill, not by this flow.

## 1. GitHub release

Triggered by a marker in the head commit message, handled by `.github/workflows/auto-release.yml`:

- `--release vX.Y.Z` cuts a new release. The tag must be strictly greater than the highest existing one. The workflow stamps the version itself, commits the bump as `github-actions[bot]` with `[skip ci]`, then dispatches `release.yml`. Running `scripts/bump-version.py` by hand first is therefore optional for this path.
- `--patch` patches the currently shipped release in place, reading the version from `src-rust/Cargo.toml`. Restricted to the `KilimcininKorOglu` actor, because a patch force-moves a published tag.

`release.yml` builds five targets and publishes archives named `mikmik-<os>-<arch>`. `install.sh` and `install.ps1` read exactly these names, so a mismatch breaks the one-line installer rather than failing loudly.

Nothing else is needed: the workflow runs under `GITHUB_TOKEN`.

## 2. VS Code extension

Extension id: `kilimcininkoroglu.mikmik-vscode`. Source in `editors/vscode/`.

### State as measured

- The publisher `kilimcininkoroglu` does **not** exist on the Marketplace (`marketplace.visualstudio.com/publishers/kilimcininkoroglu` returns 404). It has to be created before anything can be published.
- Nothing is published under that publisher, so there is no old extension id to deprecate.
- The publisher does not exist on Open VSX either. Publishing there is optional and separate.
- There is no CI workflow for the extension; publishing is manual.

`src-rust/crates/core/src/ide.rs` prints `code --install-extension kilimcininkoroglu.mikmik-vscode` to users, so the published id has to match that string exactly.

### Create the publisher (once)

1. Sign in at <https://marketplace.visualstudio.com/manage> with the Microsoft account that owns the extension.
2. Create a publisher with the id `kilimcininkoroglu`. The id is permanent and cannot be renamed; the display name can change.

### Get a Personal Access Token (once)

From Azure DevOps (<https://dev.azure.com>), under user settings, Personal Access Tokens:

- Organization: **All accessible organizations**. A token scoped to a single organization is rejected.
- Scope: **Marketplace → Manage**.

Then verify it:

```bash
cd editors/vscode
npx vsce login kilimcininkoroglu
```

### Publish

```bash
cd editors/vscode
npm ci
npm run check          # tsc over the extension, the webview and the tests
npm test
npx vsce publish --no-dependencies
```

`vscode:prepublish` runs the type check and a production esbuild, so `vsce publish` builds what it ships. `--no-dependencies` matches the existing `npm run package` script: the extension bundles through esbuild and must not ship `node_modules`.

To inspect the artefact before publishing:

```bash
npm run package        # writes mikmik-vscode-X.Y.Z.vsix
```

### Optional: publish from CI without a token

`vsce` supports OIDC, which removes the stored PAT:

```yaml
permissions:
  contents: read
  id-token: write
steps:
  - run: npx @vscode/vsce publish --oidc
```

This needs a trusted publishing policy configured on the Marketplace for the publisher. Worth doing only once the extension ships regularly.

## What is not automated

- Everything about the VS Code extension.
- Open VSX, which is not set up at all.
