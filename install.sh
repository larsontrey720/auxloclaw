#!/usr/bin/env bash
set -euo pipefail

# AUXLOCLAW Installer
# Usage: curl -sSL https://raw.githubusercontent.com/Auxlo-xyz/auxloclaw/master/install.sh | bash
# Options:
#   AUXLOCLAW_VERSION       - specific version tag (default: latest)
#   AUXLOCLAW_DIR           - install directory (default: /usr/local/bin)
#   AUXLOCLAW_SKIP_CONFIRM  - set to 1 to skip confirmation prompt

REPO="Auxlo-xyz/auxloclaw"
BINARY="auxloclaw"
INSTALL_DIR="${AUXLOCLAW_DIR:-/usr/local/bin}"
GITHUB_API="https://api.github.com/repos/${REPO}/releases"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# All output goes to stderr so stdout is clean for return values
info()  { echo -e "${CYAN}[info]${NC} $*" >&2; }
ok()    { echo -e "${GREEN}[ok]${NC} $*" >&2; }
warn()  { echo -e "${YELLOW}[warn]${NC} $*" >&2; }
err()   { echo -e "${RED}[error]${NC} $*" >&2; }
die()   { err "$*"; exit 1; }

# Global temp dir -- cleaned up on exit
TMP_DIR=""

cleanup() {
    if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)   echo "x86_64" ;;
        aarch64|arm64)   echo "aarch64" ;;
        armv7l|armhf)    echo "armv7" ;;
        *)               die "Unsupported architecture: $arch" ;;
    esac
}

detect_os() {
    local os
    os="$(uname -s)"
    case "$os" in
        Linux)   echo "linux" ;;
        Darwin)  echo "macos" ;;
        *)       die "Unsupported OS: $os. AUXLOCLAW currently supports Linux and macOS." ;;
    esac
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"
}

check_deps() {
    need_cmd curl
}

get_latest_version() {
    local version
    version="$(curl -sL "${GITHUB_API}/latest" 2>/dev/null | grep '"tag_name"' | head -1 | sed -E 's/.*"tag_name":\s*"([^"]+)".*/\1/')"
    if [ -z "$version" ]; then
        echo ""
    else
        echo "$version"
    fi
}

download_binary() {
    local version="$1" os="$2" arch="$3"
    local url

    # The release asset is named with platform suffix
    local target
    case "${os}-${arch}" in
        linux-x86_64)  target="x86_64-unknown-linux-musl" ;;
        linux-aarch64) target="aarch64-unknown-linux-musl" ;;
        macos-x86_64)  target="x86_64-apple-darwin" ;;
        macos-aarch64) target="aarch64-apple-darwin" ;;
        *) warn "Unsupported platform: ${os}-${arch}"; return 1 ;;
    esac
    local asset_name="${BINARY}-${target}"
    url="https://github.com/${REPO}/releases/download/${version}/${asset_name}"

    info "Downloading ${BINARY} ${version} for ${os}/${arch}..."

    local http_code
    http_code=$(curl -sL -w '%{http_code}' -o "${TMP_DIR}/${BINARY}" "$url")
    if [ "$http_code" != "200" ] || [ ! -s "${TMP_DIR}/${BINARY}" ]; then
        warn "Download failed (no pre-built binary for ${os}/${arch})"
        return 1
    fi

    chmod +x "${TMP_DIR}/${BINARY}"
    # Only the path goes to stdout
    echo "${TMP_DIR}/${BINARY}"
}

build_from_source() {
    info "No pre-built binary available. Building from source..."

    need_cmd git

    # Check for Rust toolchain
    if ! command -v cargo >/dev/null 2>&1; then
        info "Installing Rust toolchain..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        export PATH="$HOME/.cargo/bin:$PATH"
    fi

    info "Cloning repository..."
    git clone --depth 1 "https://github.com/${REPO}.git" "${TMP_DIR}/src" 2>&1 | tail -3 >&2

    info "Building release binary (this may take a few minutes)..."
    (
        cd "${TMP_DIR}/src"
        cargo build --release 2>&1 | tail -5 >&2
    )

    local binary_path="${TMP_DIR}/src/target/release/${BINARY}"
    if [ ! -f "$binary_path" ]; then
        die "Build failed. Binary not found at ${binary_path}"
    fi

    chmod +x "$binary_path"
    # Only the path goes to stdout
    echo "$binary_path"
}

