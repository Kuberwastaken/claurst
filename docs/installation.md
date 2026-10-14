# Claurst Installation Guide

Claurst is a Rust reimplementation of the Claude Code CLI. The fastest way
to install it is via the one-liner installers below. They drop the binary
into `~/.local/bin` (or `%LOCALAPPDATA%\Programs\claurst` on Windows; Git Bash
uses that same Windows default) and add that directory to your `PATH`
automatically.

---

## System Requirements

| Platform | Architecture | Minimum OS |
|----------|-------------|------------|
| Windows  | x86_64      | Windows 10 / Server 2019 |
| Linux    | x86_64      | glibc 2.17+ (most distros from 2014 onward) |
| Linux    | aarch64     | glibc 2.17+ (Raspberry Pi 4, AWS Graviton, etc.) |
| macOS    | x86_64      | macOS 11 Big Sur |
| macOS    | aarch64     | macOS 11 Big Sur (Apple Silicon: M1/M2/M3) |

There are no other runtime dependencies. The binary is statically linked where
possible; on Linux it links against the system glibc.

---

## Quick install (recommended)

### Linux / macOS

```bash
curl -fsSL https://github.com/Kuberwastaken/claurst/releases/latest/download/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://github.com/Kuberwastaken/claurst/releases/latest/download/install.ps1 | iex
```

### Windows (Git Bash / MSYS / Cygwin)

`install.sh` also works under Git Bash. It downloads the Windows zip
(`claurst-windows-x86_64.zip`), installs `claurst.exe` into the **same**
default directory as `install.ps1` (`%LOCALAPPDATA%\Programs\claurst`), and
updates the Windows user `PATH` (not only bashrc):

```bash
curl -fsSL https://github.com/Kuberwastaken/claurst/releases/latest/download/install.sh | bash
```

Both installers:

1. Detect your platform and architecture.
2. Download the matching archive from the latest GitHub release.
3. Verify it against the release's `SHA256SUMS` asset (skipped with a warning
   if the checksum file or a hashing tool is unavailable).
4. Extract `claurst` into `~/.local/bin/` on Linux/macOS (`$XDG_BIN_HOME` if
   set), or `%LOCALAPPDATA%\Programs\claurst` on Windows (`install.sh` under
   Git Bash uses this same Windows default).
5. Append that directory to your shell config (`.bashrc`, `.zshrc`,
   `.config/fish/config.fish`) on Unix, or to your Windows user `PATH`.
6. On macOS, strip the quarantine attribute so Gatekeeper does not block the
   unsigned binary.

Open a new terminal afterwards (or `source` the modified shell config) so
the updated `PATH` takes effect, then run `claurst --version` to verify.

### Installer flags

Both scripts accept the same flags:

| Flag (sh) | Flag (ps1) | Effect |
|---|---|---|
| `--version 0.1.0` (`-v`) | `-Version 0.1.0` | Install a specific version |
| `--binary <path>` (`-b`) | `-Binary <path>` | Install from a local file (skip download) |
| `--install-dir <path>` | `-InstallDir <path>` | Override the install directory |
| `--token <token>` | `-Token <token>` | GitHub token for API/downloads |
| `--no-modify-path` | `-NoModifyPath` | Don't touch shell config / user PATH |
| `--help` (`-h`) | `-Help` | Show usage |

Example: `curl -fsSL https://.../install.sh | bash -s -- --version 0.1.0`

### GitHub authentication

If GitHub rate-limits unauthenticated requests, or you install from a private
fork/release, set a token via env or flag. The installer also accepts
`GH_TOKEN` (GitHub CLI convention):

```bash
# Linux / macOS
export GITHUB_TOKEN=ghp_...   # or GH_TOKEN
curl -fsSL https://github.com/Kuberwastaken/claurst/releases/latest/download/install.sh | bash

# Or pass the token as a flag when running the script locally:
./install.sh --token ghp_...
```

