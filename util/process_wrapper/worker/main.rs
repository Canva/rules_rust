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

//! The persistent-worker front end of rules_rust's process wrapper.
//!
//! With `--@rules_rust//rust/settings:experimental_persistent_worker`, the `Rustc` and
//! `RustcMetadata` actions of the main repository's crates run this binary instead of the process
//! wrapper, with the process wrapper as a tool (`--process-wrapper <path>`, the one startup
//! argument) and the rest of their command line in flag files. Either way it is run, it runs the
//! unchanged process wrapper (`//util/process_wrapper`) on the action's arguments, minus the flags
//! it consumes itself (see `invocation.rs`): `--env KEY=VALUE`, the per-crate environment, goes
//! into the process wrapper's environment, which it passes on to rustc; `--rustc-incremental true`
//! asks for rustc's incremental compilation, which only a worker can offer.
//!
//! - Started by Bazel as a persistent worker (`--persistent_worker` appended), it serves
//!   `WorkRequest`s from stdin, one per action, each as one process wrapper run (see `worker.rs`).
//!   Bazel keeps the worker, and so the incremental state the worker has rustc keep under
//!   `<output base>/rustc-incremental/<configuration>/<driver>` (rustc's and clippy-driver's
//!   apart), alive between builds.
//! - Run one-shot (the same action executed remotely, sandboxed or with `--strategy=Rustc=local`),
//!   it replaces itself with the process wrapper. There is no state to keep, so
//!   `--rustc-incremental` is inert.
//!
//! Either way it expands the `--flagfile=` arguments itself: Bazel expands a worker request's `@`
//! files, but leaves `--flagfile=` ones as they are, and does nothing for a one-shot run.
//!
//! The process wrapper is an input of every Rustc action. Leaving it untouched keeps every action
//! that does not go through this binary (all of them without the setting; the crates of external
//! repositories with it) byte-for-byte the action every other build of the same crate has, so the
//! remote cache entries CI and other builds leave still match.

mod invocation;
mod worker;

use std::process::{exit, Command};

use crate::invocation::{expand_flagfiles, Invocation};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Bazel appends `--persistent_worker` to the worker's startup command; what precedes it is
    // the startup arguments, shared by every request.
    if let Some(index) = argv.iter().position(|arg| arg == "--persistent_worker") {
        let startup = expand_flagfiles(argv[..index].to_vec())
            .and_then(Invocation::parse)
            .unwrap_or_else(|error| fail(&error));
        if let Err(error) = worker::run(startup) {
            fail(&error.to_string());
        }
        return;
    }

    let command = expand_flagfiles(argv)
        .and_then(Invocation::parse)
        .and_then(|invocation| invocation.command(None))
        .unwrap_or_else(|error| fail(&error));
    run_one_shot(command)
}

fn fail(message: &str) -> ! {
    eprintln!("process wrapper worker: {message}");
    exit(1)
}

#[cfg(unix)]
fn run_one_shot(mut command: Command) -> ! {
    use std::os::unix::process::CommandExt;

    // Replaces this process: the process wrapper's exit status, stdout and stderr are the
    // action's, with nothing in between.
    let error = command.exec();
    fail(&format!(
        "failed to run {:?}: {error}",
        command.get_program()
    ))
}

#[cfg(not(unix))]
fn run_one_shot(mut command: Command) -> ! {
    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => fail(&format!(
            "failed to run {:?}: {error}",
            command.get_program()
        )),
    }
}
