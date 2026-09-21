//! `mcpls backend start`, `stop` and `status`: what each found, worded for
//! the person at the terminal.

use std::path::Path;

use mcpls_core::backend::control::{Found, StartError, Started, Stopped};
use mcpls_core::backend::{HandshakeReply, Refusal, VERSION};

/// What a command prints, and whether it succeeded.
pub struct Outcome {
    pub text: String,
    pub success: bool,
}

impl Outcome {
    const fn ok(text: String) -> Self {
        Self {
            text,
            success: true,
        }
    }

    const fn failed(text: String) -> Self {
        Self {
            text,
            success: false,
        }
    }
}

pub fn status(found: Found, root: &Path, log: &Path) -> Outcome {
    let root = root.display();
    match found {
        Found::Answered(reply) if reply.in_process => Outcome::failed(in_process(&reply, &root)),
        Found::Answered(reply) => Outcome::ok(format!(
            "the backend for {root} is running: {}\nlog: {}\n",
            describe(&reply),
            log.display()
        )),
        Found::Absent => Outcome::failed(format!("the backend for {root} is not running\n")),
        Found::Busy => Outcome::failed(busy(&root)),
    }
}

pub fn start(started: Result<Started, StartError>, root: &Path, log: &Path) -> Outcome {
    let root = root.display();
    let log = log.display();
    match started {
        Ok(Started { reply, .. }) if reply.in_process => Outcome::failed(in_process(&reply, &root)),
        Ok(Started { reply, .. }) if !reply.kept => Outcome::failed(format!(
            "the backend for {root} is running, but its version {} cannot be kept running, so it \
             exits on its idle timer: {}\n",
            reply.version,
            describe(&reply)
        )),
        Ok(Started { reply, spawned }) => Outcome::ok(format!(
            "{} the backend for {root}: {}\nlog: {log}\n",
            if spawned { "started" } else { "kept" },
            describe(&reply)
        )),
        Err(StartError::Busy) => Outcome::failed(busy(&root)),
        Err(error) => Outcome::failed(format!(
            "could not start the backend for {root}: {error}. Its log is {log}.\n"
        )),
    }
}

pub fn stop(stopped: Stopped, root: &Path) -> Outcome {
    let root = root.display();
    match stopped {
        Stopped::NotRunning => Outcome::ok(format!("the backend for {root} is not running\n")),
        Stopped::Busy => Outcome::failed(busy(&root)),
        Stopped::Stopped(reply) => Outcome::ok(format!(
            "stopped the backend for {root} (pid {})\n",
            reply.pid
        )),
        Stopped::Lingering(reply) => Outcome::failed(format!(
            "the backend for {root} (pid {}) agreed to stop but still holds its endpoint\n",
            reply.pid
        )),
        Stopped::Refused(reply) if reply.in_process => Outcome::failed(in_process(&reply, &root)),
        Stopped::Refused(reply) if reply.refusal == Some(Refusal::Attached) => {
            Outcome::failed(format!(
                "the backend for {root} (pid {}) has {} attached, so it keeps running until they \
                 leave and then exits. `mcpls backend stop --force` stops it now.\n",
                reply.pid,
                sessions(reply.sessions)
            ))
        }
        Stopped::Refused(reply) => Outcome::failed(format!(
            "the backend for {root} (pid {}) refused to stop: {:?}\n",
            reply.pid, reply.refusal
        )),
    }
}

fn describe(reply: &HandshakeReply) -> String {
    use std::fmt::Write as _;

    let mut text = format!("pid {}, version {}", reply.pid, reply.version);
    if !reply.same_build() {
        let _ = write!(text, " (this mcpls is {VERSION})");
    }
    let _ = write!(text, ", {} attached", sessions(reply.sessions));
    if reply.kept {
        text.push_str(", kept running until `mcpls backend stop`");
    }
    text
}

fn sessions(count: usize) -> String {
    match count {
        1 => "1 session".to_string(),
        count => format!("{count} sessions"),
    }
}

fn in_process(reply: &HandshakeReply, root: &impl std::fmt::Display) -> String {
    format!(
        "an mcpls serving one session in-process (pid {}) holds the endpoint for {root}, so it \
         has no shared backend. It stops with its session.\n",
        reply.pid
    )
}

fn busy(root: &impl std::fmt::Display) -> String {
    format!("the backend endpoint for {root} stayed busy; try again\n")
}
