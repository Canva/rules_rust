// Copyright 2020 The Bazel Authors. All rights reserved.
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

use std::fs::File;
use std::io::{BufRead, BufReader, Read};

/// Bazel's output base for a process running in the execution root `current_dir`
/// (`<output base>/execroot/<workspace>`): the parent of the canonical `external` directory
/// when there is one, else two levels up from the canonical current directory, else
/// `current_dir` itself.
pub(crate) fn output_base(current_dir: &str) -> String {
    let external = std::path::Path::new(current_dir).join("external");
    match std::fs::canonicalize(external) {
        Ok(canonical) => canonical
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or(current_dir)
            .to_owned(),
        Err(_) => match std::fs::canonicalize(current_dir) {
            Ok(canonical) => canonical
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.to_str())
                .unwrap_or(current_dir)
                .to_owned(),
            Err(_) => current_dir.to_owned(),
        },
    }
}

/// Replaces every `--flagfile=<path>` argument before `--` with the lines of that file (empty
/// lines dropped), the way Bazel expands flag files for a persistent worker's request. Lets the
/// same action run one-shot: Bazel only expands flag files when it runs the action in a worker.
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
                    "{} reading flag file: {:?}, current directory: {:?}",
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

pub(crate) fn read_file_to_array(path: &str) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| e.to_string()).map_err(|err| {
        format!(
            "{} reading path: {:?}, current directory: {:?}",
            err,
            path,
            std::env::current_dir()
        )
    })?;

    read_to_array(file)
}

pub(crate) fn read_stamp_status_to_array(path: String) -> Result<Vec<(String, String)>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    stamp_status_to_array(file)
}

fn read_to_array(reader: impl Read) -> Result<Vec<String>, String> {
    let reader = BufReader::new(reader);
    let mut ret = vec![];
    let mut escaped_line = String::new();
    for l in reader.lines() {
        let line = l.map_err(|e| e.to_string())?;
        if line.is_empty() {
            continue;
        }
        // a \ at the end of a line allows us to escape the new line break,
        // \\ yields a single \, so \\\ translates to a single \ and a new line
        // escape
        let end_backslash_count = line.chars().rev().take_while(|&c| c == '\\').count();
        // a 0 or even number of backslashes do not lead to a new line escape
        let escape = end_backslash_count % 2 == 1;
        //  remove backslashes and add back two for every one
        let l = line.trim_end_matches('\\');
        escaped_line.push_str(l);
        for _ in 0..end_backslash_count / 2 {
            escaped_line.push('\\');
        }
        if escape {
            // we add a newline as we expect a line after this
            escaped_line.push('\n');
        } else {
            ret.push(escaped_line);
            escaped_line = String::new();
        }
    }
    Ok(ret)
}

fn stamp_status_to_array(reader: impl Read) -> Result<Vec<(String, String)>, String> {
    let escaped_lines = read_to_array(reader)?;
    escaped_lines
        .into_iter()
        .map(|l| {
            let (s1, s2) = l
                .split_once(' ')
                .ok_or_else(|| format!("wrong workspace status file format for \"{l}\""))?;
            Ok((s1.to_owned(), s2.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_read_to_array() {
        let input = r"some escaped \\\
string
with other lines"
            .to_owned();
        let expected = vec![
            r"some escaped \
string",
            "with other lines",
        ];
        let got = read_to_array(input.as_bytes()).unwrap();
        assert_eq!(expected, got);
    }

    #[test]
    fn test_expand_flagfiles() {
        let dir =
            std::env::temp_dir().join(format!("process_wrapper_flagfiles_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first");
        std::fs::write(&first, "--env\nA=1\n\n--subst\npwd=${pwd}\n").unwrap();
        let second = dir.join("second");
        std::fs::write(&second, "--\nrustc\n").unwrap();
        let argv = vec![
            "process_wrapper".to_owned(),
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
                "process_wrapper",
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
    }

    #[test]
    fn test_expand_flagfiles_missing_file() {
        let argv = vec![
            "process_wrapper".to_owned(),
            "--flagfile=/nonexistent/process_wrapper/flagfile".to_owned(),
        ];
        assert!(expand_flagfiles(argv).is_err());
    }

    #[test]
    fn test_stamp_status_to_array() {
        let lines = "aaa bbb\\\nvvv\nccc ddd\neee fff";
        let got = stamp_status_to_array(lines.as_bytes()).unwrap();
        let expected = vec![
            ("aaa".to_owned(), "bbb\nvvv".to_owned()),
            ("ccc".to_owned(), "ddd".to_owned()),
            ("eee".to_owned(), "fff".to_owned()),
        ];
        assert_eq!(expected, got);
    }
}
