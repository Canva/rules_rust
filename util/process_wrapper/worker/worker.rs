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

//! The persistent worker loop.
//!
//! Bazel starts `worker <startup arguments> --persistent_worker` once per worker and then streams
//! length-delimited `WorkRequest` protos on stdin, one per `Rustc` action, whose `arguments` are
//! the action's flag file arguments (`--flagfile=` and `@`), the `@` ones expanded. Each request
//! is one run of the process wrapper (see `invocation.rs`), so it behaves exactly like the same
//! action run without a worker, and its exit code and output go back in a `WorkResponse`.
//!
//! What the worker adds is persistence: Bazel keeps the process alive between builds, so rustc's
//! incremental compilation state can be kept under `<output base>/rustc-incremental` and reused
//! by the next request for the same crate. The worker runs in the execution root, or in a
//! sandboxed copy of it under `<output base>/bazel-workers/` (dynamic execution sandboxes its
//! local workers); either way the output base is an ancestor of its working directory.
//!
//! The protocol code is adapted from bazelbuild/rules_rust#4184 (Angus Lees).

use std::convert::TryFrom;
use std::io::{self, BufReader, BufWriter, Read, Write};

use crate::invocation::{expand_flagfiles, incremental_base, Invocation};

const MAX_REQUEST_SIZE: usize = 64 * 1024 * 1024;

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
    was_cancelled: bool,
}

/// Serves work requests until Bazel closes stdin. `startup` is this process's startup arguments
/// (everything before `--persistent_worker`), parsed.
pub(crate) fn run(startup: Invocation) -> io::Result<()> {
    let incremental_base = incremental_base(&std::env::current_dir()?);
    if incremental_base.is_none() {
        // Goes to the worker's log (<output base>/bazel-workers/), not to any build's output.
        eprintln!(
            "process wrapper worker: working directory {} is not inside a Bazel output base, \
             compiling without incremental state",
            std::env::current_dir()?.display()
        );
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some(request) = read_request(&mut reader)? {
        let request_id = request.request_id;
        let response = if request.cancel {
            WorkResponse {
                request_id,
                was_cancelled: true,
                ..WorkResponse::default()
            }
        } else {
            let result = expand_flagfiles(request.arguments)
                .and_then(Invocation::parse)
                .map(|request| Invocation::with_startup(&startup, request))
                .and_then(|invocation| invocation.command(incremental_base.as_deref()))
                .and_then(|mut command| command.output().map_err(|error| error.to_string()));
            match result {
                Ok(output) => {
                    let mut combined = output.stdout;
                    combined.extend(output.stderr);
                    WorkResponse {
                        exit_code: output.status.code().unwrap_or(1),
                        output: String::from_utf8_lossy(&combined).into_owned(),
                        request_id,
                        was_cancelled: false,
                    }
                }
                Err(error) => WorkResponse {
                    exit_code: 1,
                    output: format!("process wrapper worker failed to run the request: {error}\n"),
                    request_id,
                    was_cancelled: false,
                },
            }
        };
        write_response(&mut writer, &response)?;
        writer.flush()?;
    }
    Ok(())
}

fn read_request(reader: &mut impl Read) -> io::Result<Option<WorkRequest>> {
    let Some(length) = read_varint(reader)? else {
        return Ok(None);
    };
    let length = usize::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request is too large"))?;
    if length > MAX_REQUEST_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request exceeds maximum size",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    decode_proto_request(&bytes).map(Some)
}

// WorkRequest: 1 = arguments (repeated string), 2 = inputs (repeated message, skipped),
// 3 = request_id (int32), 4 = cancel (bool), 5 = verbosity (int32), 6 = sandbox_dir (string).
fn decode_proto_request(bytes: &[u8]) -> io::Result<WorkRequest> {
    let mut request = WorkRequest::default();
    let mut offset = 0;
    while offset < bytes.len() {
        let key = read_varint_from_slice(bytes, &mut offset)?;
        let field = key >> 3;
        let wire_type = key & 7;
        match (field, wire_type) {
            (1, 2) => {
                let value = read_length_delimited(bytes, &mut offset)?;
                request.arguments.push(
                    std::str::from_utf8(value)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                        .to_owned(),
                );
            }
            (3, 0) => request.request_id = read_varint_from_slice(bytes, &mut offset)? as i32,
            (4, 0) => request.cancel = read_varint_from_slice(bytes, &mut offset)? != 0,
            _ => skip_proto_field(bytes, &mut offset, wire_type)?,
        }
    }
    Ok(request)
}

// WorkResponse: 1 = exit_code (int32), 2 = output (string), 3 = request_id (int32),
// 4 = was_cancelled (bool).
fn write_response(writer: &mut impl Write, response: &WorkResponse) -> io::Result<()> {
    let mut bytes = Vec::new();
    if response.exit_code != 0 {
        write_proto_key(&mut bytes, 1, 0);
        write_varint(&mut bytes, response.exit_code as u32 as u64)?;
    }
    if !response.output.is_empty() {
        write_proto_key(&mut bytes, 2, 2);
        write_varint(&mut bytes, response.output.len() as u64)?;
        bytes.extend(response.output.as_bytes());
    }
    if response.request_id != 0 {
        write_proto_key(&mut bytes, 3, 0);
        write_varint(&mut bytes, response.request_id as u32 as u64)?;
    }
    if response.was_cancelled {
        write_proto_key(&mut bytes, 4, 0);
        write_varint(&mut bytes, 1)?;
    }
    write_varint(writer, bytes.len() as u64)?;
    writer.write_all(&bytes)
}

fn read_varint(reader: &mut impl Read) -> io::Result<Option<u64>> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let mut byte = [0];
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        }
        if shift == 63 && byte[0] > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protobuf varint overflow",
            ));
        }
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid protobuf varint",
    ))
}

