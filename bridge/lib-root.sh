# lib-root.sh - sourced (never run) by the bridge/ and crates/cs2-native/scripts/ shell tools:
# where this repo's checkout and the project root are. Python has the same rule in
# bridge/bridge_build.py (`project_root`), Rust in cs2-mcp-common (`DEFAULT_PROJECT_ROOT`).
#
#   cs2mcp_main_checkout <checkout>  the repo's main checkout: for a linked git worktree the main
#                                    worktree (it holds the shared runtime state and builds), else
#                                    <checkout> itself (also without git, or when the main one is
#                                    not this repo)
#   cs2mcp_project_root <checkout>   $CS2_MCP_PROJECT_ROOT, else $CS2DOCKER_ROOT (deprecated
#                                    alias), else the main checkout. The project root holds
#                                    workshop/run/app (the Wine game tree), workshop/run/cache and
#                                    .podman/ (podman's XDG_RUNTIME_DIR and the install locks).
#
# Older layout: a parent directory holding this repo as cs2-mcp/ next to workshop/ - set
# CS2_MCP_PROJECT_ROOT to that directory.

cs2mcp_main_checkout() {
    local checkout="$1" common
    if common="$(git -C "$checkout" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"; then
        common="$(dirname "$common")"
        if [ -f "$common/bridge/install-rs.sh" ]; then
            printf '%s\n' "$common"
            return 0
        fi
    fi
    printf '%s\n' "$checkout"
}

cs2mcp_project_root() {
    if [ -n "${CS2_MCP_PROJECT_ROOT:-}" ]; then
        printf '%s\n' "$CS2_MCP_PROJECT_ROOT"
    elif [ -n "${CS2DOCKER_ROOT:-}" ]; then
        printf '%s\n' "$CS2DOCKER_ROOT"
    else
        cs2mcp_main_checkout "$1"
    fi
}
