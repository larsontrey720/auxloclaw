#!/usr/bin/env bash
set -euo pipefail

REPO="Auxlo-xyz/auxloclaw"
BINARY="auxloclaw"
INSTALL_DIR="/usr/local/bin"

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)  os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) echo "Unsupported OS: $os"; exit 1 ;;
    esac
    case "$arch" in
        x86_64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) echo "Unsupported arch: $arch"; exit 1 ;;
    esac
    echo "${arch}-${os}"
}

ensure_npm() {
    if command -v npm &>/dev/null; then
        return
    fi
    echo "==> npm not found. Installing Node.js..."
    if command -v apt-get &>/dev/null; then
        curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
        apt-get install -y nodejs
    elif command -v brew &>/dev/null; then
        brew install node
    else
        echo "Warning: Could not install Node.js automatically. Please install it manually (https://nodejs.org)."
        return 1
    fi
}

# Resolve the pip command, installing pip if needed
ensure_pip() {
    if command -v pip3 &>/dev/null; then
        PIP=pip3
        return
    elif command -v pip &>/dev/null; then
        PIP=pip
        return
    fi
    echo "==> pip not found. Installing pip..."
    if command -v apt-get &>/dev/null; then
        apt-get update -qq && apt-get install -y -qq python3-pip >/dev/null 2>&1
        if command -v pip3 &>/dev/null; then
            PIP=pip3
            return
        fi
    fi
    if command -v python3 &>/dev/null; then
        python3 -m ensurepip --upgrade 2>/dev/null || python3 -m ensurepip 2>/dev/null
        if command -v pip3 &>/dev/null; then
            PIP=pip3
            return
        fi
        # ensurepip may not put pip on PATH; try direct invocation
        if python3 -m pip --version &>/dev/null; then
            PIP="python3 -m pip"
            return
        fi
    fi
    echo "Warning: Could not install pip automatically. Please install pip manually."
    return 1
}

main() {
    local target
    target="$(detect_platform)"
    local url="https://github.com/${REPO}/releases/latest/download/${BINARY}-${target}"

    echo "Downloading ${BINARY} for ${target}..."
    curl -fsSL "$url" -o "/tmp/${BINARY}"
    chmod +x "/tmp/${BINARY}"

    echo "Installing to ${INSTALL_DIR}..."
    mv "/tmp/${BINARY}" "${INSTALL_DIR}/${BINARY}"

    echo "Installed: $(${BINARY} --version)"

    # Install agent-browser (Vercel) if missing
    if ! command -v agent-browser &>/dev/null; then
        echo "Installing agent-browser (by Vercel)..."
        if ensure_npm; then
            npm install -g agent-browser \
                && agent-browser install --with-deps \
                || echo "Warning: agent-browser install failed (manual: npm install -g agent-browser && agent-browser install --with-deps)"
        else
            echo "Warning: agent-browser install failed (manual: npm install -g agent-browser && agent-browser install --with-deps)"
        fi
    fi

    # Install webserp (multi-engine web search, no API key)
    if ! command -v webserp &>/dev/null; then
        echo "Installing webserp (web search engine)..."
        if ensure_pip; then
            $PIP install webserp --break-system-packages -q 2>&1 | tail -5 || echo "Warning: webserp install failed (manual: pip install webserp)"
        else
            echo "Warning: webserp install failed -- pip not available (manual: pip install webserp)"
        fi
    fi

    # Install scrapling (stealth web fetching with anti-bot bypass)
    if ! command -v scrapling &>/dev/null; then
        echo "Installing scrapling (stealth web fetcher)..."
        if ensure_pip; then
            $PIP install 'scrapling[all]>=0.4.7' --break-system-packages -q 2>&1 | tail -5 \
                && scrapling install \
                || echo "Warning: scrapling install failed (manual: pip install 'scrapling[all]>=0.4.7' && scrapling install)"
        else
            echo "Warning: scrapling install failed -- pip not available (manual: pip install 'scrapling[all]>=0.4.7' && scrapling install)"
        fi
    fi

    echo "Browser: agent-browser (by Vercel)"
    echo "Web search: webserp (multi-engine, no API key)"
    echo "Stealth fetch: scrapling (anti-bot bypass, TLS fingerprint spoofing)"
}

main "$@"
