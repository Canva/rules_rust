// Copyright 2026 The Bazel Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Persistent worker mode, for rustc's incremental compilation in local builds.
//!
//! A build opts in with
//!
//! ```text
//! --define=rules_rust_incremental_worker=true
//! --strategy=Rustc=worker,remote
//! ```
//!
//! The define gives the `Rustc` actions of the main repository's crates in non-`opt` builds the
//! `supports-multiplex-workers` and `requires-worker-protocol: json` execution requirements (see
//! `rustc.bzl`), and nothing else: the action is otherwise the one every other build has, and so
//! is its remote cache key. Only the actions that miss the cache run here.
//!
//! Bazel splits the action's command line, `process_wrapper <flags> -- rustc @<params>`, into
//! the worker's startup arguments (everything but the param file, then `--persistent_worker`)
//! and one `WorkRequest` per action holding the param file's lines. For each request the worker
//! writes those lines back to a param file and runs this binary one-shot on
//! `<startup arguments> @<param file>`, plus `-Cincremental=<dir>` when the crate qualifies (see
//! [`incremental_dir`]): nothing of the process wrapper's own behaviour changes. The state lives
//! under `bazel-out/_rustc_worker/`, so `bazel clean` resets it.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use tinyjson::JsonValue;

const PERSISTENT_WORKER_FLAG: &str = "--persistent_worker";

/// Under the execution root's `bazel-out`, so that `bazel clean` removes it.
const STATE_DIR: &str = "_rustc_worker";

/// Whether Bazel started this process as a persistent worker.
pub(crate) fn requested() -> bool {
    std::env::args().any(|arg| arg == PERSISTENT_WORKER_FLAG)
}

/// Serves work requests until Bazel closes stdin.
pub(crate) fn main() -> ! {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match serve(startup_arguments(&argv)) {
        Ok(()) => exit(0),
        Err(err) => {
            eprintln!("process wrapper worker: {err}");
            exit(1)
        }
    }
}

/// Everything Bazel started the worker with before `--persistent_worker`: the process wrapper's
/// flags, `--` and the compiler. `--worker_extra_flag` values come after it and are ignored.
fn startup_arguments(argv: &[String]) -> Vec<String> {
    argv.iter()
        .take_while(|arg| *arg != PERSISTENT_WORKER_FLAG)
        .cloned()
        .collect()
}

struct Worker {
    /// This binary, run one-shot for each request.
    program: PathBuf,
    startup: Vec<String>,
    /// `<execution root>/bazel-out/_rustc_worker`, or None outside a Bazel output base.
    state: Option<PathBuf>,
    next_params: AtomicU64,
}

fn serve(startup: Vec<String>) -> io::Result<()> {
    let state = state_dir(&std::env::current_dir()?);
    match &state {
        // Goes to the worker's log, under `<output base>/bazel-workers/`.
        Some(dir) => eprintln!("process wrapper worker: state in {}", dir.display()),
        None => {
            eprintln!("process wrapper worker: not in a Bazel output base, no incremental state")
        }
    }
    let worker = Arc::new(Worker {
        program: std::env::current_exe()?,
        startup,
        state,
        next_params: AtomicU64::new(0),
    });
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let mut stdin = BufReader::new(io::stdin());
    while let Some(request) = read_request(&mut stdin)? {
        if request.cancel {
            // Not advertised (no `supports-worker-cancellation`), so Bazel sends none.
            continue;
        }
        // A multiplex worker gets the next request before it answers this one.
        let worker = Arc::clone(&worker);
        let stdout = Arc::clone(&stdout);
        thread::spawn(move || {
            let response = response_line(&worker.run(request));
            let mut stdout = stdout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(err) = stdout
                .write_all(response.as_bytes())
                .and_then(|()| stdout.flush())
            {
                eprintln!("process wrapper worker: writing a response: {err}");
                exit(1);
            }
        });
    }
    Ok(())
}

impl Worker {
    fn run(&self, request: WorkRequest) -> WorkResponse {
        let (exit_code, output) = match self.execute(&request.arguments) {
            Ok(Output {
                status,
                mut stdout,
                stderr,
            }) => {
                stdout.extend(stderr);
                (status.code().unwrap_or(1), stdout)
            }
            Err(err) => (1, format!("process wrapper worker: {err}\n").into_bytes()),
        };
        WorkResponse {
            exit_code,
            output: String::from_utf8_lossy(&output).into_owned(),
            request_id: request.request_id,
        }
    }

