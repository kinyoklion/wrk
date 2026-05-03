use std::path::Path;

use anyhow::{Context, Result, anyhow};
use portable_pty::{
    Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system,
};

pub struct Pty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

pub fn spawn(command: &[String], cwd: &Path, rows: u16, cols: u16) -> Result<Pty> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("command is empty"))?;
    let mut cmd = CommandBuilder::new(program);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");

    let pty_system = native_pty_system();
    let PtyPair { master, slave } = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening pty")?;

    let child = slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {program}"))?;
    drop(slave);

    Ok(Pty { master, child })
}
