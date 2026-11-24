//! `dbr completions <shell>`: a script that teaches the shell to complete
//! `dbr`'s options and, after `-c`, the saved connections' names (read from
//! `dbr --connections` at each Tab, so a connection saved since is there).
//! Files and folders complete as the shell completes them.
//!
//! zsh and bash (3.2, macOS's own, and newer). fish and PowerShell: see
//! docs/TODO.md.

use std::process::ExitCode;

/// zsh: `eval "$(dbr completions zsh)"` in `~/.zshrc`, after `compinit`.
pub const ZSH: &str = r#"# dbr completion for zsh: add to ~/.zshrc, after compinit:
#   eval "$(dbr completions zsh)"
_dbr_connections() {
  local -a names
  names=(${(f)"$(command dbr --connections 2>/dev/null)"})
  # In any case, as -c takes them.
  _wanted connections expl 'saved connection' compadd -M 'm:{[:lower:][:upper:]}={[:upper:][:lower:]}' -a names
}
_dbr() {
  if (( CURRENT == 2 )) && [[ $words[2] == j* ]]; then
    compadd jobs
  fi
  # Each option its own word (dbr takes no -ic), and no `--`.
  _arguments \
    '(-c --conn)'{-c,--conn}'[a saved connection]:connection:_dbr_connections' \
    '(-i --inline)'{-i,--inline}'[print the rows here, not in the app]' \
    '--run[run the .sql files the app opens]' \
    '*'{-e,--execute}'[this text is SQL]:sql: ' \
    '(--json --tsv -x --expanded)--csv[every row as CSV]' \
    '(--csv --tsv -x --expanded)--json[every row as JSON, an object a line]' \
    '(--csv --json -x --expanded)--tsv[every row as TSV]' \
    '(--csv --json --tsv -x --expanded)'{-x,--expanded}'[each row as a record, values whole]' \
    '--limit[at most n rows]:rows: ' \
    '*'{-p,--param}'[a value for :name]:name=value: ' \
    '(-y --yes)'{-y,--yes}'[run on a production connection without asking]' \
    '(- *)--connections[the saved connections'"'"' names]' \
    '(- *)'{-h,--help}'[help]' \
    '(- *)'{-v,-V,--version}'[the version]' \
    '*:SQL, a .sql file or a data file:_files'
}
if (( $+functions[compdef] )); then
  compdef _dbr dbr
else
  print -u2 "dbr completions: load compinit first (autoload -U compinit && compinit), then this line"
fi
"#;

/// bash: `eval "$(dbr completions bash)"` in `~/.bashrc` (or
/// `~/.bash_profile` on macOS).
pub const BASH: &str = r#"# dbr completion for bash: add to ~/.bashrc (~/.bash_profile on macOS):
#   eval "$(dbr completions bash)"
_dbr() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    COMPREPLY=()
    case "$prev" in
        -c|--conn)
            # What was typed, unquoted (Sales\ P, "Sales P, 'Sales P). The
            # names go back as they are: readline quotes them (-o filenames)
            # as the word was begun, after a quote too.
            local typed="${cur//\\/}" n names restore glob
            typed="${typed#[\"\']}"
            names=$(command dbr --connections 2>/dev/null)
            restore=$(shopt -p nocasematch)
            shopt -s nocasematch
            # A name with * or [ in it is a name, not a pattern for files here.
            case $- in *f*) glob= ;; *) glob=1; set -f ;; esac
            local IFS=$'\n'
            for n in $names; do
                [[ "$n" == "$typed"* ]] && COMPREPLY+=("$n")
            done
            [[ -n $glob ]] && set +f
            eval "$restore"
            return 0 ;;
        -e|--execute|--limit|-p|--param)
            return 0 ;;
    esac
    if [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W "-c --conn -i --inline --run -e --execute --csv --json --tsv -x --expanded --limit -p --param -y --yes --connections -h --help -v -V --version" -- "$cur"))
        return 0
    fi
    if (( COMP_CWORD == 1 )) && [[ -n "$cur" && "jobs" == "$cur"* ]]; then
        # `jobs`, and the files that begin so too.
        local IFS=$'\n'
        COMPREPLY=(jobs $(compgen -f -- "$cur"))
    fi
    # Anything else: files and folders (complete -o default). bash 3.2 can't
    # turn that off for one word, so a value nothing matches (an unknown
    # name after -c, or --limit's) lists files too.
}
complete -o default -o filenames -F _dbr dbr
"#;

const USAGE: &str = "usage: dbr completions zsh|bash
  zsh:  add to ~/.zshrc, after compinit:   eval \"$(dbr completions zsh)\"
  bash: add to ~/.bashrc (~/.bash_profile on macOS):   eval \"$(dbr completions bash)\"";

/// The script for `shell`, if dbr has one.
pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(ZSH),
        "bash" => Some(BASH),
        _ => None,
    }
}

/// `dbr completions <shell>`: print its script.
pub fn main(args: &[String]) -> ExitCode {
    match args {
        [shell] if script(shell).is_some() => {
            print!("{}", script(shell).unwrap_or_default());
            ExitCode::SUCCESS
        }
        [h] if h == "-h" || h == "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("dbr: completions for zsh or bash (fish and PowerShell aren't there yet)\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
