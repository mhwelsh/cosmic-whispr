// SPDX-License-Identifier: MPL-2.0

//! Puts a transcript on the clipboard.
//!
//! Not through iced's clipboard, which is the obvious choice and does not
//! work here. Setting a Wayland selection through `wl_data_device` requires
//! the serial of a recent input event on one of your own surfaces, and an
//! applet driven by a global keyboard shortcut never receives one: the
//! compositor handles the shortcut and the applet is never focused. The
//! serial is zero, the compositor drops the request, and — worse — nothing
//! reports a failure, so the popup cheerfully says "Copied to clipboard"
//! while the clipboard still holds whatever it held before.
//!
//! The data-control protocols (`zwlr_data_control_manager_v1` and its `ext`
//! successor) exist for exactly this case and need no serial. `wl-copy`
//! speaks them, and forks a process that stays alive to serve the data until
//! something else takes the selection, which is work we would otherwise have
//! to do ourselves on a thread of our own.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// True if the clipboard helper is installed, so the popup can say so before
/// someone dictates a paragraph into a delivery that cannot happen.
pub fn is_available() -> bool {
    Command::new("wl-copy")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Put `text` on the clipboard. Blocking; call it off the UI thread.
pub fn copy(text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("there is nothing to copy");
    }

    // stderr is discarded rather than captured, which costs a diagnostic and
    // buys the function returning at all. `wl-copy` forks a process to serve
    // the selection, and that process inherits whatever pipes we hand it — so
    // reading its output to end-of-file, which is what `wait_with_output`
    // does, waits for a writer that stays alive until the selection is
    // replaced. Measured: it never returns, the applet stays in
    // `Status::Delivering` for good, and every later shortcut is ignored.
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain;charset=utf-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => anyhow!(
                "wl-copy is not installed, so the transcript cannot be copied — \
                 install wl-clipboard, or dictate without --clipboard to type instead"
            ),
            _ => anyhow!("cannot run wl-copy: {error}"),
        })?;

    // Taken rather than borrowed so the pipe closes here: `wl-copy` reads
    // until end of input, and would wait forever for a pipe we still hold.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("wl-copy did not offer a stdin pipe"))?;
    stdin
        .write_all(text.as_bytes())
        .context("cannot hand the transcript to wl-copy")?;
    drop(stdin);

    // `wait`, not `wait_with_output`: this waits for the parent to exit,
    // which it does as soon as it has forked the server, and never touches
    // the inherited pipes.
    let status = child.wait().context("cannot wait for wl-copy")?;
    if !status.success() {
        bail!("wl-copy failed with {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_copy_nothing() {
        let error = copy("").unwrap_err().to_string();
        assert!(error.contains("nothing to copy"), "{error}");
    }

    /// Needs a Wayland session with wl-clipboard installed, so it is not part
    /// of the default run.
    ///
    /// Also the regression test for the hang: `copy` returning at all is half
    /// of what this asserts, and the reason the timing is checked.
    #[test]
    #[ignore = "requires a Wayland session and wl-clipboard"]
    fn round_trips_through_the_clipboard() {
        let phrase = "cosmic-whispr clipboard round trip 0123456789";

        let started = std::time::Instant::now();
        copy(phrase).expect("copy");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "copy took {elapsed:?}; it is waiting on the forked server again"
        );

        let pasted = Command::new("wl-paste")
            .arg("--no-newline")
            .output()
            .expect("wl-paste");
        assert_eq!(String::from_utf8_lossy(&pasted.stdout), phrase);
    }
}
