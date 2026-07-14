#!/usr/bin/env bash
# Manually run one or more xcp-orchestrator agents with the same systemd
# credentials as their unit files, bypassing the timers.
#
# The agents read secrets exclusively from $CREDENTIALS_DIRECTORY (systemd
# LoadCredential), so a bare "/usr/local/bin/<agent> --force" fails outside
# systemd. This wraps each run in systemd-run with the credentials each unit
# declares in systemd/*.service.
#
# Usage:
#   sudo ./force-run.sh                 # run all agents in force mode
#   sudo ./force-run.sh xoa-vm-agent    # run only one agent
#   sudo ./force-run.sh iso-agent --force-iso   # extra args go to the agent
#
# Agents: orchestrator, iso-agent, xoa-vm-agent
# (orchestrator has no --force flag; it is simply run.)

set -euo pipefail

CRED_DIR=/etc/xcp-hl-credentials
BIN_DIR=/usr/local/bin

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root (sudo $0 ...)" >&2
    exit 1
fi

run_agent() {
    local agent=$1; shift
    local -a creds args

    case "$agent" in
        orchestrator)
            creds=(GITHUB_TOKEN:github_token)
            args=()          # orchestrator takes no CLI flags
            ;;
        iso-agent)
            creds=(GITHUB_TOKEN:github_token)
            args=(--force)
            ;;
        xoa-vm-agent)
            creds=(GITHUB_TOKEN:github_token
                   XCPNG_PASSWORD:xcpng_password
                   ALMALINUX_ROOT_PASSWORD:almalinux_root_password)
            args=(--force)
            ;;
        *)
            echo "error: unknown agent '$agent' (expected orchestrator, iso-agent, xoa-vm-agent)" >&2
            return 1
            ;;
    esac

    # Extra args from the command line replace the default force flags.
    if [[ $# -gt 0 ]]; then
        args=("$@")
    fi

    local -a cred_opts=()
    local c
    for c in "${creds[@]}"; do
        local name=${c%%:*} file=${c##*:}
        if [[ ! -r "$CRED_DIR/$file" ]]; then
            echo "error: missing credential file $CRED_DIR/$file (needed by $agent)" >&2
            return 1
        fi
        cred_opts+=(-p "LoadCredential=$name:$CRED_DIR/$file")
    done

    echo "==> Running $agent ${args[*]:-}"
    systemd-run --wait --pty --collect \
        --unit "manual-$agent-$$" \
        "${cred_opts[@]}" \
        "$BIN_DIR/$agent" "${args[@]}"
}

if [[ $# -eq 0 ]]; then
    for agent in orchestrator iso-agent xoa-vm-agent; do
        run_agent "$agent"
    done
else
    agent=$1; shift
    run_agent "$agent" "$@"
fi
