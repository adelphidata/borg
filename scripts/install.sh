#!/usr/bin/env bash
set -euo pipefail

# ── Borg Installer ──────────────────────────────────────────────────────────
# Usage: curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
# Or:    bash install.sh [OPTIONS]
#
# Options:
#   --version <tag>       Install a specific version (default: latest)
#   --install-dir <path>  Install directory (default: ~/.local/bin)
#   --no-onboarding       Skip running 'borg init' after install
#   --uninstall           Remove borg binary and optionally ~/.borg/
#   --dry-run             Show what would be done without making changes
#   --help                Show this help message

REPO="borganization/borg"
BINARY_NAME="borg"
DEFAULT_INSTALL_DIR="$HOME/.local/bin"
DATA_DIR="$HOME/.borg"
GITHUB_API="https://api.github.com/repos/$REPO"

# ── Options (defaults) ──
VERSION="latest"
INSTALL_DIR="$DEFAULT_INSTALL_DIR"
NO_ONBOARDING=false
UNINSTALL=false
DRY_RUN=false

# ── Detected environment ──
OS=""
ARCH=""
EXISTING_VERSION=""
TARGET_VERSION=""

# ── Colors ──
if [[ -t 1 ]] && [[ -z "${NO_COLOR:-}" ]] && [[ "${TERM:-}" != "dumb" ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    BOLD='\033[1m'
    RESET='\033[0m'
else
    RED='' GREEN='' YELLOW='' BLUE='' BOLD='' RESET=''
fi

# ── Output helpers ──

info()    { printf "${BLUE}·${RESET} %s\n" "$*"; }
success() { printf "${GREEN}✓${RESET} %s\n" "$*"; }
warn()    { printf "${YELLOW}!${RESET} %s\n" "$*" >&2; }
error()   { printf "${RED}✗${RESET} %s\n" "$*" >&2; }
step()    { printf "\n${BOLD}[%s]${RESET} %s\n" "$1" "$2"; }

# ── Cleanup ──

TMPDIR_INSTALL=""
cleanup() {
    if [[ -n "$TMPDIR_INSTALL" ]] && [[ -d "$TMPDIR_INSTALL" ]]; then
        rm -rf "$TMPDIR_INSTALL"
    fi
}
trap cleanup EXIT

# ── Helpers ──

has_command() { command -v "$1" &>/dev/null; }

is_interactive() { [[ -t 0 ]] && [[ -t 1 ]]; }

download() {
    local url="$1" dest="$2"
    if has_command curl; then
        curl -fsSL --retry 3 --retry-delay 2 -o "$dest" "$url"
    elif has_command wget; then
        wget -q -O "$dest" "$url"
    else
        error "Neither curl nor wget found. Please install one."
        exit 1
    fi
}

download_json() {
    local url="$1"
    if has_command curl; then
        curl -fsSL --retry 3 --retry-delay 2 -H "Accept: application/vnd.github+json" "$url"
    elif has_command wget; then
        wget -q -O - --header="Accept: application/vnd.github+json" "$url"
    else
        error "Neither curl nor wget found."
        exit 1
    fi
}

sha256_verify() {
    local file="$1" expected="$2"
    local actual
    if has_command sha256sum; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif has_command shasum; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    elif has_command openssl; then
        actual=$(openssl dgst -sha256 -r "$file" | awk '{print $1}')
    else
        error "Cannot verify checksum: none of sha256sum, shasum, or openssl found."
        error "Install one of these tools and retry."
        return 1
    fi
    if [[ "$actual" != "$expected" ]]; then
        error "Checksum mismatch!"
        error "  Expected: $expected"
        error "  Got:      $actual"
        return 1
    fi
    return 0
}

# ── Environment detection ──

detect_os() {
    case "$(uname -s)" in
        Darwin) OS="darwin" ;;
        Linux)  OS="linux" ;;
        *)
            error "Unsupported operating system: $(uname -s)"
            error "Borg supports macOS and Linux."
            exit 1
            ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   ARCH="x86_64" ;;
        arm64|aarch64)  ARCH="arm64" ;;
        *)
            error "Unsupported architecture: $(uname -m)"
            error "Borg supports x86_64 and arm64."
            exit 1
            ;;
    esac
}

