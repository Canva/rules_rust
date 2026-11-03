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

//! One process wrapper run as Bazel hands it to this binary: the action's command line, with the
//! flags this binary consumes picked out and the rest kept in order for the process wrapper.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a worker keeps rustc's incremental state, under Bazel's output base.
pub(crate) const INCREMENTAL_DIR_NAME: &str = "rustc-incremental";

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Invocation {
    /// `--process-wrapper <path>`: the process wrapper to run. A startup argument.
    pub(crate) process_wrapper: Option<String>,
    /// `--env KEY=VALUE`, repeated: the per-crate environment (`CARGO_CRATE_NAME` and friends).
    /// The rules put it in the request rather than in the action's environment, which is part of
    /// the key Bazel pools workers by, so that every crate shares one pool. It becomes the process
    /// wrapper's environment, which the wrapper passes on to rustc.
    pub(crate) env: Vec<(String, String)>,
    /// `--rustc-incremental true`: compile with rustc's incremental compilation when a worker,
    /// which has state to keep, runs this. Off when the request also stops rustc at the rmeta
    /// (`--rustc-quit-on-rmeta true`): an interrupted session leaves nothing to reuse.
    pub(crate) rustc_incremental: bool,
    /// Everything else, in order: the process wrapper's flags, `--`, rustc and its arguments.
    pub(crate) arguments: Vec<String>,
}

impl Invocation {
    /// Parses an argument list (startup arguments, a work request, or an expanded one-shot
    /// command line). The flags this binary consumes are taken before `--`; everything else,
    /// and everything from `--` on, passes through.
    pub(crate) fn parse(argv: Vec<String>) -> Result<Self, String> {
        let mut invocation = Self::default();
        let mut quit_on_rmeta = false;
        let mut args = argv.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                invocation.arguments.push(arg);
                invocation.arguments.extend(args);
                break;
            }
            // `--flag value` and `--flag=value` alike.
            let (flag, inline_value) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_owned(), Some(value.to_owned()))
                }
                _ => (arg.clone(), None),
            };
            let separate = inline_value.is_none();
            let mut value = || {
                inline_value
                    .clone()
                    .or_else(|| args.next())
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--process-wrapper" => invocation.process_wrapper = Some(value()?),
                "--env" => {
                    let pair = value()?;
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("--env expects KEY=VALUE, got '{pair}'"))?;
                    invocation.env.push((key.to_owned(), value.to_owned()));
                }
                "--rustc-incremental" => invocation.rustc_incremental = value()? == "true",
                "--rustc-quit-on-rmeta" => {
                    // The process wrapper's flag; only noted here.
                    let value = value()?;
                    quit_on_rmeta = value == "true";
                    if separate {
                        invocation.arguments.push(flag);
                        invocation.arguments.push(value);
                    } else {
                        invocation.arguments.push(arg);
                    }
                }
                _ => invocation.arguments.push(arg),
            }
        }
        if quit_on_rmeta {
            invocation.rustc_incremental = false;
        }
        Ok(invocation)
    }

    /// A request as the worker runs it: the startup arguments first (the process wrapper and
    /// whatever else Bazel started the worker with), then the request's.
    pub(crate) fn with_startup(startup: &Invocation, request: Invocation) -> Invocation {
        Invocation {
            process_wrapper: request
                .process_wrapper
                .or_else(|| startup.process_wrapper.clone()),
            env: [startup.env.clone(), request.env].concat(),
            rustc_incremental: request.rustc_incremental,
            arguments: [startup.arguments.clone(), request.arguments].concat(),
        }
    }

    /// The process wrapper command for this invocation. With `incremental_base` (a worker's
    /// `<output base>/rustc-incremental`) and `--rustc-incremental true`, rustc gets
    /// `-Cincremental=<incremental_base>/<configuration>`: the configuration segment of its
    /// `--out-dir` (`bazel-out/<configuration>/bin/...`), so that the same crate built in two
    /// configurations (target and exec) does not keep invalidating one session.
    pub(crate) fn command(&self, incremental_base: Option<&Path>) -> Result<Command, String> {
        let process_wrapper = self
            .process_wrapper
            .as_deref()
            .ok_or("--process-wrapper <path> is required")?;
        let mut command = Command::new(process_wrapper);
        command.args(&self.arguments);
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        if self.rustc_incremental {
            if let Some(base) = incremental_base {
                command.arg(format!(
                    "-Cincremental={}",
                    base.join(self.configuration()).display()
                ));
            }
        }
        Ok(command)
    }

    fn configuration(&self) -> &str {
        let mut rustc_args = self.arguments.iter().skip_while(|arg| *arg != "--").skip(1);
        let mut out_dir = None;
        while let Some(arg) = rustc_args.next() {
            if let Some(value) = arg.strip_prefix("--out-dir=") {
                out_dir = Some(value);
            } else if arg == "--out-dir" {
                out_dir = rustc_args.next().map(String::as_str);
            }
        }
        out_dir
            .and_then(|dir| {
                let mut segments = dir.split('/');
                match (segments.next(), segments.next()) {
                    (Some("bazel-out"), Some(configuration)) if !configuration.is_empty() => {
                        Some(configuration)
                    }
                    _ => None,
                }
            })
            .unwrap_or("default")
    }
}

