# fish completion for rcmd - drop it in ~/.config/fish/completions/
complete -c rcmd -s e -l edit -r -F -d 'start in the editor on FILE'
complete -c rcmd -s v -l view -r -F -d 'start in the viewer on FILE (- reads stdin)'
complete -c rcmd -s P -l printwd -r -F -d 'write the last directory to FILE on exit'
complete -c rcmd -s S -l skin -x -a 'mc dark bw' -d 'theme'
complete -c rcmd -s b -l nocolor -d 'black and white'
complete -c rcmd -s c -l color -d 'colour'
complete -c rcmd -s C -l colors -x -d 'mc colour spec'
complete -c rcmd -s d -l nomouse -d 'no mouse'
complete -c rcmd -s u -l nosubshell -d 'no persistent subshell'
complete -c rcmd -s U -l subshell -d 'persistent subshell'
complete -c rcmd -s l -l ftplog -r -F -d 'log the FTP dialogue to FILE'
complete -c rcmd -l remote -x -d 'hand the rest of the line to a running rcmd'
complete -c rcmd -l to -x -d 'which running rcmd'
complete -c rcmd -l print-config -d 'print every setting at its default'
complete -c rcmd -l import-mc -x -a '(__fish_complete_directories)' -d 'print a config built from mc files'
complete -c rcmd -s V -l version -d 'print the version'
complete -c rcmd -s h -l help -d 'print the usage'
complete -c rcmd -f -a '(__fish_complete_directories)'
complete -c rcedit -F
complete -c rcview -F
complete -c rcdiff -F
