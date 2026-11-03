use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::process::exit;

use crate::flags::{FlagParseError, Flags, ParseOutcome};
use crate::rustc;
use crate::util::*;

#[derive(Debug)]
pub(crate) enum OptionError {
    FlagError(FlagParseError),
    Generic(String),
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::FlagError(e) => write!(f, "error parsing flags: {e}"),
            Self::Generic(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Options {
    // Contains the path to the child executable
    pub(crate) executable: String,
    // Contains arguments for the child process fetched from files.
    pub(crate) child_arguments: Vec<String>,
    // Contains environment variables for the child process fetched from files.
    pub(crate) child_environment: HashMap<String, String>,
    // If set, create the specified file after the child process successfully
    // terminated its execution.
    pub(crate) touch_file: Option<String>,
    // If set to (source, dest) copies the source file to dest.
    pub(crate) copy_output: Option<(String, String)>,
    // If set, redirects the child process stdout to this file.
    pub(crate) stdout_file: Option<String>,
    // If set, redirects the child process stderr to this file.
    pub(crate) stderr_file: Option<String>,
    // If set, also logs all unprocessed output from the rustc output to this file.
    // Meant to be used to get json output out of rustc for tooling usage.
    pub(crate) output_file: Option<String>,
    // If set, it configures rustc to emit an rmeta file and then
    // quit.
    pub(crate) rustc_quit_on_rmeta: bool,
    // This controls the output format of rustc messages.
    pub(crate) rustc_output_format: Option<rustc::ErrorFormat>,
    // If set, rustc compiles incrementally with this directory as `-Cincremental=`. Only set
    // when this is a persistent worker's request (the worker passes the directory) that asked
    // for it (`--rustc-incremental true`) and that does not stop rustc at the rmeta (an
    // interrupted session leaves nothing to reuse).
    pub(crate) rustc_incremental_dir: Option<String>,
}

pub(crate) fn options() -> Result<Options, OptionError> {
    options_from(env::args().collect())
}

pub(crate) fn options_from(argv: Vec<String>) -> Result<Options, OptionError> {
    // Flag files first: in a persistent worker Bazel has expanded them already, one-shot runs
    // expand them here.
    let argv = expand_flagfiles(argv).map_err(OptionError::Generic)?;

    // Process argument list until -- is encountered.
    // Everything after is sent to the child process.
    let mut subst_mapping_raw = None;
    let mut stable_status_file_raw = None;
    let mut volatile_status_file_raw = None;
    let mut env_file_raw = None;
    let mut out_dir_raw = None;
    let mut arg_file_raw = None;
    let mut bin_arg_file_raw = None;
    let mut cargo_bin_name = None;
    let mut touch_file = None;
    let mut copy_output_raw = None;
    let mut stdout_file = None;
    let mut stderr_file = None;
    let mut output_file = None;
    let mut rustc_quit_on_rmeta_raw = None;
    let mut rustc_output_format_raw = None;
    let mut env_raw = None;
    let mut rustc_incremental_raw = None;
    let mut rustc_incremental_dir_raw = None;
    let mut flags = Flags::new();
    let mut require_explicit_unstable_features = None;
    flags.define_repeated_flag("--subst", "", &mut subst_mapping_raw);
    flags.define_repeated_flag(
        "--env",
        "KEY=VALUE environment variable(s) for the child process, applied before --env-file. \
         Lets a persistent worker keep per-action variables out of the action's environment \
         (part of Bazel's worker key) and in the request instead.",
        &mut env_raw,
    );
    flags.define_flag(
        "--rustc-incremental",
        "If `true`, compile incrementally when a persistent worker provides a directory \
         (--rustc-incremental-dir); ignored otherwise.",
        &mut rustc_incremental_raw,
    );
    flags.define_flag(
        "--rustc-incremental-dir",
        "Directory for rustc's incremental compilation state; passed by this wrapper's own \
         persistent worker mode, not by the build rules.",
        &mut rustc_incremental_dir_raw,
    );
    flags.define_flag("--stable-status-file", "", &mut stable_status_file_raw);
    flags.define_flag("--volatile-status-file", "", &mut volatile_status_file_raw);
    flags.define_repeated_flag(
        "--env-file",
        "File(s) containing environment variables to pass to the child process.",
        &mut env_file_raw,
    );
    flags.define_flag(
        "--out-dir",
        "Path to the build script's output directory, exposed to the child \
         process as the `OUT_DIR` environment variable (with the action's \
         working directory prepended). Sourced from a `File`-typed `Args` \
         entry on the rules_rust side so the value is rewritten by Bazel \
         path mapping (`--experimental_output_paths=strip`) when the \
         action advertises `supports-path-mapping`.",
        &mut out_dir_raw,
    );
    flags.define_repeated_flag(
        "--arg-file",
        "File(s) containing command line arguments to pass to the child process.",
        &mut arg_file_raw,
    );
    flags.define_repeated_flag(
        "--bin-arg-file",
        "Files containing BIN=ARG records; an empty BIN applies to every binary.",
        &mut bin_arg_file_raw,
    );
    flags.define_flag(
        "--cargo-bin-name",
        "Exact Cargo binary name used to select arguments from --bin-arg-file.",
        &mut cargo_bin_name,
    );
    flags.define_flag(
        "--touch-file",
        "Create this file after the child process runs successfully.",
        &mut touch_file,
    );
    flags.define_repeated_flag("--copy-output", "", &mut copy_output_raw);
    flags.define_flag(
        "--stdout-file",
        "Redirect subprocess stdout in this file.",
        &mut stdout_file,
    );
    flags.define_flag(
        "--stderr-file",
        "Redirect subprocess stderr in this file.",
        &mut stderr_file,
    );
    flags.define_flag(
        "--output-file",
        "Log all unprocessed subprocess stderr in this file.",
        &mut output_file,
    );
    flags.define_flag(
        "--rustc-quit-on-rmeta",
        "If enabled, this wrapper will terminate rustc after rmeta has been emitted.",
        &mut rustc_quit_on_rmeta_raw,
    );
    flags.define_flag(
        "--rustc-output-format",
        "Controls the rustc output format if --rustc-quit-on-rmeta is set.\n\
        'json' will cause the json output to be output, \
        'rendered' will extract the rendered message and print that.\n\
        Default: `rendered`",
        &mut rustc_output_format_raw,
    );
    flags.define_flag(
        "--require-explicit-unstable-features",
        "If set, an empty -Zallow-features= will be added to the rustc command line whenever no \
         other -Zallow-features= is present in the rustc flags.",
        &mut require_explicit_unstable_features,
    );

    let mut child_args = match flags.parse(argv).map_err(OptionError::FlagError)? {
        ParseOutcome::Help(help) => {
            eprintln!("{help}");
            exit(0);
        }
        ParseOutcome::Parsed(p) => p,
    };
    let current_dir = std::env::current_dir()
        .map_err(|e| OptionError::Generic(format!("failed to get current directory: {e}")))?
        .to_str()
        .ok_or_else(|| OptionError::Generic("current directory not utf-8".to_owned()))?
        .to_owned();
    let output_base = output_base(&current_dir);

    let exec_root = {
        let workspace_name = std::path::Path::new(&current_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("_main");
        format!("{}/execroot/{}", output_base, workspace_name)
    };

    let mut subst_mappings = subst_mapping_raw
        .unwrap_or_default()
        .into_iter()
        .map(|arg| {
            let (key, val) = arg.split_once('=').ok_or_else(|| {
                OptionError::Generic(format!("empty key for substitution '{arg}'"))
            })?;
            let v = if val == "${pwd}" {
                current_dir.as_str()
            } else if val == "${output_base}" {
                output_base.as_str()
            } else if val == "${exec_root}" {
                exec_root.as_str()
            } else {
                val
            }
            .to_owned();
            Ok((key.to_owned(), v))
        })
        .collect::<Result<Vec<(String, String)>, OptionError>>()?;
    if let Some(out_dir) = out_dir_raw.as_deref() {
        // Expose `--out-dir` as an `${out_dir}` substitution token so
        // that env values templated by `cargo_build_script_runner` (which
        // rewrites `$OUT_DIR/...` references in build-script-emitted
        // env vars to `${pwd}/${out_dir}/...`) resolve to the same path
        // we use for `OUT_DIR` itself. Sourcing this from `--out-dir`
        // (a `File`-typed `Args` entry on the rules_rust side) means the
        // value is rewritten by Bazel path mapping to the
        // `bazel-out/cfg/bin/...` prefix when the consuming action
        // advertises `supports-path-mapping`, keeping the env value in
        // sync with where the file is actually materialized.
        subst_mappings.push(("out_dir".to_owned(), out_dir.to_owned()));
    }
    let stable_stamp_mappings =
        stable_status_file_raw.map_or_else(Vec::new, |s| read_stamp_status_to_array(s).unwrap());
    let volatile_stamp_mappings =
        volatile_status_file_raw.map_or_else(Vec::new, |s| read_stamp_status_to_array(s).unwrap());
    let mut environment_file_block = env_from_pairs(env_raw.unwrap_or_default())?;
    environment_file_block.extend(env_from_files(env_file_raw.unwrap_or_default())?);
    if let Some(out_dir) = out_dir_raw.as_deref() {
        // `OUT_DIR` is materialized here (rather than in the action's `env`
        // dict on the rules_rust side) so that the value can flow through
        // a `File`-typed `Args` entry. Bazel only rewrites `Args`-derived
        // argv strings under path mapping, so routing the path through
        // an explicit `--out-dir` arg keeps it correctly mapped to the
        // `bazel-out/cfg/bin/...` prefix when the action opts into
        // `supports-path-mapping`.
        //
        // Inserting into `environment_file_block` (which subsequently
        // overrides the inherited process env) makes this take precedence
        // over any stale `OUT_DIR` Bazel may have set on the action and
        // lets the existing `${pwd}` substitution in `environment_block`
        // handle the working-directory prefix.
        environment_file_block.insert("OUT_DIR".to_owned(), format!("${{pwd}}/{out_dir}"));
    }
    let mut file_arguments = args_from_file(arg_file_raw.unwrap_or_default())?;
    if let Some(paths) = bin_arg_file_raw {
        let name = cargo_bin_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                OptionError::Generic(
                    "--bin-arg-file requires a nonempty --cargo-bin-name".to_owned(),
                )
            })?;
        file_arguments.extend(select_binary_args(args_from_file(paths)?, name)?);
    }
    // Process --copy-output
    let copy_output = copy_output_raw
        .map(|co| {
            if co.len() != 2 {
                return Err(OptionError::Generic(format!(
                    "\"--copy-output\" needs exactly 2 parameters, {} provided",
                    co.len()
                )));
            }
            let copy_source = &co[0];
            let copy_dest = &co[1];
            if copy_source == copy_dest {
                return Err(OptionError::Generic(format!(
                    "\"--copy-output\" source ({copy_source}) and dest ({copy_dest}) need to be different.",
                )));
            }
            Ok((copy_source.to_owned(), copy_dest.to_owned()))
        })
        .transpose()?;

    let rustc_quit_on_rmeta = rustc_quit_on_rmeta_raw.is_some_and(|s| s == "true");
    let rustc_incremental_dir =
        if rustc_incremental_raw.is_some_and(|s| s == "true") && !rustc_quit_on_rmeta {
            rustc_incremental_dir_raw
        } else {
            None
        };
    let rustc_output_format = rustc_output_format_raw
        .map(|v| match v.as_str() {
            "json" => Ok(rustc::ErrorFormat::Json),
            "rendered" => Ok(rustc::ErrorFormat::Rendered),
            _ => Err(OptionError::Generic(format!(
                "invalid --rustc-output-format '{v}'",
            ))),
        })
        .transpose()?;

    // Prepare the environment variables, unifying those read from files with the ones
    // of the current process.
    let vars = environment_block(
        environment_file_block,
        &stable_stamp_mappings,
        &volatile_stamp_mappings,
        &subst_mappings,
    );

    let require_explicit_unstable_features =
        require_explicit_unstable_features.is_some_and(|s| s == "true");

    // Append all the arguments fetched from files to those provided via command line.
    child_args.append(&mut file_arguments);
    let child_args = prepare_args(
        child_args,
        &subst_mappings,
        require_explicit_unstable_features,
        None,
        None,
    )?;
    // Split the executable path from the rest of the arguments.
    let (exec_path, args) = child_args.split_first().ok_or_else(|| {
        OptionError::Generic(
            "at least one argument after -- is required (the child process path)".to_owned(),
        )
    })?;

    Ok(Options {
        executable: exec_path.to_owned(),
        child_arguments: args.to_vec(),
        child_environment: vars,
        touch_file,
        copy_output,
        stdout_file,
        stderr_file,
        output_file,
        rustc_quit_on_rmeta,
        rustc_output_format,
        rustc_incremental_dir,
    })
}

/// The `-Cincremental=` directory for a rustc invocation: `base` plus the configuration segment
/// of the crate's `--out-dir` (`bazel-out/<configuration>/bin/...`), so that the same crate built
/// in two configurations (target and exec) does not share, and keep invalidating, one session.
pub(crate) fn incremental_dir(base: &str, child_arguments: &[String]) -> String {
    let mut out_dir = None;
    let mut args = child_arguments.iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--out-dir=") {
            out_dir = Some(value);
        } else if arg == "--out-dir" {
            out_dir = args.next().map(String::as_str);
        }
    }
    let configuration = out_dir
        .and_then(|dir| {
            let mut segments = dir.split('/');
            match (segments.next(), segments.next()) {
                (Some("bazel-out"), Some(configuration)) if !configuration.is_empty() => {
                    Some(configuration)
                }
                _ => None,
            }
        })
        .unwrap_or("default");
    format!("{base}/{configuration}")
}

