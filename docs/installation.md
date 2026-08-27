# MikMik Installation Guide

MikMik is a Rust reimplementation of the Claude Code CLI. The fastest way
to install it is via the one-liner installers below. They drop the binary
into `~/.local/bin` (or `%LOCALAPPDATA%\Programs\mikmik` on Windows; Git Bash
uses that same Windows location) and add that directory to your `PATH`
automatically.

---

## System Requirements

| Platform | Architecture | Minimum OS                                       |
|----------|--------------|--------------------------------------------------|
| Windows  | x86_64       | Windows 10 / Server 2019                         |
| Linux    | x86_64       | glibc 2.17+ (most distros from 2014 onward)      |
| Linux    | aarch64      | glibc 2.17+ (Raspberry Pi 4, AWS Graviton, etc.) |
| macOS    | x86_64       | macOS 11 Big Sur                                 |
| macOS    | aarch64      | macOS 11 Big Sur (Apple Silicon)                 |

There are no other runtime dependencies. The binary is statically linked where
possible; on Linux it links against the system glibc.

Every Apple Silicon Mac is `aarch64`, whichever chip it carries: M1 through M5,
and the A-series parts used in the MacBook Neo. They all take the same
`mikmik-macos-aarch64` archive.

---

## Quick install (recommended)

### Linux / macOS

```bash
curl -fsSL https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.ps1 | iex
```

### Windows (Git Bash / MSYS / Cygwin)

`install.sh` also runs under Git Bash. It downloads the Windows archive
(`mikmik-windows-x86_64.zip`), installs `mikmik.exe` into the **same**
directory `install.ps1` uses (`%LOCALAPPDATA%\Programs\mikmik`), and updates
the Windows user `PATH` rather than a shell config file, so the binary is on
`PATH` in PowerShell and cmd too:

```bash
curl -fsSL https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.sh | bash
```

Extraction uses `unzip` when it is installed and falls back to `tar`, which
Git for Windows ships and which reads a zip.

Both installers:

1. Detect your platform and architecture.
2. Download the matching archive from the latest GitHub release.
3. Extract `mikmik` into `~/.local/bin/` on Linux and macOS, or
   `%LOCALAPPDATA%\Programs\mikmik` on Windows (`install.sh` under Git Bash
   uses that same Windows location).
4. Append that directory to your shell config (`.bashrc`, `.zshrc`,
   `.config/fish/config.fish`) on Unix, or to your Windows user `PATH`.
5. On macOS, strip the quarantine attribute so Gatekeeper does not block the
   unsigned binary.

Open a new terminal afterwards (or `source` the modified shell config) so
the updated `PATH` takes effect, then run `mikmik --version` to verify.

### Installer flags

Both scripts accept the same flags:

| Flag (sh)              | Flag (ps1)           | Effect                                    |
|------------------------|----------------------|-------------------------------------------|
| `--version 0.1.0`      | `-Version 0.1.0`     | Install a specific version                |
| `--binary <path>`      | `-Binary <path>`     | Install from a local file (skip download) |
| `--install-dir <path>` | `-InstallDir <path>` | Override the install directory            |
| `--token <token>`      | `-Token <token>`     | GitHub token for the API and downloads    |
| `--no-modify-path`     | `-NoModifyPath`      | Don't touch shell config / user PATH      |
| `--help`               | `-Help`              | Show usage                                |

Example: `curl -fsSL https://.../install.sh | bash -s -- --version 0.1.0`

### GitHub authentication

