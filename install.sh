#!/bin/sh
# bento installer — downloads a released binary and installs to INSTALL_DIR.
#
# Usage:
#   curl -fsSL https://bento.build/install | sh
#   curl -fsSL https://bento.build/install | sh -s -- 0.1.0
#   BENTO_VERSION=0.1.0 BENTO_INSTALL_DIR=~/.local/bin sh install.sh
#
# (bento.build/install is a Cloudflare Pages proxy that serves this
# file from the bento-sh/bento repo.)

set -eu

GITHUB_REPO="${GITHUB_REPO:-bento-sh/bento}"
VERSION="${BENTO_VERSION:-${1:-}}"
INSTALL_DIR="${BENTO_INSTALL_DIR:-${HOME}/.local/bin}"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Private-repo support: when GITHUB_TOKEN is set, add the Authorization
# header to every GitHub request. Works for both raw release asset
# downloads and the API call for the latest version.
curl_auth() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        curl -H "Authorization: token ${GITHUB_TOKEN}" "$@"
    else
        curl "$@"
    fi
}

# ── Detect platform ───────────────────────────────────────────────

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  os_triple=unknown-linux-gnu ;;
    Darwin) os_triple=apple-darwin ;;
    *)      die "unsupported OS: $os (v0.1 ships binaries for Linux + macOS only; Windows support coming in v0.2 — for now use 'cargo install bento-cli' on Windows)" ;;
esac

case "$arch" in
    x86_64|amd64)  arch_triple=x86_64 ;;
    aarch64|arm64) arch_triple=aarch64 ;;
    *)             die "unsupported architecture: $arch" ;;
esac

target="${arch_triple}-${os_triple}"

# ── Resolve version ───────────────────────────────────────────────

if [ -z "$VERSION" ]; then
    log "resolving latest release..."
    VERSION=$(
        curl_auth -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' \
        | head -n1
    )
    [ -n "$VERSION" ] || die "could not resolve latest bento version from ${GITHUB_REPO} (set GITHUB_TOKEN if the repo is private)"
fi

tag="v${VERSION}"
asset="bento-${VERSION}-${target}"
url="https://github.com/${GITHUB_REPO}/releases/download/${tag}/${asset}.tar.gz"

log "bento v${VERSION} · target ${target}"
log "download: $url"

# ── Download + verify + extract ───────────────────────────────────

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl_auth -fsSL -o "$tmp/bento.tar.gz" "$url" || die "download failed: $url (set GITHUB_TOKEN if the repo is private)"

sha_url="${url}.sha256"
if curl_auth -fsSL -o "$tmp/bento.tar.gz.sha256" "$sha_url" 2>/dev/null; then
    log "verifying checksum..."
    expected=$(awk '{print $1}' "$tmp/bento.tar.gz.sha256")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$tmp/bento.tar.gz" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$tmp/bento.tar.gz" | awk '{print $1}')
    else
        log "no sha256sum or shasum available — skipping verification"
        actual="$expected"
    fi
    [ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual)"
fi

tar -xzf "$tmp/bento.tar.gz" -C "$tmp"
src="$tmp/${asset}/bento"
src_mcp="$tmp/${asset}/bento-mcp"
[ -x "$src" ] || die "extracted archive has no executable at $src"
have_mcp=0
[ -x "$src_mcp" ] && have_mcp=1

# ── Install ───────────────────────────────────────────────────────

mkdir -p "$INSTALL_DIR"
dest="$INSTALL_DIR/bento"
mv "$src" "$dest"
chmod +x "$dest"
log "installed: $dest"

if [ "$have_mcp" -eq 1 ]; then
    dest_mcp="$INSTALL_DIR/bento-mcp"
    mv "$src_mcp" "$dest_mcp"
    chmod +x "$dest_mcp"
    log "installed: $dest_mcp"
fi

# ── Skill bundle ──────────────────────────────────────────────────
#
# Drop the Claude Code skill (SKILL.md + bento-guard PreToolUse hook)
# under ~/.claude/skills/bento/ so a fresh Claude Code session picks
# it up without the user copying files. Honoured by `BENTO_SKILL_DIR`
# for non-default Claude installs (e.g. self-hosted, repo-local).
# Skip if the destination already exists and the user hasn't passed
# --force-skill: don't clobber a custom edit.
src_skill="$tmp/${asset}/skills/bento"
if [ -d "$src_skill" ]; then
    skill_dir="${BENTO_SKILL_DIR:-${HOME}/.claude/skills/bento}"
    if [ -d "$skill_dir" ] && [ "${BENTO_FORCE_SKILL:-}" != "1" ]; then
        log "skill bundle already present at ${skill_dir} (set BENTO_FORCE_SKILL=1 to overwrite)"
    else
        mkdir -p "$skill_dir"
        # Copy file-by-file with cp -R so existing user customisations
        # under skill_dir aren't blown away on incremental updates.
        cp -R "$src_skill/." "$skill_dir/"
        # Hook script needs +x; cp on some platforms drops the bit.
        if [ -f "$skill_dir/hooks/bento-guard.sh" ]; then
            chmod +x "$skill_dir/hooks/bento-guard.sh"
        fi
        log "installed skill bundle: $skill_dir"
        log "    register the bento-guard PreToolUse hook in your Claude Code settings —"
        log "    see ${skill_dir}/SKILL.md § 'Recommended: install the bento-guard hook'."
    fi
fi

case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) log "warning: ${INSTALL_DIR} is not on your PATH. Add it with:"
       log "    export PATH=\"${INSTALL_DIR}:\$PATH\""
       ;;
esac

"$dest" --version
[ "$have_mcp" -eq 1 ] && "$dest_mcp" --version