```powershell
# Windows
$env:GITHUB_TOKEN = 'ghp_...'   # or $env:GH_TOKEN
irm https://github.com/Kuberwastaken/claurst/releases/latest/download/install.ps1 | iex

# Or:
.\install.ps1 -Token ghp_...
```

The token is sent as `Authorization: Bearer …` on the GitHub API call that
resolves the latest version and on asset/checksum downloads. A fine-grained
or classic PAT with `contents: read` (public repos need no scopes beyond
rate-limit relief) is enough.

---

## Via npm / bun

If you have Node.js or Bun installed, you can install Claurst as a global
package. The postinstall script automatically downloads the correct pre-built
native binary for your platform from GitHub Releases — no compilation needed.

```bash
# npm
npm install -g claurst

# bun
bun install -g claurst
```

After installation, run `claurst` directly from your terminal.

You can also run Claurst without a permanent install:

```bash
npx claurst          # via npm
bunx claurst         # via bun
```

**Supported platforms via npm:**

| Platform | Architecture |
|----------|-------------|
| Linux    | x86_64, aarch64 |
| macOS    | x86_64 (Intel), aarch64 (Apple Silicon) |
| Windows  | x86_64 |

---

## Upgrading

Once installed, upgrade in place at any time:

```bash
claurst upgrade               # to the latest release
claurst upgrade --version 0.1.0   # pin to a specific version
claurst upgrade --force       # reinstall the same version
```

The upgrade command downloads the matching archive from GitHub, extracts the
new binary, and replaces the running executable atomically. Settings in
`~/.claurst/` are preserved.

---

## Manual install from GitHub Releases

