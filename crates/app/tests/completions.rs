//! `dbr completions bash|zsh`: the scripts run in the real shells (when
//! installed), against a `dbr` that lists a few connections.
#![cfg(unix)]

use databrain_app::cli::completions::{BASH, ZSH, script};

/// A folder with a fake `dbr` answering `--connections`, and a data file.
fn fake_dbr() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("dbr");
    std::fs::write(&bin, "#!/bin/sh\n[ \"$1\" = --connections ] && printf 'Archive PG\\nSales MySQL\\nSales PG\\nWarehouse\\n'\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("orders.csv"), "a\n1\n").unwrap();
    dir
}

fn has(shell: &str) -> bool {
    std::process::Command::new(shell).arg("-c").arg("true").status().is_ok_and(|s| s.success())
}

#[test]
fn scripts_for_zsh_and_bash_only() {
    assert_eq!(script("bash"), Some(BASH));
    assert_eq!(script("zsh"), Some(ZSH));
    assert_eq!(script("fish"), None);
    // Every option dbr takes is offered (the lists are kept by hand).
    for opt in [
        "--conn",
        "--inline",
        "--run",
        "--execute",
        "--csv",
        "--json",
        "--tsv",
        "--expanded",
        "--limit",
        "--param",
        "--yes",
        "--connections",
        "--help",
        "--version",
    ] {
        assert!(BASH.contains(&format!(" {opt}")) && ZSH.contains(opt), "{opt}");
    }
}

/// bash (3.2 on macOS too): connection names after -c in any case, quoted
/// for the shell; options after `-`; nothing for an option's value.
#[test]
fn bash_completes_connections_and_options() {
    if !has("bash") {
        return;
    }
    let dir = fake_dbr();
    let script_file = dir.path().join("dbr.bash");
    std::fs::write(&script_file, BASH).unwrap();
    let harness = r#"
source "$1"
t() { COMP_WORDS=("$@"); COMP_CWORD=$((${#COMP_WORDS[@]}-1)); COMPREPLY=(); _dbr; printf '%s=' "$*"; printf '[%s]' "${COMPREPLY[@]}"; echo; }
t dbr -c ""
t dbr -c sa
t dbr -c "Sales\\ P"
t dbr -c '"sales m'
t dbr --conn WARE
t dbr -c zz
t dbr -i --ex
t dbr -c Sales --limit ""
t dbr j
"#;
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(harness)
        .arg("harness")
        .arg(&script_file)
        .current_dir(dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .output()
        .unwrap();
    let got = String::from_utf8(out.stdout).unwrap();
    let want = [
        r"dbr -c =[Archive\ PG][Sales\ MySQL][Sales\ PG][Warehouse]",
        r"dbr -c sa=[Sales\ MySQL][Sales\ PG]",
        r"dbr -c Sales\ P=[Sales\ PG]",
        r#"dbr -c "sales m=[Sales\ MySQL]"#,
        r"dbr --conn WARE=[Warehouse]",
        r"dbr -c zz=[]",
        r"dbr -i --ex=[--execute][--expanded]",
        r"dbr -c Sales --limit =[]",
        r"dbr j=[jobs]",
    ];
    assert_eq!(got.lines().collect::<Vec<_>>(), want, "{}", String::from_utf8_lossy(&out.stderr));
}

/// zsh: the script parses and defines its completion (driving Tab needs a
/// terminal; checked by hand with zpty).
#[test]
fn zsh_script_loads() {
    if !has("zsh") {
        return;
    }
    let dir = fake_dbr();
    let check = r#"autoload -U compinit && compinit -u -D && eval "$(cat "$1")" && (( $+functions[_dbr] && $+functions[_dbr_connections] )) && [[ $_comps[dbr] == _dbr ]] && print ok"#;
    let file = dir.path().join("dbr.zsh");
    std::fs::write(&file, ZSH).unwrap();
    let out = std::process::Command::new("zsh")
        .args(["-f", "-c", check, "check"])
        .arg(&file)
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok", "{}", String::from_utf8_lossy(&out.stderr));
}
