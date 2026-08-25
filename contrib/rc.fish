# rcmd shell wrapper for fish - the mc-wrapper trick, doing the one
# thing a program cannot do for itself: leave the shell in the
# directory you were last looking at.
#
#   cp rc.fish ~/.config/fish/functions/rc.fish
#   rc [rcmd arguments...]
#
# rcmd writes its last active directory to a file on exit (-P); the
# function reads it, removes it and cds. A run that ends in a crash, or
# in a directory that has since gone away, leaves the shell where it was.

function rc --description 'rcmd, leaving the shell in its last directory'
    set -l pwd_file (mktemp -t rc-pwd.XXXXXX)
    rcmd -P $pwd_file $argv
    set -l status_code $status
    if test -r $pwd_file
        set -l last (cat $pwd_file)
        if test -n "$last" -a -d "$last" -a "$last" != "$PWD"
            cd $last
        end
    end
    rm -f $pwd_file
    return $status_code
end