install_binary() {
    local binary_path="$1"

    if [ ! -f "$binary_path" ]; then
        die "Binary not found: $binary_path"
    fi

    mkdir -p "$INSTALL_DIR"
    cp "$binary_path" "${INSTALL_DIR}/${BINARY}"
    chmod +x "${INSTALL_DIR}/${BINARY}"
    ok "Installed ${BINARY} to ${INSTALL_DIR}/${BINARY}"
}

post_install() {
    # Check if install dir is in PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        warn "${INSTALL_DIR} is not in your PATH."
        echo ""
        echo "  Add it to your shell profile:" >&2
        echo "" >&2
        if [ -f "$HOME/.zshrc" ]; then
            echo "    echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc" >&2
            echo "    source ~/.zshrc" >&2
        elif [ -f "$HOME/.bashrc" ]; then
            echo "    echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc" >&2
            echo "    source ~/.bashrc" >&2
        else
            echo "    export PATH=\"${INSTALL_DIR}:\$PATH\"" >&2
        fi
        echo "" >&2
    fi

    # Install optional dependencies
    echo "" >&2
    info "Installing optional dependencies..."

    # agent-browser (headless browser automation)
    if ! command -v agent-browser >/dev/null 2>&1; then
        if command -v npm >/dev/null 2>&1; then
            info "Installing agent-browser..."
            npm install -g agent-browser 2>/dev/null \
                && agent-browser install --with-deps 2>/dev/null \
                || warn "agent-browser install failed (manual: npm install -g agent-browser && agent-browser install --with-deps)"
        else
            warn "npm not found -- skipping agent-browser (install Node.js, then: npm install -g agent-browser)"
        fi
    else
        ok "agent-browser already installed"
    fi

    # webserp (multi-engine web search)
    if ! command -v webserp >/dev/null 2>&1; then
        if command -v pip3 >/dev/null 2>&1; then
            info "Installing webserp..."
            pip3 install webserp --break-system-packages -q 2>/dev/null \
                || warn "webserp install failed (manual: pip install webserp)"
        elif command -v pip >/dev/null 2>&1; then
            pip install webserp --break-system-packages -q 2>/dev/null \
                || warn "webserp install failed (manual: pip install webserp)"
        else
            warn "pip not found -- skipping webserp (install Python, then: pip install webserp)"
        fi
    else
        ok "webserp already installed"
    fi

    # scrapling (stealth web fetching with anti-bot bypass)
    if ! command -v scrapling >/dev/null 2>&1; then
        if command -v pip3 >/dev/null 2>&1; then
            info "Installing scrapling (stealth web fetcher)..."
            pip3 install 'scrapling[all]>=0.4.7' --break-system-packages -q 2>/dev/null \
                && scrapling install 2>/dev/null \
                || warn "scrapling install failed (manual: pip install 'scrapling[all]>=0.4.7' && scrapling install)"
        elif command -v pip >/dev/null 2>&1; then
            pip install 'scrapling[all]>=0.4.7' --break-system-packages -q 2>/dev/null \
                && scrapling install 2>/dev/null \
                || warn "scrapling install failed (manual: pip install 'scrapling[all]>=0.4.7' && scrapling install)"
        else
            warn "pip not found -- skipping scrapling (install Python, then: pip install 'scrapling[all]>=0.4.7' && scrapling install)"
        fi
    else
        ok "scrapling already installed"
    fi

    # playwright (required by scrapling stealth/dynamic modes)
    if ! python3 -c "import playwright" 2>/dev/null; then
        if command -v pip3 >/dev/null 2>&1; then
            info "Installing playwright (browser automation)..."
            pip3 install playwright --break-system-packages -q 2>/dev/null \
                || warn "playwright install failed (manual: pip install playwright)"
        elif command -v pip >/dev/null 2>&1; then
            info "Installing playwright (browser automation)..."
            pip install playwright --break-system-packages -q 2>/dev/null \
                || warn "playwright install failed (manual: pip install playwright)"
        else
            warn "pip not found -- skipping playwright (install Python, then: pip install playwright)"
        fi
    else
        ok "playwright already installed"
    fi

    # Download Chromium for Playwright (if not already installed)
    if python3 -c "import playwright" 2>/dev/null; then
        if ! python3 -c "
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    b = p.chromium
" 2>/dev/null; then
            info "Downloading Chromium for Playwright..."
            python3 -m playwright install --with-deps chromium 2>&1 | tail -5 >&2 \
                || warn "Chromium download failed (manual: playwright install --with-deps chromium)"
        else
            ok "Chromium already installed for Playwright"
        fi
    fi

    # Deploy stealth_fetch helper script
    local HELPER_DIR="/usr/local/share/auxloclaw"
    local HELPER_PATH="${HELPER_DIR}/stealth_fetch_helper.py"
    if [ ! -f "$HELPER_PATH" ]; then
        info "Deploying stealth_fetch helper script..."
        mkdir -p "$HELPER_DIR"
        curl -fsSL "https://raw.githubusercontent.com/Auxlo-xyz/auxloclaw/master/scripts/stealth_fetch_helper.py" \
            -o "$HELPER_PATH" \
            && chmod +x "$HELPER_PATH" \
            || warn "Failed to download stealth_fetch helper script"
    else
        ok "stealth_fetch helper script deployed"
    fi

    echo "" >&2
    echo -e "${BOLD}Next steps:${NC}" >&2
    echo "" >&2
    echo "  1. Run the setup wizard:" >&2
    echo -e "     ${CYAN}auxloclaw setup${NC}" >&2
    echo "" >&2
    echo "  2. Add MCP integrations (GitHub, etc):" >&2
    echo -e "     ${CYAN}auxloclaw mcp add github${NC}" >&2
    echo -e "     ${CYAN}auxloclaw token set GITHUB_TOKEN your-token-here${NC}" >&2
    echo "" >&2
    echo "  3. Start the gateway:" >&2
    echo -e "     ${CYAN}auxloclaw gateway${NC}" >&2
    echo "" >&2
    echo -e "  Docs: ${CYAN}https://github.com/${REPO}${NC}" >&2
    echo "" >&2
}