fn read_varint_from_slice(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated varint"))?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protobuf varint overflow",
            ));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid protobuf varint",
    ))
}

fn read_length_delimited<'a>(bytes: &'a [u8], offset: &mut usize) -> io::Result<&'a [u8]> {
    let length = usize::try_from(read_varint_from_slice(bytes, offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "field is too large"))?;
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated field"))?;
    let value = &bytes[*offset..end];
    *offset = end;
    Ok(value)
}

fn skip_proto_field(bytes: &[u8], offset: &mut usize, wire_type: u64) -> io::Result<()> {
    let length = match wire_type {
        0 => {
            read_varint_from_slice(bytes, offset)?;
            return Ok(());
        }
        1 => 8,
        2 => usize::try_from(read_varint_from_slice(bytes, offset)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "field is too large"))?,
        5 => 4,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported protobuf wire type",
            ))
        }
    };
    *offset = offset
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated field"))?;
    Ok(())
}

fn write_proto_key(bytes: &mut Vec<u8>, field: u64, wire_type: u64) {
    write_varint(bytes, (field << 3) | wire_type).expect("writing to a Vec cannot fail");
}

fn write_varint(writer: &mut impl Write, mut value: u64) -> io::Result<()> {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
        if value == 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_encodes_work_messages() {
        // arguments: ["foo"], inputs: [{path: ""(skipped), digest: ...}], request_id: 7
        let request = [
            11, 0x0a, 0x03, b'f', b'o', b'o', 0x12, 0x02, 0x08, 0x01, 0x18, 0x07,
        ];
        let mut request = request.as_slice();
        let request = read_request(&mut request).unwrap().unwrap();
        assert_eq!(request.arguments, ["foo"]);
        assert_eq!(request.request_id, 7);
        assert!(!request.cancel);

        let response = WorkResponse {
            exit_code: 2,
            output: "no".to_owned(),
            request_id: 7,
            was_cancelled: false,
        };
        let mut bytes = Vec::new();
        write_response(&mut bytes, &response).unwrap();
        assert_eq!(bytes, [8, 0x08, 0x02, 0x12, 0x02, b'n', b'o', 0x18, 0x07]);
    }

    #[test]
    fn end_of_stream_ends_the_worker() {
        let mut empty: &[u8] = &[];
        assert!(read_request(&mut empty).unwrap().is_none());
    }

    #[test]
    fn decodes_several_arguments_and_cancel() {
        // arguments: ["--", "rustc"], cancel: true
        let request = [
            13, 0x0a, 0x02, b'-', b'-', 0x0a, 0x05, b'r', b'u', b's', b't', b'c', 0x20, 0x01,
        ];
        let mut request = request.as_slice();
        let request = read_request(&mut request).unwrap().unwrap();
        assert_eq!(request.arguments, ["--", "rustc"]);
        assert!(request.cancel);
    }
}