    /// One-shot run of the process wrapper on the startup arguments and the request's arguments
    /// (the compiler's, from the action's param file), in a param file again: inline they could
    /// exceed the command line limit.
    fn execute(&self, rustc_args: &[String]) -> io::Result<Output> {
        let params_dir = match &self.state {
            Some(state) => state.join("params"),
            None => std::env::temp_dir(),
        };
        fs::create_dir_all(&params_dir)?;
        let params = params_dir.join(format!(
            "{}-{}.params",
            std::process::id(),
            self.next_params.fetch_add(1, Ordering::Relaxed)
        ));
        let mut content = rustc_args.join("\n");
        content.push('\n');
        fs::write(&params, content)?;

        let mut command = Command::new(&self.program);
        command
            .args(&self.startup)
            .arg(format!("@{}", params.display()));
        if let Some(dir) = self
            .state
            .as_deref()
            .and_then(|state| incremental_dir(state, &self.startup, rustc_args))
        {
            command.arg(format!("-Cincremental={}", dir.display()));
        }
        let output = command.output();

        // The process wrapper writes the substituted copy next to the param file.
        let _ = fs::remove_file(&params);
        let _ = fs::remove_file(format!("{}.expanded", params.display()));
        output
    }
}

/// `<execution root>/bazel-out/_rustc_worker`, for a worker whose working directory is the
/// execution root (`<output base>/execroot/<workspace>`) or a sandboxed copy of it
/// (`<output base>/bazel-workers/<worker>/<workspace>`, as dynamic execution runs a local
/// worker). The output base is the nearest ancestor holding an `execroot` directory.
fn state_dir(current_dir: &Path) -> Option<PathBuf> {
    let workspace = current_dir.file_name()?;
    let output_base = current_dir
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("execroot").is_dir())?;
    Some(
        output_base
            .join("execroot")
            .join(workspace)
            .join("bazel-out")
            .join(STATE_DIR),
    )
}

/// Where rustc keeps the incremental state of a compile, or None to compile without it:
/// `<state>/incremental/<configuration>/<driver>`, with the configuration from the compiler's
/// `--out-dir` (`bazel-out/<configuration>/bin/...`), so that the builds of a crate in two
/// configurations do not invalidate one another, and the driver the file name of the compiler.
///
/// The rules decide which crates come here (`rustc.bzl`). A compile that emits a single object
/// file (the `cc_common.link` layout), uses LTO or asks for its own incremental directory runs
/// without state, as rustc could not combine those with it.
fn incremental_dir(state: &Path, startup: &[String], rustc_args: &[String]) -> Option<PathBuf> {
    let codegen = codegen_options(rustc_args);
    if codegen
        .iter()
        .any(|opt| opt.starts_with("incremental") || is_lto(opt))
    {
        return None;
    }
    if emits_object(rustc_args) {
        return None;
    }
    let out_dir = flag_value(rustc_args, "--out-dir")?;
    let (configuration, _) = out_dir.strip_prefix("bazel-out/")?.split_once('/')?;
    let driver = startup
        .iter()
        .skip_while(|arg| *arg != "--")
        .nth(1)
        .and_then(|path| Path::new(path).file_name())?;
    Some(state.join("incremental").join(configuration).join(driver))
}

/// The value of `<flag>=<value>` or `<flag> <value>`; the last one wins.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut value = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == flag {
            value = args.next().map(String::as_str);
        } else if let Some(inline) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            value = Some(inline);
        }
    }
    value
}

/// The `-C` options: `-C<opt>`, `-C <opt>`, `--codegen=<opt>`, `--codegen <opt>`.
fn codegen_options(args: &[String]) -> Vec<&str> {
    let mut options = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "-C" || arg == "--codegen" {
            options.extend(args.next().map(String::as_str));
        } else if let Some(opt) = arg.strip_prefix("--codegen=") {
            options.push(opt);
        } else if let Some(opt) = arg.strip_prefix("-C") {
            options.push(opt);
        }
    }
    options
}

fn is_lto(codegen_option: &str) -> bool {
    match codegen_option.strip_prefix("lto") {
        Some("") => true,
        Some(value) => !matches!(value.strip_prefix('='), Some("off" | "no" | "n" | "false")),
        None => false,
    }
}

/// `--emit` asks for an object file (`obj`, or `obj=<path>`).
fn emits_object(args: &[String]) -> bool {
    let mut args = args.iter();
    let mut emits = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--emit" {
            emits.extend(args.next().map(String::as_str));
        } else if let Some(kinds) = arg.strip_prefix("--emit=") {
            emits.push(kinds);
        }
    }
    emits
        .iter()
        .flat_map(|kinds| kinds.split(','))
        .any(|kind| kind == "obj" || kind.starts_with("obj="))
}

