use std::{
    fmt::Display,
    io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tracing::{debug, trace};

#[derive(Debug)]
pub(crate) struct Podman {
    /// Path to the podman binary.
    podman_path: PathBuf,
}

impl Podman {
    /// Creates a new podman handle.
    pub(crate) fn new<P: AsRef<Path>>(podman_path: P) -> Self {
        Self {
            podman_path: podman_path.as_ref().into(),
        }
    }

    pub(crate) fn inspect(&self, container: &str) -> Result<serde_json::Value, CommandError> {
        let mut cmd = self.mk_podman_command();
        cmd.arg("inspect");
        cmd.arg(container);
        cmd.args(["--format", "json"]);
        fetch_json(cmd)
    }

    pub(crate) fn ps(&self, all: bool) -> Result<serde_json::Value, CommandError> {
        let mut cmd = self.mk_podman_command();
        cmd.arg("ps");

        if all {
            cmd.arg("--all");
        }

        cmd.args(["--format", "json"]);

        fetch_json(cmd)
    }

    pub(crate) fn run(&self, image_url: &str) -> StartCommand {
        StartCommand {
            podman: &self,
            image_url: image_url.to_owned(),
            rm: false,
            name: None,
            rmi: false,
            tls_verify: true,
            env: Vec::new(),
            publish: Vec::new(),
        }
    }

    pub(crate) fn rm(&self, container: &str, force: bool) -> Result<Output, CommandError> {
        let mut cmd = self.mk_podman_command();

        cmd.arg("rm");

        if force {
            cmd.arg("--force");
        }

        cmd.arg(container);

        checked_output(cmd)
    }

    fn mk_podman_command(&self) -> Command {
        Command::new(&self.podman_path)
    }
}

pub(crate) struct StartCommand<'a> {
    podman: &'a Podman,
    env: Vec<(String, String)>,
    image_url: String,
    name: Option<String>,
    rm: bool,
    rmi: bool,
    tls_verify: bool,
    publish: Vec<String>,
}

impl<'a> StartCommand<'a> {
    pub fn env<S1: Into<String>, S2: Into<String>>(&mut self, var: S1, value: S2) -> &mut Self {
        self.env.push((var.into(), value.into()));
        self
    }

    #[inline]
    pub fn name<S: Into<String>>(&mut self, name: S) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    #[inline]
    pub fn publish<S: Into<String>>(&mut self, publish: S) -> &mut Self {
        self.publish.push(publish.into());
        self
    }

    #[inline]
    pub(crate) fn rm(&mut self) -> &mut Self {
        self.rm = true;
        self
    }

    #[inline]
    pub(crate) fn rmi(&mut self) -> &mut Self {
        self.rmi = true;
        self
    }

    #[inline]
    pub(crate) fn tls_verify(&mut self, tls_verify: bool) -> &mut Self {
        self.tls_verify = tls_verify;
        self
    }

    #[inline]
    pub(crate) fn execute(&self) -> Result<Output, CommandError> {
        let mut cmd = self.podman.mk_podman_command();

        cmd.arg("run");
        cmd.arg(format!("--tls-verify={}", self.tls_verify));
        cmd.arg("--detach");

        if self.rm {
            cmd.arg("--rm");
        }

        if self.rmi {
            cmd.arg("--rmi");
        }

        if let Some(ref name) = self.name {
            cmd.args(["--name", name.as_str()]);
        }

        for publish in &self.publish {
            cmd.args(["-p", publish.as_str()]);
        }

        for (key, value) in &self.env {
            cmd.args(["-e", &format!("{}={}", key, value)]);
        }

        cmd.arg(&self.image_url);

        checked_output(cmd)
    }
}

#[derive(Debug)]
pub(crate) struct CommandError {
    err: io::Error,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

impl From<io::Error> for CommandError {
    fn from(value: io::Error) -> Self {
        CommandError {
            err: value,
            stdout: None,
            stderr: None,
        }
    }
}

impl Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.err.fmt(f)?;

        if let Some(ref stdout) = self.stdout {
            let text = String::from_utf8_lossy(stdout);
            f.write_str("\nstdout: ")?;
            f.write_str(&text)?;
            f.write_str("\n")?;
        }

        if let Some(ref stderr) = self.stderr {
            let text = String::from_utf8_lossy(stderr);
            f.write_str("\nstderr: ")?;
            f.write_str(&text)?;
            f.write_str("\n")?;
        }

        Ok(())
    }
}

impl std::error::Error for CommandError {}

fn checked_output(mut cmd: Command) -> Result<Output, CommandError> {
    debug!(?cmd, "running command");
    let output = cmd.output()?;

    if !output.status.success() {
        return Err(CommandError {
            err: io::Error::new(io::ErrorKind::Other, "non-zero exit status"),
            stdout: Some(output.stdout),
            stderr: Some(output.stderr),
        });
    }

    Ok(output)
}

fn fetch_json(cmd: Command) -> Result<serde_json::Value, CommandError> {
    let output = checked_output(cmd)?;

    trace!(raw = %String::from_utf8_lossy(&output.stdout), "parsing JSON");

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

    Ok(parsed)
}
