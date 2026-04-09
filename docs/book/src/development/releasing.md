# Releasing Wren

Wren uses an automated release pipeline triggered by merging pull requests to `main`. This guide covers the release process, versioning scheme, and release artifacts.

## Automatic Release on PR Merge

The primary release workflow is **automatic on PR merge**:

1. PR is merged to `main`
2. `auto-release` workflow runs:
   - Bumps patch version (e.g., 0.1.2 → 0.1.3)
   - Generates CHANGELOG from commits since last release
   - Creates commit with updated Cargo.toml, Cargo.lock, and CHANGELOG.md
   - Creates and pushes git tag
3. `release` workflow runs:
   - Builds static binaries for Linux and macOS
   - Creates GitHub Release with binaries and changelog
   - Builds and pushes Docker image to GHCR

### Skip Automatic Release

Add the `skip-release` label to a PR to prevent the automatic release workflow from triggering on merge.

```bash
gh pr edit <PR_NUMBER> --add-label skip-release
```

This is useful for documentation or non-user-facing changes.

## Manual Release Workflow

For major or minor version bumps (not automatic patch bumps), use the manual release workflow:

1. Go to `.github/workflows/manual-release.yml`
2. Click "Run workflow"
3. Select the version bump type:
   - `patch` — Bug fixes (0.1.2 → 0.1.3)
   - `minor` — New features (0.1.0 → 0.2.0)
   - `major` — Breaking changes (0.1.0 → 1.0.0)

This will:
- Bump the version in Cargo.toml and Cargo.lock
- Generate CHANGELOG
- Create a release commit and tag
- Trigger the release workflow

You can also manually bump from the command line:

```bash
# Edit Cargo.toml to new version
vim Cargo.toml

# Update lockfile
cargo generate-lockfile

# Generate changelog
git cliff --config cliff.toml --output CHANGELOG.md

# Commit and tag
VERSION="0.2.0"
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): v${VERSION} [skip ci]"
git tag "v${VERSION}"
git push origin main --follow-tags
```

The `release` workflow will automatically run when the tag is pushed.

## Versioning Scheme

Wren follows **Semantic Versioning** (semver):

```
MAJOR.MINOR.PATCH
  |       |      |
  |       |      +-- Patch version (bug fixes, increment for auto-releases)
  |       +---------- Minor version (new features, backwards compatible)
  +----------------- Major version (breaking changes)
```

### Version Bumping Rules

| Change Type | Old Version | New Version | When |
|---|---|---|---|
| Bug fix | 0.1.2 | 0.1.3 | Automatic on PR merge |
| New feature | 0.1.0 | 0.2.0 | Manual workflow (minor bump) |
| Breaking API change | 0.2.0 | 1.0.0 | Manual workflow (major bump) |
| Pre-release | 0.2.0 | 0.2.0-rc.1 | Edit Cargo.toml manually |

**Note:** All releases are from `main` branch only. No maintenance branches for older versions.

## Release Artifacts

Each release produces the following artifacts:

### 1. GitHub Release

Located at `https://github.com/miguelgila/wren/releases/tag/vX.Y.Z`

Contains:
- **wren-controller-linux-x86_64** — Static Linux binary (glibc)
- **wren-controller-macos-aarch64** — Static macOS binary (Apple Silicon)
- **wren-controller-macos-x86_64** — Static macOS binary (Intel)
- **wren-cli-linux-x86_64** — CLI tool (Linux)
- **wren-cli-macos-aarch64** — CLI tool (macOS, Apple Silicon)
- **wren-cli-macos-x86_64** — CLI tool (macOS, Intel)
- **CHANGELOG.md** — Release notes

All binaries are fully static (no runtime dependencies) and ready to run.

### 2. Docker Image

Published to GitHub Container Registry (GHCR):

```bash
docker pull ghcr.io/miguelgila/wren-controller:0.1.3
docker pull ghcr.io/miguelgila/wren-controller:latest
```

Image contains:
- `wren-controller` binary (Kubernetes controller)
- `/metrics` Prometheus endpoint
- Health check endpoints: `/healthz`, `/readyz`

Image tags:
- `0.1.3` — Exact version
- `0.1` — Minor version (latest 0.1.x)
- `0` — Major version (latest 0.x.x)
- `latest` — Latest release

### 3. Helm Chart

Published to the same repository under `charts/wren/`:

```bash
# Add Wren Helm repo (not yet published to external repo)
# For now, use the repository directly:
helm repo add wren https://github.com/miguelgila/wren/raw/gh-pages/
helm install wren wren/wren --values my-values.yaml
```

Chart includes:
- WrenJob and WrenQueue CRD templates
- RBAC (ServiceAccount, ClusterRole, ClusterRoleBinding)
- Deployment (controller)
- Service (metrics endpoint)
- ServiceMonitor (Prometheus integration)
- ConfigMap (controller configuration)

Chart version matches app version (0.1.3).

### 4. Static Binaries

All binaries are **statically linked** — no runtime dependencies:

- No libc version requirements
- No OpenSSL dependency (uses rustls)
- No shared object dependencies (ldd shows "not a dynamic executable" on Linux)

