#!/usr/bin/env bash
# Shared shell helpers for Research publication pin scripts.

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

skip_check() {
    printf '%s check skipped: %s\n' "${PIN_CHECK_LABEL:-research pin}" "$*" >&2
    exit 0
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is unavailable: $1"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        die "neither sha256sum nor shasum is available"
    fi
}