fn env_from_pairs(pairs: Vec<String>) -> Result<HashMap<String, String>, OptionError> {
    pairs
        .into_iter()
        .map(|pair| {
            pair.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .ok_or_else(|| {
                    OptionError::Generic(format!("--env expects KEY=VALUE, got '{pair}'"))
                })
        })
        .collect()
}

fn args_from_file(paths: Vec<String>) -> Result<Vec<String>, OptionError> {
    let mut args = vec![];
    for path in paths.iter() {
        let mut lines = read_file_to_array(path).map_err(|err| {
            OptionError::Generic(format!(
                "{} while processing args from file paths: {:?}",
                err, paths
            ))
        })?;
        args.append(&mut lines);
    }
    Ok(args)
}

// Keep the selector out of rustc's arguments and preserve the script's ordering.
// Split only once: linker flags commonly contain additional equals signs.
fn select_binary_args(records: Vec<String>, name: &str) -> Result<Vec<String>, OptionError> {
    let mut args = Vec::new();
    for record in records {
        let (selector, arg) = record.split_once('=').ok_or_else(|| {
            OptionError::Generic("binary argument file requires BIN=ARG records".to_owned())
        })?;
        if selector.is_empty() || selector == name {
            args.push(arg.to_owned());
        }
    }
    Ok(args)
}

