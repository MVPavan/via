//! A minimal libtest-compatible runner for a `harness = false` test binary:
//! `--list` (with `--ignored`) prints `name: test` lines, and a run selects
//! cases by name (`--exact` or substring), skipping ignored ones unless
//! `--ignored` or `--include-ignored` is given. Nextest runs one case per
//! process; a run of several cases re-executes this binary once per case,
//! so every case gets a fresh process (failpoint activation and the
//! subreaper flag are per process).

use std::{ffi::OsString, process::ExitCode};

/// One test case.
pub(crate) struct Case {
    /// The case's name.
    pub(crate) name: &'static str,
    /// The case body; a panic fails it.
    pub(crate) run: fn(),
    /// Why the case is ignored by default, if it is.
    pub(crate) ignored: Option<&'static str>,
}

/// Runs the cases `args` (without the program name) select.
pub(crate) fn main(args: &[OsString], cases: &[Case]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut filters = Vec::new();
    let mut flags = Vec::new();
    let mut values = args.iter();
    while let Some(arg) = values.next() {
        match arg.as_str() {
            "--format" | "--test-threads" | "--color" | "--skip" | "--logfile" | "-Z" => {
                values.next();
            }
            flag if flag.starts_with('-') => flags.push(flag.to_owned()),
            filter => filters.push(filter.to_owned()),
        }
    }
    let has = |flag: &str| flags.iter().any(|candidate| candidate == flag);
    let exact = has("--exact");
    let only_ignored = has("--ignored");
    let include_ignored = has("--include-ignored");
    let named = |case: &&Case| {
        filters.is_empty()
            || filters.iter().any(|filter| {
                if exact {
                    case.name == filter
                } else {
                    case.name.contains(filter.as_str())
                }
            })
    };
    if has("--list") {
        for case in cases.iter().filter(named) {
            if !only_ignored || case.ignored.is_some() {
                println!("{}: test", case.name);
            }
        }
        return ExitCode::SUCCESS;
    }
    let selected: Vec<&Case> = cases
        .iter()
        .filter(named)
        .filter(|case| {
            if only_ignored {
                case.ignored.is_some()
            } else {
                include_ignored || case.ignored.is_none()
            }
        })
        .collect();
    let skipped = cases.iter().filter(named).count() - selected.len();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    if let [case] = selected.as_slice() {
        if std::panic::catch_unwind(case.run).is_err() {
            failed.push(case.name);
        }
        println!(
            "test {} ... {}",
            case.name,
            if failed.is_empty() { "ok" } else { "FAILED" }
        );
    } else {
        let exe = std::env::current_exe().expect("test binary path");
        for case in &selected {
            let mut command = std::process::Command::new(&exe);
            command.args(["--exact", case.name, "--nocapture"]);
            if case.ignored.is_some() {
                command.arg("--ignored");
            }
            let passed = command.status().is_ok_and(|status| status.success());
            if !passed {
                failed.push(case.name);
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed; {skipped} ignored",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        for name in failed {
            println!("    {name}");
        }
        ExitCode::from(101)
    }
}
