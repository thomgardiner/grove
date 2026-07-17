use super::Failure;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn run(
    dir: &Path,
    args: &[&str],
    input: Option<&str>,
    operation: &str,
) -> Result<String, Failure> {
    let mut command = Command::new("git");
    command.current_dir(dir).args(args.iter().map(OsStr::new));
    execute(&mut command, input, operation)
}

pub(super) fn run_bytes(
    dir: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    operation: &str,
) -> Result<Vec<u8>, Failure> {
    let mut command = Command::new("git");
    command.current_dir(dir).args(args.iter().map(OsStr::new));
    execute_bytes(&mut command, input, operation)
}

pub(super) fn optional(
    dir: &Path,
    args: &[&str],
    operation: &str,
) -> Result<Option<String>, Failure> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args.iter().map(OsStr::new))
        .output()
        .map_err(|error| io_failure(operation, &error))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map(|value| Some(value.trim().into()))
            .map_err(|error| Failure::Setup(format!("{operation}: non-UTF-8 output: {error}")));
    }
    if output.status.code() == Some(1) && output.stderr.is_empty() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(Failure::classify(
        output.status.code(),
        &format!("{operation}: {}", stderr.trim()),
    ))
}

pub(super) fn execute(
    command: &mut Command,
    input: Option<&str>,
    operation: &str,
) -> Result<String, Failure> {
    let output = execute_bytes(command, input.map(str::as_bytes), operation)?;
    String::from_utf8(output)
        .map_err(|error| Failure::Setup(format!("{operation}: non-UTF-8 output: {error}")))
}

fn execute_bytes(
    command: &mut Command,
    input: Option<&[u8]>,
    operation: &str,
) -> Result<Vec<u8>, Failure> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| io_failure(operation, &error))?;
    if let Some(input) = input {
        let result = child
            .stdin
            .as_mut()
            .ok_or_else(|| Failure::Setup(format!("{operation}: Git stdin was unavailable")))
            .and_then(|stdin| {
                stdin
                    .write_all(input)
                    .map_err(|error| io_failure(operation, &error))
            });
        if let Err(error) = result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| io_failure(operation, &error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = format!("{operation}: {}", stderr.trim());
        return Err(Failure::classify(output.status.code(), &detail));
    }
    Ok(output.stdout)
}

fn io_failure(operation: &str, error: &io::Error) -> Failure {
    let detail = format!("{operation}: {error}");
    if error.kind() == io::ErrorKind::NotFound {
        Failure::Unsupported(detail)
    } else {
        Failure::Setup(detail)
    }
}