main() {
    echo "" >&2
    echo -e "${BOLD}AUXLOCLAW Installer${NC}" >&2
    echo "  Ultra-High-Performance AI Agent Framework" >&2
    echo "" >&2

    check_deps

    local os arch
    os="$(detect_os)"
    arch="$(detect_arch)"
    info "Detected: ${os}/${arch}"

    # Create global temp dir (cleaned up by trap)
    TMP_DIR="$(mktemp -d)"

    # Check for existing install
    if command -v "$BINARY" >/dev/null 2>&1; then
        local current_version
        current_version="$($BINARY --version 2>/dev/null || echo 'unknown')"
        warn "Existing installation found: ${current_version}"
        if [ "${AUXLOCLAW_SKIP_CONFIRM:-0}" != "1" ]; then
            if [ -t 0 ]; then
                # stdin is a terminal -- safe to prompt
                read -r -p "  Overwrite? [y/N] " answer </dev/tty
                case "$answer" in
                    [yY]*) ;;
                    *) info "Cancelled."; exit 0 ;;
                esac
            else
                # stdin is a pipe -- default to overwrite
                warn "Non-interactive mode detected. Overwriting automatically."
            fi
        fi
    fi

    local version
    version="${AUXLOCLAW_VERSION:-$(get_latest_version)}"

    if [ -z "$version" ]; then
        warn "No releases found. Will build from source."
    else
        info "Version: ${version}"
    fi

    local binary_path=""
    if [ -n "$version" ]; then
        binary_path="$(download_binary "$version" "$os" "$arch")" || true
    fi

    if [ -z "$binary_path" ] || [ ! -f "$binary_path" ]; then
        binary_path="$(build_from_source)"
    fi

    install_binary "$binary_path"

    # Verify
    if command -v "$BINARY" >/dev/null 2>&1 || [ -x "${INSTALL_DIR}/${BINARY}" ]; then
        local installed_version
        installed_version="$("${INSTALL_DIR}/${BINARY}" --version 2>/dev/null || echo 'installed')"
        ok "AUXLOCLAW ${installed_version} installed successfully!"
    else
        warn "Binary installed but not found in PATH. You may need to restart your shell."
    fi

    post_install
}

main "$@"
