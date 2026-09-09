# Automated release builds with GitHub Actions

This is a walkthrough for setting up release builds yourself: what each piece does and
why, then a workflow you can paste in. Nothing here is wired up yet; the CI workflow in
`.github/workflows/ci.yml` only tests.

## What you are building

A workflow that, when you push a version tag such as `v0.2.0`:

1. builds `terratofu-gui` and `ttg` in release mode on Windows, macOS and Linux,
2. packs each platform's binaries into an archive (`.zip` on Windows, `.tar.gz` elsewhere),
3. creates a GitHub Release for the tag and attaches the archives, so people download
   them from the Releases page without a Rust toolchain.

Tags are the trigger because they are the one thing that says "this commit is a
release". Pushing to `master` keeps running the normal CI; nothing is published until
you tag.

## Concepts you will meet

- **Workflow / job / step.** A workflow is one YAML file under `.github/workflows/`. It
  has jobs (each on its own runner VM); a job has steps (shell commands or reusable
  *actions*). Jobs run in parallel unless one `needs` another.
- **Matrix.** One job definition expanded over several combinations, here one per
  operating system. Each expansion is a separate VM.
- **Artifacts.** Files a job hands to a later job in the same run (`upload-artifact` /
  `download-artifact`). Used to move the three platform archives to the job that
  creates the release.
- **`GITHUB_TOKEN`.** A token GitHub creates for every run. To let it create a release
  the workflow needs `permissions: contents: write`; the default is read-only.
- **Runner images.** `windows-latest`, `macos-latest` (Apple Silicon), `ubuntu-latest`.
  Linux needs the GTK/X11 development packages for egui, exactly as `ci.yml` installs.
- **Caching.** `Swatinem/rust-cache` keeps the `target` directory between runs keyed on
  `Cargo.lock`, which turns a 15-minute release build into a few minutes when only
  source changed.

## Step by step

### 1. Decide the version scheme

Use the workspace version in `Cargo.toml` (`[workspace.package] version`) and tag the
matching commit `v<version>`. Bump the version in a normal commit first, then tag:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The workflow below refuses nothing if they disagree, but keeping them in sync makes the
`--version` output match the download.

### 2. Create the workflow file

Create `.github/workflows/release.yml` with the contents in the next section. Commit and
push it to `master` first (workflows must exist on the default branch before a tag can
trigger them).

### 3. Push a tag and watch the run

Go to the repository's **Actions** tab: you will see one `Release` run with three build
jobs and one `publish` job. Each build job takes 5 to 15 minutes cold. When `publish`
finishes, the **Releases** page has a draft-free release named after the tag with the
archives attached.

### 4. If something fails

- *Linux build fails in `eframe`/`glutin`:* the apt packages step is missing or
  incomplete. `libgtk-3-dev libxkbcommon-dev libssl-dev` is what CI uses today.
- *`publish` says "Resource not accessible by integration":* the `permissions` block is
  missing or the repository settings restrict workflow permissions (Settings ▸ Actions ▸
  General ▸ Workflow permissions ▸ "Read and write").
- *Archives are empty:* the `strip` or copy step ran in the wrong directory; check the
  `ls target/release` output the workflow prints.

## The workflow

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write   # let GITHUB_TOKEN create the release and upload assets

env:
  CARGO_TERM_COLOR: always

jobs:
  build:
    name: Build (${{ matrix.name }})
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - name: windows-x86_64
            os: windows-latest
            archive: zip
          - name: macos-arm64
            os: macos-latest
            archive: tar.gz
          - name: linux-x86_64
            os: ubuntu-latest
            archive: tar.gz
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - name: Install GUI build dependencies (Linux)
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libssl-dev

      - name: Build release binaries
        run: cargo build --release -p ttg-app -p ttg-cli

      # Collect the two binaries plus the docs people need offline into one folder.
      - name: Stage files
        shell: bash
        run: |
          set -eux
          ls target/release
          stage="terratofu-gui-${GITHUB_REF_NAME}-${{ matrix.name }}"
          mkdir -p "$stage"
          if [ "${{ runner.os }}" = "Windows" ]; then
            cp target/release/terratofu-gui.exe target/release/ttg.exe "$stage/"
          else
            cp target/release/terratofu-gui target/release/ttg "$stage/"
            strip "$stage/terratofu-gui" "$stage/ttg" || true
          fi
          cp README.md LICENSE* "$stage/" 2>/dev/null || cp README.md "$stage/"
          cp -r examples "$stage/examples"
          echo "STAGE=$stage" >> "$GITHUB_ENV"

      - name: Archive (zip)
        if: matrix.archive == 'zip'
        shell: pwsh
        run: Compress-Archive -Path "$env:STAGE" -DestinationPath "$env:STAGE.zip"

      - name: Archive (tar.gz)
        if: matrix.archive == 'tar.gz'
        run: tar -czf "$STAGE.tar.gz" "$STAGE"

      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.name }}
          path: |
            ${{ env.STAGE }}.zip
            ${{ env.STAGE }}.tar.gz
          if-no-files-found: error

  publish:
    name: Create GitHub Release
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: dist
          merge-multiple: true

      - name: List assets
        run: ls -la dist

      - name: Create release and upload assets
        uses: softprops/action-gh-release@v2
        with:
          files: dist/*
          generate_release_notes: true   # commits since the previous tag become the notes
```

## Things you may want later

- **Signed / notarised macOS builds.** Unsigned binaries trigger Gatekeeper warnings.
  Notarisation needs an Apple Developer account and a couple of secrets
  (`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`); `apple-actions/import-codesign-certs`
  plus `xcrun notarytool` in the macOS job is the usual route.
- **Windows code signing.** Same idea with a certificate in a secret and `signtool`.
- **Intel macOS.** Add a matrix entry with `os: macos-13` (the last Intel image) and
  `name: macos-x86_64`.
- **Version check.** A step that fails when `Cargo.toml`'s version and the tag differ:
  ```bash
  v=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2); [ "v$v" = "$GITHUB_REF_NAME" ]
  ```
- **Checksums.** `sha256sum dist/* > dist/SHA256SUMS` in the publish job before the
  release step, so downloads can be verified.
- **Pre-releases.** Tag `v0.3.0-rc1` and add `prerelease: ${{ contains(github.ref_name, '-') }}`
  to the release step so release candidates are marked as such.