If you'd rather not run an install script, grab archives directly from
[**GitHub Releases**](https://github.com/Kuberwastaken/claurst/releases):

| Archive | Platform |
|---------|----------|
| `claurst-windows-x86_64.zip` | Windows 64-bit |
| `claurst-linux-x86_64.tar.gz` | Linux x86_64 |
| `claurst-linux-aarch64.tar.gz` | Linux ARM64 |
| `claurst-macos-x86_64.tar.gz` | macOS Intel |
| `claurst-macos-aarch64.tar.gz` | macOS Apple Silicon |

Every archive contains a single binary named `claurst` (or `claurst.exe`).
Extract it and put it somewhere on your `PATH`. For example on Linux:

```bash
curl -L https://github.com/Kuberwastaken/claurst/releases/latest/download/claurst-linux-x86_64.tar.gz \
  | tar -xz
chmod +x claurst
sudo mv claurst /usr/local/bin/
```

On macOS, also strip the quarantine flag so Gatekeeper allows the unsigned
binary:

```bash
xattr -rd com.apple.quarantine /usr/local/bin/claurst
```

On Windows, extract the zip and add the folder containing `claurst.exe`
to your user `PATH` via **Settings → System → Advanced system settings →
Environment Variables**.

### User-local install without sudo

```bash
mkdir -p ~/.local/bin
mv claurst ~/.local/bin/claurst
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

For Zsh users, substitute `.zshrc` for `.bashrc`.

---

## Verifying the Installation

```bash
claurst --version
```

A successful installation prints the version string, for example:

```
claurst 0.1.8
```

To confirm the binary is the one you installed:

```bash
which claurst          # Linux / macOS
where claurst          # Windows (Command Prompt)
```

---

## Building from Source

Building from source requires a recent stable Rust toolchain (1.82 or later;
the code uses `Option::is_none_or`, and CI builds with the current stable).
Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Option A: Install via Cargo

Claurst is not published on crates.io. Install from the Git repository:

```bash
cargo install --git https://github.com/Kuberwastaken/claurst --locked claurst
```

This clones, compiles, and installs the binary to `~/.cargo/bin/claurst`.
That directory is added to `PATH` automatically by `rustup`.

### Option B: Clone and Build

```bash
git clone https://github.com/Kuberwastaken/claurst.git
cd claurst/src-rust

# Debug build (fast to compile, larger binary, extra runtime checks)
cargo build --package claurst

# Release build (optimised, smaller, suitable for everyday use)
cargo build --release --package claurst
```

The release binary is placed at:

```
src-rust/target/release/claurst        # Linux / macOS
src-rust/target\release\claurst.exe   # Windows
```

Copy it to a directory on your `PATH` as described above.

### Linux system dependencies

On Linux, the default build needs ALSA development headers (for the `voice`
feature, on by default) and `pkg-config`. OpenSSL is not needed: TLS uses
rustls and a bundled BoringSSL, which needs a C/C++ compiler and CMake. The
release workflow installs only `libasound2-dev pkg-config` on Ubuntu.

```bash
# Debian / Ubuntu
sudo apt-get install -y libasound2-dev pkg-config

# Fedora / RHEL
sudo dnf install -y alsa-lib-devel pkgconf-pkg-config

# Arch
sudo pacman -S alsa-lib pkgconf
```

To skip ALSA, build without default features:
`cargo build --release --package claurst --no-default-features`.

### Optional cargo features

| Feature | Description |
|---------|-------------|
| `voice` | Microphone input / voice prompting (enabled by default) |
| `claurst-tools/computer-use` | Screenshot capture and mouse/keyboard control |
| `claurst-core/dev_full` | All experimental core feature flags combined |

The `claurst` package itself defines only `voice`; the others are features of
workspace crates and are enabled with the `crate/feature` syntax from
`src-rust/`:

```bash
cargo build --release --package claurst --features claurst-tools/computer-use
cargo build --release --package claurst --features claurst-core/dev_full
```

### Cross-compiling for Linux aarch64

The release workflow uses [cross](https://github.com/cross-rs/cross) for
aarch64 Linux builds. To reproduce it locally:

```bash
cargo install cross --git https://github.com/cross-rs/cross
cd src-rust
cat > Cross.toml <<'EOF'
[target.aarch64-unknown-linux-gnu]
pre-build = [
  "dpkg --add-architecture $CROSS_DEB_ARCH",
  "apt-get update",
  "apt-get install -y pkg-config libasound2-dev:$CROSS_DEB_ARCH"
]
EOF
cross build --release --locked --package claurst --target aarch64-unknown-linux-gnu
```

`cross` provides the Docker sysroot; the `Cross.toml` pre-build step (the same
one the release workflow writes) installs the arm64 ALSA headers.

---

## Shell Completions

Claurst does not currently ship a dedicated `completions` subcommand. All
flags can be discovered via `claurst --help`. In zsh you can use the generic
`--help` parser:

```bash
# zsh — add to ~/.zshrc (requires compinit)
compdef _gnu_generic claurst
```

Do not use bash's `complete -C claurst claurst`: it runs `claurst` itself as
the completer, which starts a session.

Richer completion scripts may be added in a future release.

---

## Upgrading a source install

```bash
cargo install --git https://github.com/Kuberwastaken/claurst --locked --force claurst
```

For binary installs (the recommended path), use `claurst upgrade` — see
the [Upgrading](#upgrading) section above.

---

## Uninstalling

If you used the install script, remove the binary (or install directory):

```bash
rm -f ~/.local/bin/claurst               # Linux / macOS
# Windows (PowerShell / default install.ps1 path):
Remove-Item -Force "$env:LOCALAPPDATA\Programs\claurst\claurst.exe"
# Optional: remove the empty install dir
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\claurst" -ErrorAction SilentlyContinue
```

For manual installs:

```bash
sudo rm /usr/local/bin/claurst           # if installed system-wide
rm ~/.local/bin/claurst                  # if installed user-local
```

To also remove all settings and session data:

```bash
rm -rf ~/.claurst
```

You may also want to remove the `# claurst` PATH line that the installer
appended to your shell config (`.bashrc`, `.zshrc`, etc.), or the Windows
user PATH entry for `%LOCALAPPDATA%\Programs\claurst`.