detect_shell_profile() {
    local shell_name
    shell_name=$(basename "${SHELL:-/bin/sh}")
    case "$shell_name" in
        zsh)  echo "$HOME/.zshrc" ;;
        bash)
            if [[ -f "$HOME/.bash_profile" ]]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.bashrc"
            fi
            ;;
        fish) echo "$HOME/.config/fish/config.fish" ;;
        *)    echo "$HOME/.profile" ;;
    esac
}

# ── Argument parsing ──

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --version)
                VERSION="${2:-}"
                if [[ -z "$VERSION" ]]; then
                    error "--version requires a value"
                    exit 1
                fi
                shift 2
                ;;
            --install-dir)
                INSTALL_DIR="${2:-}"
                if [[ -z "$INSTALL_DIR" ]]; then
                    error "--install-dir requires a value"
                    exit 1
                fi
                shift 2
                ;;
            --no-onboarding) NO_ONBOARDING=true; shift ;;
            --uninstall)     UNINSTALL=true; shift ;;
            --dry-run)       DRY_RUN=true; shift ;;
            --help|-h)       usage; exit 0 ;;
            *)
                error "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done
}

usage() {
    cat <<'EOF'

Usage: install.sh [OPTIONS]

Options:
  --version <tag>       Install a specific version (default: latest)
  --install-dir <path>  Install directory (default: ~/.local/bin)
  --no-onboarding       Skip running 'borg init' after install
  --uninstall           Remove borg binary and optionally ~/.borg/
  --dry-run             Show what would be done without making changes
  --help                Show this help message

Examples:
  curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
  bash install.sh --version v0.2.0
  bash install.sh --uninstall

EOF
}

# ── Pre-flight ──

check_existing_install() {
    local existing
    existing=$(command -v "$BINARY_NAME" 2>/dev/null || true)
    if [[ -n "$existing" ]]; then
        EXISTING_VERSION=$("$existing" --version 2>/dev/null | awk '{print $2}' || echo "unknown")
        info "Found existing borg $EXISTING_VERSION at $existing"
    fi
}

# ── Version resolution ──