This makes them safe to use in minimal container images or on air-gapped networks.

## Release Checklist

Before releasing (manually or after auto-release):

1. **Verify version matches**:
   ```bash
   grep '^version' Cargo.toml
   git describe --tags
   ```

2. **Verify CHANGELOG is accurate**:
   ```bash
   git show v0.1.3:CHANGELOG.md | head -50
   ```

3. **Verify GitHub Release was created**:
   - Visit `https://github.com/miguelgila/wren/releases`
   - Confirm binaries are uploaded
   - Confirm changelog is populated

4. **Verify Docker image is available**:
   ```bash
   docker pull ghcr.io/miguelgila/wren-controller:0.1.3
   docker image inspect ghcr.io/miguelgila/wren-controller:0.1.3
   ```

5. **Verify Helm chart lints**:
   ```bash
   helm lint charts/wren
   helm template wren charts/wren --values charts/wren/values.yaml
   ```

## Release Workflow Details

### CI Pipeline (`ci.yaml`)

Runs on every push to `main` and all PRs:
- Format check
- Lint + unit tests
- Security audit
- Code coverage
- Helm lint
- Docker build & push (to GHCR)

### Auto-Release Workflow (`auto-release.yml`)

Triggered when a PR is merged to `main` (unless labeled `skip-release`):

```yaml
jobs:
  auto-release:
    name: Bump Version and Release
    if: github.event.pull_request.merged == true && !contains(github.event.pull_request.labels.*.name, 'skip-release')
    steps:
      1. Check for release loop (avoid infinite commits)
      2. Bump patch version in Cargo.toml
      3. Update Cargo.lock
      4. Generate CHANGELOG with git-cliff
      5. Commit changes with "chore(release): vX.Y.Z [skip ci]"
      6. Create and push git tag "vX.Y.Z"
      7. Trigger release.yml workflow
```

Outputs: CHANGELOG.md, updated Cargo.toml/Cargo.lock, git tag

### Release Workflow (`release.yml`)

Triggered when a tag matching `v*.*.*` is pushed:

```yaml
jobs:
  validate-tag:
    # Verify tag version matches Cargo.toml version

  build-artifacts:
    # Build static binaries for:
    #   - linux-x86_64
    #   - macos-aarch64
    #   - macos-x86_64
    # Upload to GitHub Release

  docker:
    # Build and push controller image to GHCR
    # Tags: v0.1.3, 0.1.3, 0.1, 0, latest
```

## Post-Release Tasks

After a release is created:

1. **Announce in discussions** (if major feature or significant fix)
2. **Update main branch documentation** with latest version
3. **Test installation** with the new release:
   ```bash
   helm repo update
   helm install wren wren/wren --values values.yaml
   ```

## Troubleshooting

### Release workflow did not trigger

Check that the tag was pushed with commits:

```bash
git push origin main --follow-tags
```

The tag must be on the same commit as the version bump in Cargo.toml.

### Version mismatch between tag and Cargo.toml

The release workflow validates that the tag version matches Cargo.toml. If they don't match:

```bash
# Delete the bad tag locally and on remote
git tag -d v0.1.3
git push origin --delete v0.1.3

# Fix Cargo.toml to match the tag
vim Cargo.toml

# Or re-tag with the correct version
cargo generate-lockfile
git add Cargo.toml Cargo.lock
git commit -m "chore(release): fix version mismatch"
git tag v0.1.3
git push origin main --follow-tags
```

### Docker image not pushed

Check GHCR credentials in GitHub Actions settings. The workflow uses `${{ secrets.GITHUB_TOKEN }}` which should work automatically, but verify:

1. Repository Settings → Secrets and Variables → Actions
2. Ensure `GITHUB_TOKEN` is available (should be automatic)
3. Re-run the workflow if credentials were updated

### Static binary has runtime dependencies

If `ldd` shows shared object dependencies, the build used the system libc. To force static linking:

```bash
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-gnu
```

This is already configured in `release.yml`.

## Development Releases

For testing releases before pushing to `main`:

1. Create a feature branch and test locally:
   ```bash
   cargo build --release
   ./target/release/wren-controller --version
   ```

2. Create a GitHub release manually (pre-release):
   ```bash
   gh release create v0.1.3-rc.1 \
     --title "v0.1.3-rc.1 (Release Candidate)" \
     --notes "Testing this release candidate" \
     --prerelease
   ```

3. Test the pre-release version

4. If all good, delete the pre-release and merge to `main`:
   ```bash
   gh release delete v0.1.3-rc.1
   ```

The auto-release workflow will create the final release.

## References

- [Semantic Versioning](https://semver.org/) — Version numbering standard
- [Conventional Commits](https://www.conventionalcommits.org/) — Commit message format
- [git-cliff](https://github.com/orhun/git-cliff) — Changelog generator (used by auto-release)
- **[Contributing Guide](contributing.md)** — Commit message conventions
- **[Testing Guide](testing.md)** — How to verify releases locally

## Next Steps

- **[Contributing Guide](contributing.md)** — Development workflow
- **[Testing Guide](testing.md)** — Test suites and how to add tests
- **[Installation](../getting-started/installation.md)** — How users install released versions
