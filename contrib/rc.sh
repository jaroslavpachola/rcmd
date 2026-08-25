# rcmd shell wrapper for bash / zsh / any POSIX shell - mc's
# mc-wrapper.sh, doing the one thing a program cannot do for itself:
# leave the shell in the directory you were last looking at.
#
#   . /path/to/rc.sh          (or copy the function into ~/.bashrc)
#   rc [rcmd arguments...]
#
# rcmd writes its last active directory to a file on exit (-P); the
# function reads it, removes it and cds. A run that ends in a crash, or
# in a directory that has since gone away, leaves the shell where it was.

rc() {
    RCMD_PWD_FILE="${TMPDIR:-/tmp}/rc-pwd.$$"
    rcmd -P "$RCMD_PWD_FILE" "$@"
    status=$?
    if [ -r "$RCMD_PWD_FILE" ]; then
        RCMD_PWD=$(cat -- "$RCMD_PWD_FILE")
        if [ -n "$RCMD_PWD" ] && [ -d "$RCMD_PWD" ] && [ "$RCMD_PWD" != "$PWD" ]; then
            cd -- "$RCMD_PWD" || true
        fi
        unset RCMD_PWD
    fi
    rm -f -- "$RCMD_PWD_FILE"
    unset RCMD_PWD_FILE
    return $status
}