fn env_from_files(paths: Vec<String>) -> Result<HashMap<String, String>, OptionError> {
    let mut env_vars = HashMap::new();
    for path in paths.into_iter() {
        let lines = read_file_to_array(&path).map_err(OptionError::Generic)?;
        for line in lines.into_iter() {
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| OptionError::Generic("environment file invalid".to_owned()))?;
            env_vars.insert(k.to_owned(), v.to_owned());
        }
    }
    Ok(env_vars)
}

fn is_allow_features_flag(arg: &str) -> bool {
    arg.starts_with("-Zallow-features=") || arg.starts_with("allow-features=")
}

fn prepare_arg(mut arg: String, subst_mappings: &[(String, String)]) -> String {
    for (f, replace_with) in subst_mappings {
        let from = format!("${{{f}}}");
        arg = arg.replace(&from, replace_with);
    }
    arg
}

/// Apply substitutions to the given param file. Returns true iff any allow-features flags were found.
fn prepare_param_file(
    filename: &str,
    subst_mappings: &[(String, String)],
    read_file: &mut impl FnMut(&str) -> Result<Vec<String>, OptionError>,
    write_to_file: &mut impl FnMut(&str) -> Result<(), OptionError>,
) -> Result<bool, OptionError> {
    fn process_file(
        filename: &str,
        subst_mappings: &[(String, String)],
        read_file: &mut impl FnMut(&str) -> Result<Vec<String>, OptionError>,
        write_to_file: &mut impl FnMut(&str) -> Result<(), OptionError>,
    ) -> Result<bool, OptionError> {
        let mut has_allow_features_flag = false;
        for arg in read_file(filename)? {
            let arg = prepare_arg(arg, subst_mappings);
            has_allow_features_flag |= is_allow_features_flag(&arg);
            if let Some(arg_file) = arg.strip_prefix('@') {
                has_allow_features_flag |=
                    process_file(arg_file, subst_mappings, read_file, write_to_file)?;
            } else {
                write_to_file(&arg)?;
            }
        }
        Ok(has_allow_features_flag)
    }
    let has_allow_features_flag = process_file(filename, subst_mappings, read_file, write_to_file)?;
    Ok(has_allow_features_flag)
}

