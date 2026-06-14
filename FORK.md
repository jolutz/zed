# Zed Audio Fork

This repository is Jon's personal fork of Zed for a Kubuntu/Linux-focused custom
build with an MVP audio file viewer. The long-lived working branch is
`audio-file-viewer` in `https://github.com/jolutz/zed.git`.

## Repository Policy

- Treat `origin` as `https://github.com/jolutz/zed.git`.
- Treat `upstream` as `https://github.com/zed-industries/zed.git`.
- Update `audio-file-viewer` from the latest non-prerelease upstream Zed release
  tag, not from upstream `main`.
- Prefer merge-based updates over rebasing so weekly maintenance remains simple.
- Push successful maintenance changes back to `origin/audio-file-viewer`.
- Keep the custom build Linux-only unless Jon explicitly asks otherwise.

## Audio Viewer Scope

The custom feature is an MVP audio file viewer. It should open common audio files
from the project panel/file opener instead of showing them as text.

Supported formats are intentionally conservative: `wav`, `mp3`, `flac`, and
`ogg`. Avoid broader codec support unless it is straightforward and low risk.

Important implementation locations:

- UI/player crate: `crates/audio_viewer`
- Project-side local/remote loading: `crates/project/src/audio_store.rs`
- Zed integration points: root `Cargo.toml`, `crates/zed/Cargo.toml`, and Zed
  app initialization in `crates/zed/src/`
- Remote protocol and server routing: `crates/proto`, `crates/collab`, and
  `crates/remote_server`

Remote SSH behavior matters: audio bytes may be read on the remote host, but
playback should happen on the local Zed client. Model this after the image
viewer's local/remote store flow.

## Build And Distribution

GitHub Actions should do the heavy bundle build. The local machine should usually
run focused checks only.

Custom distribution path:

- Workflow: `.github/workflows/build_audio_zed_linux.yml`
- Release tag: `audio-zed-linux-latest`
- Release asset: `zed-linux-x86_64.tar.gz`
- Installer/update script: `script/install-audio-zed-linux`
- Local launcher installed by that script: `~/.local/bin/zed-audio`

The installer script must stay executable in Git (`100755`). If a user has to run
`chmod +x script/install-audio-zed-linux`, fix the Git mode with:

```sh
git update-index --chmod=+x script/install-audio-zed-linux
```

## Verification

For ordinary fork maintenance, prefer lightweight local checks before pushing:

```sh
cargo check -p audio_viewer
cargo check -p project
rustfmt --check <touched-rust-files>
git diff --check
```

After pushing, verify the Linux workflow and release asset when the change could
affect CI, distribution, or installation:

```sh
gh run list --repo jolutz/zed --branch audio-file-viewer --limit 5
gh release view audio-zed-linux-latest --repo jolutz/zed
```

## Automation

There is a weekly Codex automation named `update-zed-audio-fork`. It updates this
branch from the latest upstream release tag, runs lightweight checks, pushes on
success, and lets GitHub Actions publish the Linux bundle.
