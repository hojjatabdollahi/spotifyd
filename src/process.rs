use crate::error::Error;
use librespot_playback::player::PlayerEvent;
use log::info;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    process::{Command, ExitStatus, Stdio},
};

/// Blocks while provided command is run in a subprocess using the provided
/// shell. If successful, returns the contents of the subprocess's `stdout` as a
/// `String`.
pub(crate) fn run_program(shell: &str, cmd: &str) -> Result<String, Error> {
    info!("Running {:?} using {:?}", cmd, shell);
    let output = Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| Error::subprocess_with_err(shell, cmd, e))?;
    if !output.status.success() {
        let s = std::str::from_utf8(&output.stderr).map_err(|_| Error::subprocess(shell, cmd))?;
        return Err(Error::subprocess_with_str(shell, cmd, s));
    }
    let s = String::from_utf8(output.stdout).map_err(|_| Error::subprocess(shell, cmd))?;
    Ok(s)
}

/// Spawns provided command in a subprocess using the provided shell.
fn spawn_program(shell: &str, cmd: &str, env: HashMap<&str, String>) -> Result<Child, Error> {
    info!(
        "Running {:?} using {:?} with environment variables {:?}",
        cmd, shell, env
    );
    let inner = Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .envs(env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::subprocess_with_err(shell, cmd, e))?;
    let child = Child::new(cmd.to_string(), inner, shell.to_string());
    Ok(child)
}

/// Spawns provided command in a subprocess using the provided shell.
/// Various environment variables are included in the subprocess's environment
/// depending on the `PlayerEvent` that was passed in.
pub(crate) fn spawn_program_on_event(
    shell: &str,
    cmd: &str,
    event: PlayerEvent,
) -> Result<Child, Error> {
    let mut env = HashMap::new();
    match event {
        PlayerEvent::Changed {
            old_track_id,
            new_track_id,
        } => {
            env.insert("OLD_TRACK_ID", old_track_id.to_base62().unwrap());
            env.insert("PLAYER_EVENT", "change".to_string());
            env.insert("TRACK_ID", new_track_id.to_base62().unwrap());
        }
        PlayerEvent::Started {
            track_id,
            play_request_id,
            position_ms,
        } => {
            env.insert("PLAYER_EVENT", "start".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
            env.insert("POSITION_MS", position_ms.to_string());
        }
        PlayerEvent::Stopped {
            track_id,
            play_request_id,
        } => {
            env.insert("PLAYER_EVENT", "stop".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
        }
        PlayerEvent::Loading {
            track_id,
            play_request_id,
            position_ms,
        } => {
            env.insert("PLAYER_EVENT", "load".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
            env.insert("POSITION_MS", position_ms.to_string());
        }
        PlayerEvent::Playing {
            track_id,
            play_request_id,
            position_ms,
            duration_ms,
        } => {
            env.insert("PLAYER_EVENT", "play".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
            env.insert("POSITION_MS", position_ms.to_string());
            env.insert("DURATION_MS", duration_ms.to_string());
        }
        PlayerEvent::Paused {
            track_id,
            play_request_id,
            position_ms,
            duration_ms,
        } => {
            env.insert("PLAYER_EVENT", "pause".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
            env.insert("POSITION_MS", position_ms.to_string());
            env.insert("DURATION_MS", duration_ms.to_string());
        }
        PlayerEvent::TimeToPreloadNextTrack {
            track_id,
            play_request_id,
        } => {
            env.insert("PLAYER_EVENT", "preload".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
        }
        PlayerEvent::EndOfTrack {
            track_id,
            play_request_id,
        } => {
            env.insert("PLAYER_EVENT", "endoftrack".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
        }
        PlayerEvent::VolumeSet { volume } => {
            env.insert("PLAYER_EVENT", "volumeset".to_string());
            env.insert("VOLUME", volume.to_string());
        }
        PlayerEvent::Unavailable {
            play_request_id,
            track_id,
        } => {
            env.insert("PLAYER_EVENT", "unavailable".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
            env.insert("PLAY_REQUEST_ID", play_request_id.to_string());
        }
        PlayerEvent::Preloading { track_id } => {
            env.insert("PLAYER_EVENT", "preloading".to_string());
            env.insert("TRACK_ID", track_id.to_base62().unwrap());
        }
    }
    spawn_program(shell, cmd, env)
}

/// Same as a `std::process::Child` except when this `Child` exits:
/// * successfully: It writes the contents of it's stdout to the stdout of the
///   main process.
/// * unsuccesfully: It returns an error that includes the contents it's stderr
///   as well as information on the command that was run and the shell that
///   invoked it.
#[derive(Debug)]
pub(crate) struct Child {
    cmd: String,
    inner: std::process::Child,
    shell: String,
}

impl Child {
    pub(crate) fn new(cmd: String, child: std::process::Child, shell: String) -> Self {
        Self {
            cmd,
            inner: child,
            shell,
        }
    }

    #[allow(unused)]
    pub(crate) fn wait(&mut self) -> Result<(), Error> {
        match self.inner.wait() {
            Ok(status) => {
                self.write_output(status)?;
                Ok(())
            }
            Err(e) => Err(Error::subprocess_with_err(&self.shell, &self.cmd, e)),
        }
    }

    #[allow(unused)]
    pub(crate) fn try_wait(&mut self) -> Result<Option<()>, Error> {
        match self.inner.try_wait() {
            Ok(Some(status)) => {
                self.write_output(status)?;
                Ok(Some(()))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::subprocess_with_err(&self.shell, &self.cmd, e)),
        }
    }

    fn write_output(&mut self, status: ExitStatus) -> Result<(), Error> {
        if status.success() {
            // If successful, write subprocess's stdout to main process's stdout...
            let mut stdout_of_child = self.inner.stdout.as_mut().unwrap();
            let reader = &mut stdout_of_child;

            let stdout_of_main = io::stdout();
            let mut guard = stdout_of_main.lock();
            let writer = &mut guard;

            io::copy(reader, writer)
                .map_err(|e| Error::subprocess_with_err(&self.shell, &self.cmd, e))?;

            writer
                .flush()
                .map_err(|e| Error::subprocess_with_err(&self.shell, &self.cmd, e))?;

            Ok(())
        } else {
            // If unsuccessful, return an error that includes the contents of stderr...
            let mut buf = String::new();
            match self.inner.stderr.as_mut().unwrap().read_to_string(&mut buf) {
                Ok(_nread) => Err(Error::subprocess_with_str(&self.shell, &self.cmd, &buf)),
                Err(e) => Err(Error::subprocess_with_err(&self.shell, &self.cmd, e)),
            }
        }
    }
}

impl std::ops::Deref for Child {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<Child> for std::process::Child {
    fn from(child: Child) -> Self {
        child.inner
    }
}