#[derive(Debug, Default, PartialEq)]
struct WorkRequest {
    arguments: Vec<String>,
    request_id: i32,
    cancel: bool,
}

#[derive(Debug, Default, PartialEq)]
struct WorkResponse {
    exit_code: i32,
    output: String,
    request_id: i32,
}

// Bazel's JSON worker protocol: one message per line, in protobuf's JSON mapping (camelCase field
// names, fields with default values left out, unknown fields ignored).

/// The next request, or None when Bazel closes stdin.
fn read_request(reader: &mut impl BufRead) -> io::Result<Option<WorkRequest>> {
    let mut line = String::new();
    while line.trim().is_empty() {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
    }
    parse_request(&line)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn parse_request(line: &str) -> Result<WorkRequest, String> {
    let value: JsonValue = line
        .trim()
        .parse()
        .map_err(|err| format!("invalid work request: {err}"))?;
    let JsonValue::Object(fields) = value else {
        return Err("work request is not an object".to_owned());
    };
    let mut request = WorkRequest::default();
    match fields.get("arguments") {
        None => {}
        Some(JsonValue::Array(arguments)) => {
            for argument in arguments {
                let JsonValue::String(argument) = argument else {
                    return Err("work request argument is not a string".to_owned());
                };
                request.arguments.push(argument.clone());
            }
        }
        Some(_) => return Err("work request arguments are not an array".to_owned()),
    }
    if let Some(JsonValue::Number(request_id)) = fields.get("requestId") {
        request.request_id = *request_id as i32;
    }
    if let Some(JsonValue::Boolean(cancel)) = fields.get("cancel") {
        request.cancel = *cancel;
    }
    Ok(request)
}

fn response_line(response: &WorkResponse) -> String {
    let mut line = JsonValue::Object(HashMap::from([
        (
            "exitCode".to_owned(),
            JsonValue::Number(f64::from(response.exit_code)),
        ),
        (
            "output".to_owned(),
            JsonValue::String(response.output.clone()),
        ),
        (
            "requestId".to_owned(),
            JsonValue::Number(f64::from(response.request_id)),
        ),
    ]))
    .stringify()
    .expect("a response holds no non-finite number");
    line.push('\n');
    line
}

#[cfg(test)]
mod test {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn incremental_state_per_configuration_and_driver() {
        let argv = owned(&["--", "bin/rustc", "--persistent_worker", "--x=y"]);
        let startup = startup_arguments(&argv);
        assert_eq!(startup, owned(&["--", "bin/rustc"]));
        let dir = |args: &[&str]| incremental_dir(Path::new("/s"), &startup, &owned(args));
        let out_dir = "--out-dir=bazel-out/k8-fastbuild/bin/foo";
        assert_eq!(
            dir(&[out_dir]),
            Some(PathBuf::from("/s/incremental/k8-fastbuild/rustc"))
        );
        // None for LTO, a single object file, a directory of its own, an out-dir elsewhere.
        assert_eq!(dir(&[out_dir, "-Clto=thin"]), None);
        assert_eq!(dir(&[out_dir, "--emit=dep-info,obj=x.o"]), None);
        assert_eq!(dir(&[out_dir, "-C", "incremental=/x"]), None);
        assert_eq!(dir(&["--out-dir=/tmp/foo"]), None);
    }

    #[test]
    fn state_is_under_the_execution_root_s_bazel_out() {
        let base = std::env::temp_dir().join(format!("pw_worker_{}", std::process::id()));
        let execroot = base.join("execroot").join("_main");
        fs::create_dir_all(&execroot).unwrap();
        assert_eq!(
            state_dir(&execroot),
            Some(execroot.join("bazel-out").join(STATE_DIR))
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn json_protocol() {
        // Unknown fields are ignored; a missing requestId is 0.
        let mut stream = concat!(
            r#"{"arguments":["--","rustc"],"inputs":[{"path":"a"}],"requestId":7}"#,
            "\n",
            r#"{"arguments":["x"]}"#,
            "\n",
        )
        .as_bytes();
        let request = read_request(&mut stream).unwrap().unwrap();
        assert_eq!(
            (request.arguments, request.request_id),
            (owned(&["--", "rustc"]), 7)
        );
        assert_eq!(read_request(&mut stream).unwrap().unwrap().request_id, 0);
        assert_eq!(read_request(&mut stream).unwrap(), None);

        // One line, with integers as Bazel reads them (not `-1.0`).
        let line = response_line(&WorkResponse {
            exit_code: -1,
            output: "a\nb".to_owned(),
            request_id: 7,
        });
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains(r#""exitCode":-1"#), "{}", line);
        assert!(line.contains(r#""output":"a\nb""#), "{}", line);
    }
}