/// Apply substitutions to the provided arguments, recursing into param files.
#[allow(clippy::type_complexity)]
fn prepare_args(
    args: Vec<String>,
    subst_mappings: &[(String, String)],
    require_explicit_unstable_features: bool,
    read_file: Option<&mut dyn FnMut(&str) -> Result<Vec<String>, OptionError>>,
    mut write_file: Option<&mut dyn FnMut(&str, &str) -> Result<(), OptionError>>,
) -> Result<Vec<String>, OptionError> {
    let mut allowed_features = false;
    let mut processed_args = Vec::<String>::new();

    let mut read_file_wrapper = |s: &str| read_file_to_array(s).map_err(OptionError::Generic);
    let mut read_file = read_file.unwrap_or(&mut read_file_wrapper);

    for arg in args.into_iter() {
        let arg = prepare_arg(arg, subst_mappings);
        if let Some(param_file) = arg.strip_prefix('@') {
            let expanded_file = format!("{param_file}.expanded");
            let format_err = |err: io::Error| {
                OptionError::Generic(format!(
                    "{} writing path: {:?}, current directory: {:?}",
                    err,
                    expanded_file,
                    std::env::current_dir()
                ))
            };

            enum Writer<'f, F: FnMut(&str, &str) -> Result<(), OptionError>> {
                Function(&'f mut F),
                BufWriter(io::BufWriter<File>),
            }
            let mut out = match write_file {
                Some(ref mut f) => Writer::Function(f),
                None => Writer::BufWriter(io::BufWriter::new(
                    File::create(&expanded_file).map_err(format_err)?,
                )),
            };
            let mut write_to_file = |s: &str| -> Result<(), OptionError> {
                match out {
                    Writer::Function(ref mut f) => f(&expanded_file, s),
                    Writer::BufWriter(ref mut bw) => writeln!(bw, "{s}").map_err(format_err),
                }
            };

            // Note that substitutions may also apply to the param file path!
            let (file, allowed) = prepare_param_file(
                param_file,
                subst_mappings,
                &mut read_file,
                &mut write_to_file,
            )
            .map(|af| (format!("@{expanded_file}"), af))?;
            allowed_features |= allowed;
            processed_args.push(file);
        } else {
            allowed_features |= is_allow_features_flag(&arg);
            processed_args.push(arg);
        }
    }
    if !allowed_features && require_explicit_unstable_features {
        processed_args.push("-Zallow-features=".to_string());
    }
    Ok(processed_args)
}

