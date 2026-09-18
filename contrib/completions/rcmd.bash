# bash completion for rcmd - source it, or drop it in
# ~/.local/share/bash-completion/completions/rcmd
_rcmd() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    case "$prev" in
        -e|--edit|-v|--view|-P|--printwd|-l|--ftplog)
            COMPREPLY=($(compgen -f -- "$cur")); return ;;
        -S|--skin)
            COMPREPLY=($(compgen -W "mc dark bw" -- "$cur")); return ;;
        --import-mc)
            COMPREPLY=($(compgen -d -- "$cur")); return ;;
        --remote|--to|-C|--colors)
            return ;;
    esac
    if [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W "-e --edit -v --view -P --printwd -S --skin
            -b --nocolor -c --color -C --colors -d --nomouse -u --nosubshell
            -U --subshell -l --ftplog --remote --to --print-config
            --import-mc -V --version -h --help" -- "$cur"))
    else
        COMPREPLY=($(compgen -d -- "$cur"))
    fi
}
complete -F _rcmd rcmd
complete -f rcedit rcview rcdiff
