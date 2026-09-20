# Fork branch binaries

The `Build Fork Binaries` workflow builds this fork without publishing an npm package or creating a GitHub release. It produces exactly two native packages:

- macOS Apple Silicon: `aarch64-apple-darwin`
- Linux x64: `x86_64-unknown-linux-gnu`, with a glibc 2.28 compatibility floor

Each tarball contains `bin/agent-browser`, the complete `skills/` and `skill-data/` runtime trees, build metadata, and the project and axe-core license files.

The Linux package runs on native Ubuntu x64 and inside an Ubuntu x64 WSL2 distribution. It is a Linux executable, not a Windows `.exe`, so run it from the WSL2 shell and install it in the WSL2 filesystem.

## Run the branch workflow

A push to the exact `fix/goal-command-reliability` branch starts the workflow. A manual dispatch for another ref skips both matrix jobs and produces no artifacts. GitHub generally exposes a workflow in the web UI only after its workflow file exists on the repository's default branch, so do not rely on the **Run workflow** button while this file exists only on the fork branch.

The workflow needs only the automatically provided read-only repository token. It does not need API keys or application secrets.

## Download and verify

Open the successful workflow run in GitHub Actions and download the artifact for the host:

- `agent-browser-macos-arm64`
- `agent-browser-linux-x64`

A browser download is a GitHub ZIP envelope. Unzip it first to get the `.tar.gz` payload and `SHA256SUMS`. The GitHub CLI's artifact download command removes this outer envelope automatically.

On macOS:

```bash
unzip agent-browser-macos-arm64.zip -d agent-browser-macos-arm64
cd agent-browser-macos-arm64
shasum -a 256 -c SHA256SUMS
tar -xzf agent-browser-aarch64-apple-darwin.tar.gz
bundle=agent-browser-aarch64-apple-darwin
commit=$(sed -n 's/^commit=//p' "$bundle/BUILD-INFO.txt")
case "$commit" in ''|*[!0-9a-f]*) echo "Invalid build commit" >&2; exit 1;; esac
prefix="$HOME/.local/lib/agent-browser/$commit"
mkdir -p "$(dirname "$prefix")" "$HOME/.local/bin"
if [ ! -e "$prefix" ]; then mv "$bundle" "$prefix"; fi
test -x "$prefix/bin/agent-browser"
ln -sfn "$prefix/bin/agent-browser" "$HOME/.local/bin/agent-browser"
```

On native Ubuntu x64 or Ubuntu x64 in WSL2:

```bash
unzip agent-browser-linux-x64.zip -d agent-browser-linux-x64
cd agent-browser-linux-x64
sha256sum -c SHA256SUMS
tar -xzf agent-browser-x86_64-unknown-linux-gnu.tar.gz
bundle=agent-browser-x86_64-unknown-linux-gnu
commit=$(sed -n 's/^commit=//p' "$bundle/BUILD-INFO.txt")
case "$commit" in ''|*[!0-9a-f]*) echo "Invalid build commit" >&2; exit 1;; esac
prefix="$HOME/.local/lib/agent-browser/$commit"
mkdir -p "$(dirname "$prefix")" "$HOME/.local/bin"
if [ ! -e "$prefix" ]; then mv "$bundle" "$prefix"; fi
test -x "$prefix/bin/agent-browser"
ln -sfn "$prefix/bin/agent-browser" "$HOME/.local/bin/agent-browser"
```

Ensure `~/.local/bin` is on `PATH`, then install Chrome. Linux should include the required system packages:

```bash
agent-browser install --with-deps
```

On macOS, use `agent-browser install` without `--with-deps`. These fork binaries are not notarized. If macOS blocks a trusted download, use the usual **Open Anyway** control in **System Settings → Privacy & Security** after verifying its checksum.

Each macOS, native Ubuntu, or WSL2 environment has its own user configuration at `~/.agent-browser/config.json`. Create it separately on each host or WSL2 distribution and protect it with `chmod 600 ~/.agent-browser/config.json`. API secrets may be supplied through runtime environment variables or this protected user configuration. Never put secrets in the workflow, an artifact, or a committed `agent-browser.json`.

Do not use `agent-browser upgrade` to update these fork builds because that command follows the public npm, Homebrew, or Cargo channels. Download and install a newer workflow artifact instead.
