#!/usr/bin/env fish
# Fish completion script for Helix editor

complete -c hx -s h -l help -d "Prints help information"
complete -c hx -l tutor -d "Loads the tutorial"
complete -c hx -l health -xa "(__hx_langs_ops)" -d "Checks for errors"
complete -c hx -l health -xka all -d "Prints all diagnostic informations"
complete -c hx -l health -xka all-languages -d "Lists all languages"
complete -c hx -l health -xka languages -d "Lists user configured languages"
complete -c hx -l health -xka clipboard -d "Prints system clipboard provider"
complete -c hx -s g -l grammar -x -a "fetch build" -d "Fetch or build tree-sitter grammars"
complete -c hx -s v -o vv -o vvv -d "Increases logging verbosity"
complete -c hx -s V -l version -d "Prints version information"
complete -c hx -l vsplit -d "Splits all given files vertically"
complete -c hx -l hsplit -d "Splits all given files horizontally"
complete -c hx -s c -l config -r -d "Specifies a file to use for config"
complete -c hx -l log -r -d "Specifies a file to use for logging"
complete -c hx -s w -l working-dir -d "Specify initial working directory" -xa "(__fish_complete_directories)"
complete -c hx -s s -l session -x -d "Join the editor shared under this name"
complete -c hx -l no-session -d "Run a separate editor instead of joining the session"
complete -c hx -l remote -d "Open the files in the session's last used terminal and exit"
complete -c hx -l wait -d "With --remote, exit once none of the files is shown"

function __hx_langs_ops
    hx --health all-languages | tail -n '+2' | string replace -fr '^(\S+) .*' '$1'
end
