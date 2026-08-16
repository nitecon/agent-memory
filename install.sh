#!/usr/bin/env bash
set -euo pipefail

REPO="nitecon/agent-memory"
INSTALL_DIR="/opt/agentic/bin"
# Release 2 ships two binaries per archive: `memory` (the main CLI/MCP
# binary) and `memory-dream` (the offline batch compactor). Both are
# force-bundled — installing one always installs the other.
BINARIES=("memory" "memory-dream")
SYMLINKS=("/usr/local/bin/memory" "/usr/local/bin/memory-dream")

# --- Helpers ----------------------------------------------------------------

info()  { printf '\033[1;32m[INFO]\033[0m  %s\n' "$*"; }
warn()  { printf '\033[1;33m[WARN]\033[0m  %s\n' "$*"; }
error() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }

# --- Pre-flight checks ------------------------------------------------------

if [ "$(id -u)" -ne 0 ]; then
  error "This script must be run as root. Try: curl -fsSL <url> | sudo bash"
fi

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)  PLATFORM="linux" ;;
  darwin) PLATFORM="macos" ;;
  *)      error "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)             error "Unsupported architecture: $ARCH" ;;
esac

# --- Resolve latest version -------------------------------------------------

info "Resolving latest release..."
if command -v curl &>/dev/null; then
  DOWNLOAD="curl -fsSL"
  DOWNLOAD_OUT="curl -fsSL -o"
elif command -v wget &>/dev/null; then
  DOWNLOAD="wget -qO-"
  DOWNLOAD_OUT="wget -qO"
else
  error "Neither curl nor wget found. Install one and retry."
fi

RELEASE_JSON=$($DOWNLOAD "https://api.github.com/repos/${REPO}/releases/latest")

LATEST_TAG=$(printf '%s' "$RELEASE_JSON" | grep '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')

if [ -z "$LATEST_TAG" ]; then
  error "Could not determine latest release from GitHub."
fi

info "Latest version: ${LATEST_TAG}"

# Current asset name format: `agent-memory-<platform>-<arch>.tar.gz` (tag-less).
# Older releases shipped a tag-ful form `agent-memory-<tag>-<platform>-<arch>.tar.gz`,
# so select the matching asset from the release JSON and accept either convention.
ASSET_RE="^agent-memory-(${LATEST_TAG}-)?${PLATFORM}-${ARCH}\.tar\.gz$"

# Pull the matching asset's name straight from the release payload we already
# fetched (no second API call), then build its download URL deterministically
# from the repo + tag. Accepts both the current tag-less names and the legacy
# tag-ful ones.
ARCHIVE_NAME=""
DOWNLOAD_URL=""
while IFS= read -r asset_name; do
  if printf '%s' "$asset_name" | grep -Eq "$ASSET_RE"; then
    ARCHIVE_NAME="$asset_name"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${asset_name}"
    break
  fi
done <<EOF
$(printf '%s' "$RELEASE_JSON" | grep -oE '"name": *"agent-memory-[^"]+"' | sed -E 's/.*"name": *"([^"]+)".*/\1/')
EOF

if [ -z "$DOWNLOAD_URL" ]; then
  AVAILABLE=$(printf '%s' "$RELEASE_JSON" | grep -oE '"name": *"agent-memory-[^"]+"' | sed -E 's/.*"name": *"([^"]+)".*/\1/' | paste -sd ', ' -)
  error "Could not find a ${PLATFORM} ${ARCH} release archive for ${LATEST_TAG}. Available assets: ${AVAILABLE:-none}"
fi

# --- Check existing installation --------------------------------------------

if [ -f "${INSTALL_DIR}/memory" ]; then
  CURRENT_VERSION=$(${INSTALL_DIR}/memory --version 2>/dev/null || echo "unknown")
  info "Existing installation found: ${CURRENT_VERSION}"
  info "Upgrading to ${LATEST_TAG}..."
else
  info "No existing installation found. Installing fresh."
fi

# --- Download and extract ---------------------------------------------------

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${ARCHIVE_NAME}..."
$DOWNLOAD_OUT "${TMPDIR}/${ARCHIVE_NAME}" "$DOWNLOAD_URL"

info "Extracting..."
tar xzf "${TMPDIR}/${ARCHIVE_NAME}" -C "$TMPDIR"

# --- Compatibility validation ----------------------------------------------

# Validate the downloaded pair before replacing either installed binary. This
# is especially important for recovery from a GLIBC mismatch: the installer
# must never overwrite a working/older executable with an artifact the host
# loader cannot start.
for bin in "${BINARIES[@]}"; do
  if [ ! -f "${TMPDIR}/${bin}" ]; then
    if [ "$bin" = "memory" ]; then
      error "Archive did not contain 'memory' binary"
    fi
    warn "Archive did not contain '${bin}' — skipping compatibility check (older release?)"
    continue
  fi

  chmod +x "${TMPDIR}/${bin}"
  if ! VERSION_OUTPUT=$("${TMPDIR}/${bin}" --version 2>&1); then
    error "Downloaded ${bin} cannot run on this host. The existing installation was not changed. Loader output: ${VERSION_OUTPUT}"
  fi
  info "Validated ${bin}: ${VERSION_OUTPUT}"
done

# --- Install ----------------------------------------------------------------

mkdir -p "$INSTALL_DIR"
for bin in "${BINARIES[@]}"; do
  if [ ! -f "${TMPDIR}/${bin}" ]; then
    # Pre-R2 archives only contain `memory`. Treat the companion binary as
    # soft-missing so legacy installs don't fail, but surface a warning so
    # the user knows memory-dream isn't available.
    if [ "$bin" = "memory" ]; then
      error "Archive did not contain 'memory' binary"
    fi
    warn "Archive did not contain '${bin}' — skipping (older release?)"
    continue
  fi
  mv "${TMPDIR}/${bin}" "${INSTALL_DIR}/${bin}"
  info "Installed ${INSTALL_DIR}/${bin}"
done

# --- Symlinks ---------------------------------------------------------------

for i in "${!BINARIES[@]}"; do
  bin="${BINARIES[$i]}"
  link="${SYMLINKS[$i]}"
  if [ -f "${INSTALL_DIR}/${bin}" ]; then
    ln -sf "${INSTALL_DIR}/${bin}" "$link"
    info "Symlinked ${link} -> ${INSTALL_DIR}/${bin}"
  fi
done

# --- Done -------------------------------------------------------------------

echo ""
info "Installation complete!"
echo ""
echo "  memory:       ${INSTALL_DIR}/memory"
echo "  memory-dream: ${INSTALL_DIR}/memory-dream"
echo "  Version:      ${LATEST_TAG}"
echo ""
echo "Quick start:"
echo "  memory store \"my first memory\" -m user -t \"test\""
echo "  memory search \"first memory\""
echo ""
echo "Compact the DB on a schedule (optional — run manually or via cron):"
echo "  memory-dream --pull          # one-time, fetch gemma3 weights"
echo "  memory-dream --dry-run       # preview condensations + dedup"
echo "  memory-dream                 # apply condensation + dedup"
echo ""
echo "Register as MCP server for Claude Code:"
echo "  claude mcp add agent-memory -- ${INSTALL_DIR}/memory serve"
echo ""
