#!/bin/sh
# Prepares /workspace (clone on first use) and then runs the given command.
set -eu

if [ -n "${CTM_REPO:-}" ] && [ ! -d /workspace/.git ]; then
    echo "[ctm] cloning ${CTM_REPO}" >&2
    if [ -n "${GH_TOKEN:-${GITHUB_TOKEN:-}}" ]; then
        # Let git authenticate to github.com over HTTPS with the forwarded token.
        git config --global credential.helper \
            '!f() { echo username=x-access-token; echo "password=${GH_TOKEN:-$GITHUB_TOKEN}"; }; f'
    fi
    git clone --quiet "$CTM_REPO" /workspace
fi

if [ -n "${CTM_GIT_NAME:-}" ]; then git config --global user.name "$CTM_GIT_NAME"; fi
if [ -n "${CTM_GIT_EMAIL:-}" ]; then git config --global user.email "$CTM_GIT_EMAIL"; fi

exec "$@"