fn environment_block(
    environment_file_block: HashMap<String, String>,
    stable_stamp_mappings: &[(String, String)],
    volatile_stamp_mappings: &[(String, String)],
    subst_mappings: &[(String, String)],
) -> HashMap<String, String> {
    // Taking all environment variables from the current process
    // and sending them down to the child process
    let mut environment_variables: HashMap<String, String> = std::env::vars().collect();
    // Have the last values added take precedence over the first.
    // This is simpler than needing to track duplicates and explicitly override
    // them.
    environment_variables.extend(environment_file_block);
    for (f, replace_with) in &[stable_stamp_mappings, volatile_stamp_mappings].concat() {
        for value in environment_variables.values_mut() {
            let from = format!("{{{f}}}");
            let new = value.replace(from.as_str(), replace_with);
            *value = new;
        }
    }
    for (f, replace_with) in subst_mappings {
        for value in environment_variables.values_mut() {
            let from = format!("${{{f}}}");
            let new = value.replace(from.as_str(), replace_with);
            *value = new;
        }
    }
    environment_variables
}

#[cfg(test)]
mod test {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_incremental_dir_uses_the_configuration() {
        assert_eq!(
            incremental_dir(
                "/base",
                &owned(&[
                    "--crate-name=foo",
                    "--out-dir=bazel-out/k8-fastbuild-ST-1234/bin/tools/foo",
                ])
            ),
            "/base/k8-fastbuild-ST-1234"
        );
        assert_eq!(
            incremental_dir(
                "/base",
                &owned(&["--out-dir", "bazel-out/k8-opt-exec/bin/x"])
            ),
            "/base/k8-opt-exec"
        );
        assert_eq!(
            incremental_dir("/base", &owned(&["--out-dir=/somewhere/else"])),
            "/base/default"
        );
        assert_eq!(incremental_dir("/base", &[]), "/base/default");
    }