resolve_version() {
    if [[ "$VERSION" == "latest" ]]; then
        info "Resolving latest release..."
        local release_json
        release_json=$(download_json "$GITHUB_API/releases/latest")
        TARGET_VERSION=$(echo "$release_json" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')
        if [[ -z "$TARGET_VERSION" ]]; then
            error "Could not determine latest release version."
            error "Check https://github.com/$REPO/releases"
            exit 1
        fi
    else
        TARGET_VERSION="$VERSION"
    fi

    # Validate version format to prevent path traversal in download URLs
    if ! echo "$TARGET_VERSION" | grep -qE '^v?[0-9]+\.[0-9]+\.[0-9]+'; then
        error "Invalid version format: $TARGET_VERSION (expected vX.Y.Z)"
        exit 1
    fi
}

# ── Download & install ──

download_and_install() {
    local asset="borg-${OS}-${ARCH}.tar.gz"
    local checksum_asset="checksums.txt"
    local tag="${TARGET_VERSION}"
    local base_url="https://github.com/$REPO/releases/download/$tag"

    TMPDIR_INSTALL=$(mktemp -d)
    local archive="$TMPDIR_INSTALL/$asset"
    local checksums="$TMPDIR_INSTALL/$checksum_asset"

    info "Downloading borg ${tag} for ${OS}/${ARCH}..."
    download "$base_url/$asset" "$archive"

    info "Downloading checksums..."
    if ! download "$base_url/$checksum_asset" "$checksums"; then
        error "Failed to download checksums.txt — cannot verify binary integrity."
        error "This may indicate a network issue or a tampered release."
        exit 1
    fi

    local expected
    expected=$(grep "$asset" "$checksums" | awk '{print $1}')
    if [[ -z "$expected" ]]; then
        error "No checksum entry found for $asset in checksums.txt"
        error "The release may be incomplete or corrupted."
        exit 1
    fi

    sha256_verify "$archive" "$expected" || exit 1
    success "Checksum verified"

    # Extract
    tar xzf "$archive" -C "$TMPDIR_INSTALL"

    if [[ ! -f "$TMPDIR_INSTALL/$BINARY_NAME" ]]; then
        error "Binary not found in archive. Expected: $BINARY_NAME"
        exit 1
    fi

    # Install
    mkdir -p "$INSTALL_DIR"
    mv "$TMPDIR_INSTALL/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"
    success "Borg installed to $INSTALL_DIR/$BINARY_NAME"
}

# ── PATH setup ──

ensure_path() {
    if echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        return 0
    fi

    local profile
    profile=$(detect_shell_profile)
    local shell_name
    shell_name=$(basename "${SHELL:-/bin/sh}")

    local line
    if [[ "$shell_name" == "fish" ]]; then
        line="fish_add_path $INSTALL_DIR"
    else
        line="export PATH=\"$INSTALL_DIR:\$PATH\""
    fi

    if [[ -f "$profile" ]] && grep -qF "$INSTALL_DIR" "$profile"; then
        return 0
    fi

    echo "" >> "$profile"
    echo "# Added by Borg installer" >> "$profile"
    echo "$line" >> "$profile"
    success "PATH updated in $profile"
    info "Run 'source $profile' or open a new terminal to use borg"

    # Also update current session
    export PATH="$INSTALL_DIR:$PATH"
}

# ── Uninstall ──

do_uninstall() {
    local binary
    binary=$(command -v "$BINARY_NAME" 2>/dev/null || echo "$INSTALL_DIR/$BINARY_NAME")

    if [[ -f "$binary" ]]; then
        rm -f "$binary"
        success "Removed $binary"
    else
        warn "borg binary not found"
    fi

    if [[ -d "$DATA_DIR" ]]; then
        if is_interactive; then
            printf "Remove data directory %s? [y/N] " "$DATA_DIR"
            read -r answer
            if [[ "$answer" =~ ^[Yy]$ ]]; then
                rm -rf "$DATA_DIR"
                success "Removed $DATA_DIR"
            else
                info "Kept $DATA_DIR"
            fi
        else
            info "Kept $DATA_DIR (non-interactive mode)"
        fi
    fi

    success "Borg uninstalled"
}

# ── Main ──

main() {
    parse_args "$@"

    # Uninstall flow
    if [[ "$UNINSTALL" == true ]]; then
        if [[ "$DRY_RUN" == true ]]; then
            info "Would uninstall borg"
            exit 0
        fi
        do_uninstall
        exit 0
    fi

    # Detect environment
    detect_os
    detect_arch
    success "Detected: $OS ($ARCH)"

    # Resolve target version
    resolve_version

    # Show install plan
    printf "\n${BOLD}Install plan${RESET}\n"
    printf "OS: %s\n" "$OS"
    printf "Arch: %s\n" "$ARCH"
    printf "Install method: binary\n"
    printf "Requested version: %s\n" "$TARGET_VERSION"

    # Check existing
    check_existing_install
    if [[ -n "$EXISTING_VERSION" ]] && [[ "$EXISTING_VERSION" == "${TARGET_VERSION#v}" ]]; then
        success "Already installed and up to date ($EXISTING_VERSION)"
        exit 0
    fi

    # Dry run exit
    if [[ "$DRY_RUN" == true ]]; then
        printf "\n"
        info "Dry run — no changes made"
        info "Would download: borg-${OS}-${ARCH}.tar.gz"
        info "Would install to: $INSTALL_DIR/$BINARY_NAME"
        exit 0
    fi

    # Install
    step "1/2" "Installing Borg"
    download_and_install

    # PATH
    step "2/2" "Finalizing setup"
    ensure_path

    # Verify
    local installed_version
    installed_version=$("$INSTALL_DIR/$BINARY_NAME" --version 2>/dev/null | awk '{print $2}' || echo "unknown")
    printf "\n"
    success "Borg installed successfully (borg $installed_version)"
    printf "\n"

    # Onboarding
    if [[ "$NO_ONBOARDING" == false ]] && is_interactive && [[ ! -f "$DATA_DIR/config.toml" ]]; then
        info "Starting setup..."
        printf "\n"
        "$INSTALL_DIR/$BINARY_NAME" init
    elif [[ ! -f "$DATA_DIR/config.toml" ]]; then
        info "Run 'borg init' to complete setup"
    fi
}

main "$@"