/// `<output base>/rustc-incremental` for a worker whose working directory is inside Bazel's
/// output base: the execution root, `<output base>/execroot/<workspace>`, or a sandboxed worker's
/// copy of it, `<output base>/bazel-workers/<worker>/<workspace>` (dynamic execution sandboxes its
/// local workers). The output base is the nearest ancestor that holds an `execroot` directory;
/// `None` when there is none.
pub(crate) fn incremental_base(current_dir: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(current_dir).ok()?;
    canonical
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("execroot").is_dir())
        .map(|output_base| output_base.join(INCREMENTAL_DIR_NAME))
}

/// Replaces every `--flagfile=<path>` argument before `--` with the lines of that file (empty
/// lines dropped), the way Bazel expands a worker request's `@` files. Bazel treats `--flagfile=`
/// arguments as part of the request but does not expand them, and expands nothing for a one-shot
/// run. Already expanded arguments pass through unchanged.
pub(crate) fn expand_flagfiles(argv: Vec<String>) -> Result<Vec<String>, String> {
    let mut expanded = Vec::with_capacity(argv.len());
    let mut past_separator = false;
    for arg in argv {
        if past_separator {
            expanded.push(arg);
            continue;
        }
        if arg == "--" {
            past_separator = true;
            expanded.push(arg);
        } else if let Some(path) = arg.strip_prefix("--flagfile=") {
            let content = std::fs::read_to_string(path).map_err(|err| {
                format!(
                    "{} reading flag file {:?} (current directory: {:?})",
                    err,
                    path,
                    std::env::current_dir()
                )
            })?;
            for line in content.lines() {
                if line == "--" {
                    past_separator = true;
                }
                if !line.is_empty() {
                    expanded.push(line.to_owned());
                }
            }
        } else {
            expanded.push(arg);
        }
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn command_line(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_its_flags_and_passes_the_rest_through() {
        let invocation = Invocation::parse(owned(&[
            "--process-wrapper=bin/process_wrapper",
            "--env",
            "CARGO_CRATE_NAME=foo",
            "--env=CARGO_PKG_VERSION=1.2.3",
            "--subst",
            "pwd=${pwd}",
            "--rustc-incremental",
            "true",
            "--touch-file",
            "out/marker",
            "--",
            "rustc",
            "--env",
            "not-ours-after-the-separator",
        ]))
        .unwrap();
        assert_eq!(
            invocation,
            Invocation {
                process_wrapper: Some("bin/process_wrapper".to_owned()),
                env: vec![
                    ("CARGO_CRATE_NAME".to_owned(), "foo".to_owned()),
                    ("CARGO_PKG_VERSION".to_owned(), "1.2.3".to_owned()),
                ],
                rustc_incremental: true,
                arguments: owned(&[
                    "--subst",
                    "pwd=${pwd}",
                    "--touch-file",
                    "out/marker",
                    "--",
                    "rustc",
                    "--env",
                    "not-ours-after-the-separator",
                ]),
            }
        );
    }

    #[test]
    fn env_values_may_contain_equals_signs_and_be_empty() {
        let invocation =
            Invocation::parse(owned(&["--env", "B=x=y", "--env", "C=", "--", "rustc"])).unwrap();
        assert_eq!(
            invocation.env,
            [
                ("B".to_owned(), "x=y".to_owned()),
                ("C".to_owned(), String::new())
            ]
        );
        assert!(Invocation::parse(owned(&["--env", "novalue"])).is_err());
        assert!(Invocation::parse(owned(&["--env"])).is_err());
        assert!(Invocation::parse(owned(&["--process-wrapper"])).is_err());
    }

    #[test]
    fn stopping_at_the_rmeta_turns_incremental_off() {
        let invocation = Invocation::parse(owned(&[
            "--rustc-incremental",
            "true",
            "--rustc-quit-on-rmeta",
            "true",
            "--rustc-output-format",
            "rendered",
            "--",
            "rustc",
        ]))
        .unwrap();
        assert!(!invocation.rustc_incremental);
        // The process wrapper's flag still reaches it, as given.
        assert_eq!(
            invocation.arguments,
            owned(&[
                "--rustc-quit-on-rmeta",
                "true",
                "--rustc-output-format",
                "rendered",
                "--",
                "rustc"
            ])
        );
        let inline = Invocation::parse(owned(&["--rustc-quit-on-rmeta=false", "--"])).unwrap();
        assert_eq!(
            inline.arguments,
            owned(&["--rustc-quit-on-rmeta=false", "--"])
        );
        let off = Invocation::parse(owned(&["--rustc-incremental", "false", "--"])).unwrap();
        assert!(!off.rustc_incremental);
    }

    #[test]
    fn startup_arguments_come_first() {
        let startup =
            Invocation::parse(owned(&["--process-wrapper", "pw", "--env", "A=1"])).unwrap();
        let request = Invocation::parse(owned(&[
            "--env",
            "B=2",
            "--rustc-incremental",
            "true",
            "--",
            "rustc",
        ]))
        .unwrap();
        let merged = Invocation::with_startup(&startup, request);
        assert_eq!(merged.process_wrapper.as_deref(), Some("pw"));
        assert_eq!(
            merged.env,
            [
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
        assert!(merged.rustc_incremental);
        assert_eq!(merged.arguments, owned(&["--", "rustc"]));
    }

    #[test]
    fn command_runs_the_process_wrapper_with_the_environment_and_incremental_state() {
        let invocation = Invocation::parse(owned(&[
            "--process-wrapper",
            "bin/process_wrapper",
            "--env",
            "CARGO_CRATE_NAME=foo",
            "--rustc-incremental",
            "true",
            "--",
            "rustc",
            "--crate-name=foo",
            "--out-dir",
            "bazel-out/k8-fastbuild-ST-1234/bin/tools/foo",
        ]))
        .unwrap();
        let command = invocation.command(Some(Path::new("/base"))).unwrap();
        assert_eq!(
            command_line(&command),
            owned(&[
                "bin/process_wrapper",
                "--",
                "rustc",
                "--crate-name=foo",
                "--out-dir",
                "bazel-out/k8-fastbuild-ST-1234/bin/tools/foo",
                "-Cincremental=/base/k8-fastbuild-ST-1234",
            ])
        );
        let env: Vec<_> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            env,
            [("CARGO_CRATE_NAME".to_owned(), Some("foo".to_owned()))]
        );

        // One-shot: no state to keep, the flag is inert.
        let command = invocation.command(None).unwrap();
        assert!(!command_line(&command)
            .iter()
            .any(|arg| arg.starts_with("-Cincremental=")));

        // Without the flag nothing is added.
        let plain = Invocation::parse(owned(&["--process-wrapper", "pw", "--", "rustc"])).unwrap();
        let command = plain.command(Some(Path::new("/base"))).unwrap();
        assert_eq!(command_line(&command), owned(&["pw", "--", "rustc"]));

        assert!(Invocation::parse(owned(&["--", "rustc"]))
            .unwrap()
            .command(None)
            .is_err());
    }

    #[test]
    fn configuration_comes_from_rustc_s_out_dir() {
        let configuration = |args: &[&str]| {
            Invocation::parse(owned(args))
                .unwrap()
                .configuration()
                .to_owned()
        };
        assert_eq!(
            configuration(&["--", "rustc", "--out-dir=bazel-out/k8-opt-exec/bin/x"]),
            "k8-opt-exec"
        );
        assert_eq!(
            configuration(&["--", "rustc", "--out-dir", "bazel-out/k8-fastbuild/bin/x"]),
            "k8-fastbuild"
        );
        assert_eq!(
            configuration(&["--", "rustc", "--out-dir=/somewhere/else"]),
            "default"
        );
        assert_eq!(configuration(&["--", "rustc"]), "default");
        // The process wrapper's own --out-dir (a build script's OUT_DIR) is not rustc's.
        assert_eq!(
            configuration(&[
                "--out-dir",
                "bazel-out/k8-opt/bin/build_script.out_dir",
                "--",
                "rustc"
            ]),
            "default"
        );
    }

    #[test]
    fn incremental_base_is_under_the_output_base() {
        let base =
            std::env::temp_dir().join(format!("process_wrapper_worker_{}", std::process::id()));
        let execroot = base.join("execroot").join("_main");
        let sandbox = base
            .join("bazel-workers")
            .join("worker-3-Rustc")
            .join("_main");
        std::fs::create_dir_all(&execroot).unwrap();
        std::fs::create_dir_all(&sandbox).unwrap();
        let from_execroot = incremental_base(&execroot);
        let from_sandbox = incremental_base(&sandbox);
        let elsewhere = incremental_base(&base);
        std::fs::remove_dir_all(&base).unwrap();
        let expected = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("process_wrapper_worker_{}", std::process::id()))
            .join(INCREMENTAL_DIR_NAME);
        assert_eq!(from_execroot, Some(expected.clone()));
        assert_eq!(from_sandbox, Some(expected));
        // The output base itself is not inside an output base.
        assert_eq!(elsewhere, None);
        assert_eq!(
            incremental_base(Path::new("/nonexistent/execroot/_main")),
            None
        );
    }

    #[test]
    fn expands_flagfiles_before_the_separator() {
        let dir =
            std::env::temp_dir().join(format!("process_wrapper_flagfiles_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first");
        std::fs::write(&first, "--env\nA=1\n\n--subst\npwd=${pwd}\n").unwrap();
        let second = dir.join("second");
        std::fs::write(&second, "--\nrustc\n").unwrap();
        let argv = vec![
            "--process-wrapper=pw".to_owned(),
            format!("--flagfile={}", first.display()),
            "--touch-file".to_owned(),
            "out".to_owned(),
            format!("--flagfile={}", second.display()),
            "--flagfile=not-expanded-after-the-separator".to_owned(),
            "@params".to_owned(),
        ];
        let got = expand_flagfiles(argv).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            got,
            [
                "--process-wrapper=pw",
                "--env",
                "A=1",
                "--subst",
                "pwd=${pwd}",
                "--touch-file",
                "out",
                "--",
                "rustc",
                "--flagfile=not-expanded-after-the-separator",
                "@params",
            ]
        );
        assert!(expand_flagfiles(vec![
            "--flagfile=/nonexistent/process_wrapper/flagfile".to_owned()
        ])
        .is_err());
    }
}