    #[test]
    fn test_env_pairs() {
        let env = env_from_pairs(owned(&["A=1", "B=x=y", "C="])).unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "x=y");
        assert_eq!(env["C"], "");
        assert!(env_from_pairs(owned(&["novalue"])).is_err());
    }

    #[test]
    fn test_options_env_and_incremental() {
        let opts = options_from(owned(&[
            "process_wrapper",
            "--env",
            "CARGO_CRATE_NAME=foo",
            "--rustc-incremental",
            "true",
            "--rustc-incremental-dir",
            "/base",
            "--",
            "rustc",
            "--crate-name=foo",
        ]))
        .unwrap();
        assert_eq!(opts.executable, "rustc");
        assert_eq!(opts.child_arguments, ["--crate-name=foo"]);
        assert_eq!(opts.child_environment["CARGO_CRATE_NAME"], "foo");
        assert_eq!(opts.rustc_incremental_dir.as_deref(), Some("/base"));

        // Without the worker's directory the flag is inert (one-shot execution).
        let opts = options_from(owned(&[
            "process_wrapper",
            "--rustc-incremental",
            "true",
            "--",
            "rustc",
        ]))
        .unwrap();
        assert_eq!(opts.rustc_incremental_dir, None);

        // Stopping at the rmeta leaves no session to reuse: no incremental.
        let opts = options_from(owned(&[
            "process_wrapper",
            "--rustc-incremental",
            "true",
            "--rustc-incremental-dir",
            "/base",
            "--rustc-quit-on-rmeta",
            "true",
            "--rustc-output-format",
            "rendered",
            "--",
            "rustc",
        ]))
        .unwrap();
        assert_eq!(opts.rustc_incremental_dir, None);
    }

    // https://github.com/bazelbuild/rules_rust/issues/4299
    #[test]
    fn binary_args_match_exact_names_and_preserve_order() {
        let records = [
            "=-Clink-arg=first",
            "my-bin=-Clink-arg=${pwd}/${out}/file=with=equals",
            "my_bin=-Clink-arg=other",
            "=-Clink-arg=last",
        ];
        let args = select_binary_args(records.map(str::to_owned).to_vec(), "my-bin").unwrap();
        assert_eq!(
            args,
            [
                "-Clink-arg=first",
                "-Clink-arg=${pwd}/${out}/file=with=equals",
                "-Clink-arg=last"
            ]
        );
        let args = prepare_args(
            args,
            &[
                ("pwd".to_owned(), "/exec".to_owned()),
                ("out".to_owned(), "mapped/out".to_owned()),
            ],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(args[1], "-Clink-arg=/exec/mapped/out/file=with=equals");
        assert_eq!(
            select_binary_args(records.map(str::to_owned).to_vec(), "my_bin").unwrap(),
            ["-Clink-arg=first", "-Clink-arg=other", "-Clink-arg=last"]
        );
        assert!(select_binary_args(vec!["invalid".to_owned()], "my-bin").is_err());
    }

    #[test]
    fn test_enforce_allow_features_flag_user_didnt_say() {
        let args = vec!["rustc".to_string()];
        let subst_mappings: Vec<(String, String)> = vec![];
        let args = prepare_args(args, &subst_mappings, true, None, None).unwrap();
        assert_eq!(
            args,
            vec!["rustc".to_string(), "-Zallow-features=".to_string(),]
        );
    }

    #[test]
    fn test_enforce_allow_features_flag_user_requested_something() {
        let args = vec![
            "rustc".to_string(),
            "-Zallow-features=whitespace_instead_of_curly_braces".to_string(),
        ];
        let subst_mappings: Vec<(String, String)> = vec![];
        let args = prepare_args(args, &subst_mappings, true, None, None).unwrap();
        assert_eq!(
            args,
            vec![
                "rustc".to_string(),
                "-Zallow-features=whitespace_instead_of_curly_braces".to_string(),
            ]
        );
    }

    #[test]
    fn test_enforce_allow_features_flag_user_requested_something_in_param_file() {
        let mut written_files = HashMap::<String, String>::new();
        let mut read_files = HashMap::<String, Vec<String>>::new();
        read_files.insert(
            "rustc_params".to_string(),
            vec!["-Zallow-features=whitespace_instead_of_curly_braces".to_string()],
        );

        let mut read_file = |filename: &str| -> Result<Vec<String>, OptionError> {
            read_files
                .get(filename)
                .cloned()
                .ok_or_else(|| OptionError::Generic(format!("file not found: {}", filename)))
        };
        let mut write_file = |filename: &str, content: &str| -> Result<(), OptionError> {
            if let Some(v) = written_files.get_mut(filename) {
                v.push_str(content);
            } else {
                written_files.insert(filename.to_owned(), content.to_owned());
            }
            Ok(())
        };

        let args = vec!["rustc".to_string(), "@rustc_params".to_string()];
        let subst_mappings: Vec<(String, String)> = vec![];

        let args = prepare_args(
            args,
            &subst_mappings,
            true,
            Some(&mut read_file),
            Some(&mut write_file),
        );

        assert_eq!(
            args.unwrap(),
            vec!["rustc".to_string(), "@rustc_params.expanded".to_string(),]
        );

        assert_eq!(
            written_files,
            HashMap::<String, String>::from([(
                "rustc_params.expanded".to_string(),
                "-Zallow-features=whitespace_instead_of_curly_braces".to_string()
            )])
        );
    }
}