GitHub rate-limits unauthenticated API requests per IP address, which is easy
to hit behind a shared network or in CI. Pass a token to lift the limit, and to
install from a private fork. Both installers also read `GITHUB_TOKEN` and
`GH_TOKEN` (the GitHub CLI's variable):

```bash
# Linux / macOS / Git Bash
export GITHUB_TOKEN=ghp_...
curl -fsSL https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.sh | bash

# Or, running the script locally:
./install.sh --token ghp_...
```

```powershell
# Windows
$env:GITHUB_TOKEN = 'ghp_...'
irm https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/install.ps1 | iex

# Or:
.\install.ps1 -Token ghp_...
```

A classic or fine-grained token with `contents: read` is enough; for a public
repository no scope at all is needed, since the token only lifts the rate
limit.

The token is sent as `Authorization: Bearer …` to `github.com` and
`api.github.com` only. A release download redirects to a separate storage host,
and neither installer sends the token there: `install.sh` relies on curl, which
drops the header across hosts, and `install.ps1` follows the redirect itself
and rebuilds the headers per hop. `install.sh` also hands the header to curl on
stdin rather than as an argument, so the token does not appear in the process
list.

---

## Via npm / bun

If you have Node.js or Bun installed, you can install MikMik as a global
package. The postinstall script automatically downloads the correct pre-built
native binary for your platform from GitHub Releases — no compilation needed.

```bash
# npm
npm install -g mikmik

# bun
bun install -g mikmik
```

After installation, run `mikmik` directly from your terminal.

You can also run MikMik without a permanent install:

```bash
npx mikmik          # via npm
bunx mikmik         # via bun
```

**Supported platforms via npm:**

| Platform | Architecture                                |
|----------|---------------------------------------------|
| Linux    | x86_64, aarch64                             |
| macOS    | x86_64 (Intel), aarch64 (all Apple Silicon) |
| Windows  | x86_64                                      |

---

## Upgrading

Once installed, upgrade in place at any time:

```bash
mikmik upgrade               # to the latest release
mikmik upgrade --version 0.1.0   # pin to a specific version
mikmik upgrade --force       # reinstall the same version
```

The upgrade command downloads the matching archive from GitHub, extracts the
new binary, and replaces the running executable atomically. Settings in
`~/.config/mikmik/` are preserved.

---

## Manual install from GitHub Releases

If you'd rather not run an install script, grab archives directly from
[**GitHub Releases**](https://github.com/KilimcininKorOglu/mikmik/releases):

| Archive                        | Platform                                 |
|--------------------------------|------------------------------------------|
| `mikmik-windows-x86_64.zip`   | Windows 64-bit                           |
| `mikmik-linux-x86_64.tar.gz`  | Linux x86_64                             |
| `mikmik-linux-aarch64.tar.gz` | Linux ARM64                              |
| `mikmik-macos-x86_64.tar.gz`  | macOS Intel                              |
| `mikmik-macos-aarch64.tar.gz` | macOS Apple Silicon (M1 to M5, A-series) |

Every archive contains a single binary named `mikmik` (or `mikmik.exe`).
Extract it and put it somewhere on your `PATH`. For example on Linux:

```bash
curl -L https://github.com/KilimcininKorOglu/mikmik/releases/latest/download/mikmik-linux-x86_64.tar.gz \
  | tar -xz
chmod +x mikmik
sudo mv mikmik /usr/local/bin/
```

On macOS, also strip the quarantine flag so Gatekeeper allows the unsigned
binary:

```bash
xattr -rd com.apple.quarantine /usr/local/bin/mikmik
```

On Windows, extract the zip and add the folder containing `mikmik.exe`
to your user `PATH` via **Settings → System → Advanced system settings →
Environment Variables**.

### User-local install without sudo

```bash
mkdir -p ~/.local/bin
mv mikmik ~/.local/bin/mikmik
```

Then put that directory on your `PATH`.

```bash
# bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

```zsh
# zsh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

```fish
# fish
fish_add_path ~/.local/bin
```

`fish_add_path` writes `$fish_user_paths`, a universal variable, so the entry
survives new sessions and is not duplicated on a second run. Nothing needs to
be sourced and no config file needs editing. The one-liner installers already
do this for you.

---

## Verifying the Installation

```bash
mikmik --version
```

A successful installation prints the version string, for example:

```
mikmik 0.1.7
```

To confirm the binary is the one you installed:

```bash
which mikmik          # Linux / macOS
where mikmik          # Windows (Command Prompt)
```

---

## Building from Source

Building from source requires the Rust toolchain (stable channel, 1.75 or
later). Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Clone and build

```bash
git clone https://github.com/KilimcininKorOglu/mikmik.git
cd mikmik/src-rust

# Debug build (fast to compile, larger binary, extra runtime checks)
cargo build --package mikmik

# Release build (optimised, smaller, suitable for everyday use)
cargo build --release --package mikmik
```

The release binary is placed at:

```
src-rust/target/release/mikmik        # Linux / macOS
src-rust/target\release\mikmik.exe   # Windows
```

Copy it to a directory on your `PATH` as described above.

### Linux system dependencies

On Linux, the build requires ALSA development headers (for the optional voice
feature) and OpenSSL:

```bash
# Debian / Ubuntu
sudo apt-get install -y libasound2-dev libssl-dev pkg-config

# Fedora / RHEL
sudo dnf install -y alsa-lib-devel openssl-devel

# Arch
sudo pacman -S alsa-lib openssl
```

### Cargo features

The `mikmik` package has two features, `voice` and `computer-use`, and both are
on by default. Turn them off to build without microphone support and without
desktop control, which is what drops the Linux system libraries below:

```bash
cargo build --release --package mikmik --no-default-features
```

`voice` needs ALSA. `computer-use` (screenshot capture and mouse/keyboard
control) reaches `xcap` and `enigo`, which on Linux need wayland, pipewire, EGL
and libclang. On Debian and Ubuntu that is:

```bash
sudo apt-get install -y pkg-config libasound2-dev \
  libwayland-dev libpipewire-0.3-dev libclang-dev libegl-dev
```

Turning `computer-use` off in the build is not the same as turning it off for
the model. `computerUseEnabled` decides whether the tool is offered, and it is
off by default; the Cargo feature decides whether the tool exists to offer.

`mikmik-core` carries `dev_full`, which the `mikmik` package does not reach:
naming it there is an error.

### Cross-compiling for Linux aarch64

The release workflow uses [cross](https://github.com/cross-rs/cross) for
aarch64 Linux builds. To reproduce it locally:

```bash
cargo install cross --git https://github.com/cross-rs/cross
cd src-rust
cross build --release --locked --package mikmik --target aarch64-unknown-linux-gnu
```

`cross` manages the Docker sysroot, OpenSSL, and ALSA headers automatically.

---

## Shell Completions

MikMik does not currently ship a dedicated `completions` subcommand. All
flags can be discovered via `mikmik --help`. For basic tab completion you can
use the generic helper built into your shell:

```bash
# bash — add to ~/.bashrc
complete -C mikmik mikmik

# zsh — add to ~/.zshrc (requires compinit)
compdef _gnu_generic mikmik
```

```fish
# fish — add to ~/.config/fish/completions/mikmik.fish
complete -c mikmik -a '(mikmik --help 2>&1 | string match -r -- "--\S+")'
```

The fish line scrapes the long flags out of `--help` on every completion. It
deliberately omits `-f`: that switch would turn off filename completion for the
whole command, and `--add-dir` and `--system-prompt-file` both take a path.

Richer completion scripts may be added in a future release.

---

## Uninstalling

If you used the install script, remove the binary it installed:

```bash
rm -f ~/.local/bin/mikmik               # Linux / macOS
# Windows (PowerShell):
Remove-Item -Force "$env:LOCALAPPDATA\Programs\mikmik\mikmik.exe"
# and the directory, once it is empty:
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\mikmik" -ErrorAction SilentlyContinue
```

For manual installs:

```bash
sudo rm /usr/local/bin/mikmik           # if installed system-wide
rm ~/.local/bin/mikmik                  # if installed user-local
```

To also remove all settings and session data:

```bash
rm -rf ~/.config/mikmik
```

You may also want to remove the `# mikmik` PATH line that the installer
appended to your shell config (`.bashrc`, `.zshrc`, etc.), or the Windows user
`PATH` entry for `%LOCALAPPDATA%\Programs\mikmik`.
