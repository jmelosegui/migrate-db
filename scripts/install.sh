#!/bin/bash
set -e

# migrate-db installer for Linux and macOS
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/jmelosegui/migrate-db/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/jmelosegui/migrate-db/main/scripts/install.sh | bash -s -- --prerelease
# Env: INSTALL_DIR (default ~/.migrate-db/bin)

REPO="jmelosegui/migrate-db"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.migrate-db/bin}"
TMPDIR="${TMPDIR:-/tmp}"
PRERELEASE=false

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

info() { printf "${GREEN}==>${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}warning:${NC} %s\n" "$1"; }
error() { printf "${RED}error:${NC} %s\n" "$1" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
    case $1 in
        -p|--prerelease) PRERELEASE=true; shift ;;
        *) shift ;;
    esac
done

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "darwin" ;;
        *)       error "unsupported OS: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "amd64" ;;
        arm64|aarch64) echo "arm64" ;;
        *)             error "unsupported architecture: $(uname -m)" ;;
    esac
}

get_release_tag() {
    local endpoint
    if [ "$PRERELEASE" = true ]; then
        endpoint="https://api.github.com/repos/${REPO}/releases?per_page=1"
    else
        endpoint="https://api.github.com/repos/${REPO}/releases/latest"
    fi

    curl -fsSL "$endpoint" |
        grep -m 1 '"tag_name":' |
        sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/'
}

add_to_path() {
    if [[ -n "${GITHUB_ACTIONS:-}" && -n "${GITHUB_PATH:-}" ]]; then
        echo "$INSTALL_DIR" >> "$GITHUB_PATH"
        info "Added to GITHUB_PATH for this workflow"
        return
    fi

    local shell_name config_file path_line
    shell_name=$(basename "${SHELL:-/bin/bash}")

    case "$shell_name" in
        bash)
            if [[ -f "$HOME/.bashrc" ]]; then
                config_file="$HOME/.bashrc"
            elif [[ -f "$HOME/.bash_profile" ]]; then
                config_file="$HOME/.bash_profile"
            else
                config_file="$HOME/.bashrc"
            fi
            path_line="export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
        zsh)
            config_file="${ZDOTDIR:-$HOME}/.zshrc"
            path_line="export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
        fish)
            config_file="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
            path_line="fish_add_path ${INSTALL_DIR}"
            ;;
        *)
            config_file="$HOME/.profile"
            path_line="export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
    esac

    mkdir -p "$(dirname "$config_file")"

    if [[ -f "$config_file" ]] && grep -qF "$INSTALL_DIR" "$config_file" 2>/dev/null; then
        return
    fi

    {
        echo ""
        echo "# Added by migrate-db installer"
        echo "$path_line"
    } >> "$config_file"

    info "Added ${INSTALL_DIR} to PATH in ${config_file}"
    echo
    echo "Restart your shell or run:"
    echo "  source ${config_file}"
}

main() {
    if [ "$PRERELEASE" = true ]; then
        info "Installing migrate-db (including pre-releases)..."
    else
        info "Installing migrate-db..."
    fi

    OS=$(detect_os)
    ARCH=$(detect_arch)
    info "Detected: ${OS}-${ARCH}"

    TAG=$(get_release_tag)
    if [ -z "$TAG" ]; then
        error "could not determine release tag. See https://github.com/${REPO}/releases"
    fi
    info "Release: ${TAG}"

    # Archive name: migrate-db_<version-without-v>_<os>_<arch>.tar.gz
    VERSION="${TAG#v}"
    FILENAME="migrate-db_${VERSION}_${OS}_${ARCH}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/${TAG}/${FILENAME}"

    info "Downloading ${URL}..."
    DOWNLOAD_DIR="${TMPDIR}/migrate-db-install-$$"
    mkdir -p "$DOWNLOAD_DIR"
    trap 'rm -rf "$DOWNLOAD_DIR"' EXIT

    if ! curl -fsSL "$URL" -o "${DOWNLOAD_DIR}/${FILENAME}"; then
        error "download failed: ${URL}"
    fi

    CHECKSUMS_URL="https://github.com/${REPO}/releases/download/${TAG}/checksums.txt"
    if curl -fsSL "$CHECKSUMS_URL" -o "${DOWNLOAD_DIR}/checksums.txt" 2>/dev/null; then
        info "Verifying checksum..."
        local expected actual=""
        expected=$(grep " ${FILENAME}\$" "${DOWNLOAD_DIR}/checksums.txt" | awk '{print $1}')
        if [ -n "$expected" ]; then
            if command -v sha256sum >/dev/null 2>&1; then
                actual=$(sha256sum "${DOWNLOAD_DIR}/${FILENAME}" | awk '{print $1}')
            elif command -v shasum >/dev/null 2>&1; then
                actual=$(shasum -a 256 "${DOWNLOAD_DIR}/${FILENAME}" | awk '{print $1}')
            fi
            if [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
                error "checksum mismatch for ${FILENAME}"
            fi
        else
            warn "no checksum entry for ${FILENAME}; skipping verification"
        fi
    fi

    info "Extracting..."
    tar -xzf "${DOWNLOAD_DIR}/${FILENAME}" -C "$DOWNLOAD_DIR"

    mkdir -p "$INSTALL_DIR"
    mv "${DOWNLOAD_DIR}/migrate-db" "${INSTALL_DIR}/migrate-db"
    chmod +x "${INSTALL_DIR}/migrate-db"

    info "Installed to ${INSTALL_DIR}/migrate-db"
    "${INSTALL_DIR}/migrate-db" --version || true

    if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
        add_to_path
    fi
}

main "$@"
