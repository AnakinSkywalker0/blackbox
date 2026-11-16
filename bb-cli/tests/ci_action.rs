//! The GitHub Action's script, run for real in `--print` mode (which posts nothing) against
//! saved logs and comment listings. The real posting path needs GitHub, so it is checked by
//! hand instead: see "Checking it for real" in the README.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn script() -> String {
    fwd(&Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(".github").join("blackbox").join("diagnose.sh"))
}

/// A bash that can read the script and has the usual tools. On Windows the first `bash` on
/// PATH is often the WSL launcher, which can't see Windows paths, so every bash on PATH
/// (and Git for Windows' own) is tried.
fn find_bash() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["bash", "bash.exe"] {
                candidates.push(dir.join(name));
            }
        }
    }
    for root in ["C:/Program Files/Git", "D:/Git", "C:/Git"] {
        candidates.push(Path::new(root).join("bin").join("bash.exe"));
    }
    candidates.into_iter().filter(|c| c.is_file()).find(|c| {
        Command::new(c)
            .args(["-c", r#"test -f "$1" && command -v mktemp tr sed tail cut head >/dev/null"#, "_", &script()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bb-ci-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Out {
    code: i32,
    text: String,
}

/// Runs diagnose.sh --print with the given environment on top of a sensible default.
fn run(dir: &Path, log: &str, comments: &str, env: &[(&str, &str)]) -> Out {
    std::fs::write(dir.join("log.txt"), log).unwrap();
    std::fs::write(dir.join("comments.json"), comments).unwrap();
    let mut c = Command::new(find_bash().expect("checked by need_bash"));
    c.arg(script()).arg("--print");
    c.env("BB_BIN", fwd(Path::new(env!("CARGO_BIN_EXE_bb"))))
        .env("BB_LOG_FILE", fwd(&dir.join("log.txt")))
        .env("BB_EXISTING_COMMENTS_FILE", fwd(&dir.join("comments.json")))
        .env("BB_RUN_ID", "123")
        .env("GITHUB_REPOSITORY", "owner/repo")
        .env("WORKFLOW_NAME", "CI")
        .env("CONCLUSION", "failure")
        .env("PR_NUMBERS", "7")
        .env("RUN_URL", "https://github.com/owner/repo/actions/runs/123")
        .env_remove("GH_TOKEN");
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    Out { code: o.status.code().unwrap_or(-1), text: format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)) }
}

const LINKER: &str = "build\tCompile\t2026-11-16T10:00:00Z error: linker `link.exe` not found\n";
const MARKER: &str = "<!-- blackbox-diagnosis:ci -->";

fn bot_comment(id: u64, body: &str) -> String {
    format!(r#"[{{"id":{id},"user":{{"type":"Bot"}},"body":{}}}]"#, serde_json::to_string(body).unwrap())
}

macro_rules! need_bash {
    () => {
        if find_bash().is_none() {
            eprintln!("skipped: no usable bash is available here");
            return;
        }
    };
}

#[test]
fn a_failure_with_a_known_cause_creates_one_comment() {
    need_bash!();
    let d = tmp("create");
    let o = run(&d, LINKER, "[]", &[]);
    assert_eq!(o.code, 0, "{}", o.text);
    assert!(o.text.contains("would CREATE a comment on PR #7"), "{}", o.text);
    assert!(o.text.contains(MARKER) && o.text.contains("The C/C++ linker is missing"), "{}", o.text);
    assert!(o.text.contains("[Failing run](https://github.com/owner/repo/actions/runs/123)"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_re_run_updates_the_existing_comment_instead_of_adding_another() {
    need_bash!();
    let d = tmp("update");
    let o = run(&d, LINKER, &bot_comment(42, &format!("{MARKER}\n### blackbox: old text")), &[]);
    assert_eq!(o.code, 0, "{}", o.text);
    assert!(o.text.contains("would UPDATE comment 42 on PR #7"), "{}", o.text);
    assert!(!o.text.contains("would CREATE"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_identical_diagnosis_is_left_alone() {
    need_bash!();
    let d = tmp("same");
    // First run to learn the exact body, then pretend it was already posted.
    let first = run(&d, LINKER, "[]", &[]);
    let body: String = first.text.lines().filter_map(|l| l.strip_prefix("  | ")).collect::<Vec<_>>().join("\n");
    assert!(body.starts_with(MARKER), "{}", first.text);
    let o = run(&d, LINKER, &bot_comment(9, &body), &[]);
    assert!(o.text.contains("comment 9 is already up to date"), "{}", o.text);
    assert!(!o.text.contains("would UPDATE") && !o.text.contains("would CREATE"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_person_quoting_the_marker_is_never_edited() {
    need_bash!();
    let d = tmp("person");
    let people = format!(r#"[{{"id":5,"user":{{"type":"User"}},"body":{}}}]"#, serde_json::to_string(&format!("{MARKER} look")).unwrap());
    let o = run(&d, LINKER, &people, &[]);
    assert!(o.text.contains("would CREATE"), "{}", o.text);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_run_that_passes_now_updates_the_old_comment_but_never_starts_one() {
    need_bash!();
    let d = tmp("success");
    let o = run(&d, "", &bot_comment(42, &format!("{MARKER}\n### blackbox: why `CI` failed")), &[("CONCLUSION", "success")]);
    assert!(o.text.contains("would UPDATE comment 42") && o.text.contains("this workflow passes now"), "{}", o.text);
    let none = run(&d, "", "[]", &[("CONCLUSION", "success")]);
    assert_eq!(none.code, 0);
    assert!(!none.text.contains("would CREATE") && !none.text.contains("would UPDATE"), "{}", none.text);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn nothing_is_posted_when_there_is_nothing_useful_to_say() {
    need_bash!();
    let d = tmp("quiet");
    let cases: Vec<(&str, &str, Vec<(&str, &str)>)> = vec![
        ("a log blackbox can't read a cause from", "something odd happened\n", vec![]),
        ("an empty log", "", vec![]),
        ("a cancelled run", LINKER, vec![("CONCLUSION", "cancelled")]),
        ("no pull request (a fork, or a push)", LINKER, vec![("PR_NUMBERS", "")]),
        ("a bad pull request number", LINKER, vec![("PR_NUMBERS", "7; rm -rf /")]),
        ("a missing bb", LINKER, vec![("BB_BIN", "/nonexistent/bb")]),
    ];
    for (what, log, env) in cases {
        let o = run(&d, log, "[]", &env);
        assert_eq!(o.code, 0, "{what}: {}", o.text);
        assert!(!o.text.contains("would CREATE") && !o.text.contains("would UPDATE"), "{what}: {}", o.text);
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn if_bb_itself_breaks_the_script_posts_nothing_and_still_succeeds() {
    need_bash!();
    let d = tmp("brokenbb");
    for (what, body) in [("exits 3", "#!/bin/sh\nexit 3\n"), ("prints junk then fails", "#!/bin/sh\necho garbage\nexit 1\n")] {
        let fake = d.join("fake-bb");
        std::fs::write(&fake, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let o = run(&d, LINKER, "[]", &[("BB_BIN", &fwd(&fake))]);
        assert_eq!(o.code, 0, "{what}: {}", o.text);
        assert!(!o.text.contains("would CREATE") && !o.text.contains("would UPDATE"), "{what}: {}", o.text);
    }
    // A comments listing that can't be parsed also means nothing is posted.
    let o = run(&d, LINKER, "{not json", &[]);
    assert_eq!(o.code, 0);
    assert!(!o.text.contains("would CREATE") && !o.text.contains("would UPDATE"), "{}", o.text);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn hostile_log_text_stays_inside_a_code_fence_in_the_comment() {
    need_bash!();
    let d = tmp("hostile");
    let log = "permission denied ```` @everyone <!-- blackbox-diagnosis:ci --> [x](http://evil.example)\n";
    let o = run(&d, log, "[]", &[("WORKFLOW_NAME", "CI `$(touch pwned)` <img>")]);
    assert_eq!(o.code, 0, "{}", o.text);
    assert!(o.text.contains("would CREATE"), "{}", o.text);
    assert!(!d.join("pwned").exists() && !Path::new("pwned").exists(), "the workflow name must never be executed");
    let marker_lines = o.text.lines().filter(|l| l.contains("blackbox-diagnosis:")).count();
    assert!(marker_lines >= 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_action_file_only_passes_inputs_through_environment_variables() {
    // Inputs interpolated straight into `run:` scripts are how workflows get injected.
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("action.yml")).unwrap();
    for line in text.lines().filter(|l| l.contains("${{")) {
        let ok = line.trim_start().starts_with("default:") || line.contains("action_path") || line.trim_start().starts_with("GH_TOKEN") || line.contains("env") || line.trim().ends_with("}}") && line.contains(": ${{");
        assert!(ok, "unexpected expression outside env/default: {line}");
    }
    assert!(text.contains("continue-on-error: true"));
}

fn install_script() -> String {
    script().replace("diagnose.sh", "install.sh")
}

fn run_install(dir: &Path, env: &[(&str, &str)]) -> (i32, String, String) {
    let envfile = dir.join("github_env");
    let _ = std::fs::remove_file(&envfile);
    std::fs::write(&envfile, "").unwrap();
    let mut c = Command::new(find_bash().expect("checked by need_bash"));
    c.arg(install_script()).env("GITHUB_ENV", fwd(&envfile)).env("RUNNER_TEMP", fwd(dir)).env_remove("BB_PATH").env_remove("BB_VERSION");
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)), std::fs::read_to_string(&envfile).unwrap())
}

#[test]
fn the_installer_step_accepts_a_usable_bb_and_refuses_everything_else_without_failing() {
    need_bash!();
    let d = tmp("install");
    let bb = fwd(Path::new(env!("CARGO_BIN_EXE_bb")));
    let (code, _, env) = run_install(&d, &[("BB_PATH", &bb)]);
    assert_eq!(code, 0);
    assert_eq!(env.trim(), format!("BB_BIN={bb}"));

    for (what, vars) in [
        ("a path that isn't there", vec![("BB_PATH", "/nonexistent/bb")]),
        ("a version that is a URL trick", vec![("BB_VERSION", "1.0.0/../../evil")]),
        ("a version that isn't one", vec![("BB_VERSION", "main")]),
    ] {
        let (code, out, env) = run_install(&d, &vars);
        assert_eq!(code, 0, "{what}: {out}");
        assert!(env.trim().is_empty(), "{what}: BB_BIN must stay unset, got {env:?}");
        assert!(out.contains("::warning::blackbox"), "{what}: {out}");
    }
    let _ = std::fs::remove_dir_all(&d);
}
