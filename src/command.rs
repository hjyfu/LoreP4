use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
};

pub fn run(mut cmd: Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    let label = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().with_context(|| format!("starting {label}"))?;
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes)?;
    }
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "{label} failed ({}): {} {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(out.stdout)
}

pub fn lines(bytes: &[u8]) -> Result<Vec<Value>> {
    std::str::from_utf8(bytes)?
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| serde_json::from_str(s).with_context(|| format!("unexpected CLI output: {s}")))
        .collect()
}
