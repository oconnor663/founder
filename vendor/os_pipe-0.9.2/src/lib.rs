//! A cross-platform library for opening OS pipes.
//!
//! The standard library uses pipes to read output from child processes,
//! but it doesn't expose a way to create them directly. This crate
//! fills that gap with the `pipe` function. It also includes some
//! helpers for passing pipes to the `std::process::Command` API.
//!
//! - [Docs](https://docs.rs/os_pipe)
//! - [Crate](https://crates.io/crates/os_pipe)
//! - [Repo](https://github.com/oconnor663/os_pipe.rs)
//!
//! Usage note: The main purpose of `os_pipe` is to support the
//! higher-level [`duct`](https://github.com/oconnor663/duct.rs)
//! library, which handles most of the same use cases with much less
//! code and no risk of deadlocks. `duct` can run the entire example
//! below in one line of code.
//!
//! # Example
//!
//! Join the stdout and stderr of a child process into a single stream,
//! and read it. To do that we open a pipe, duplicate its write end, and
//! pass those writers as the child's stdout and stderr. Then we can
//! read combined output from the read end of the pipe. We have to be
//! careful to close the write ends first though, or reading will block
//! waiting for EOF.
//!
//! ```rust
//! use os_pipe::pipe;
//! use std::io::prelude::*;
//! use std::process::{Command, Stdio};
//!
//! // This command prints "foo" to stdout and "bar" to stderr. It
//! // works on both Unix and Windows, though there are whitespace
//! // differences that we'll account for at the bottom.
//! let shell_command = "echo foo && echo bar >&2";
//!
//! // Ritual magic to run shell commands on different platforms.
//! let (shell, flag) = if cfg!(windows) { ("cmd.exe", "/C") } else { ("sh", "-c") };
//!
//! let mut child = Command::new(shell);
//! child.arg(flag);
//! child.arg(shell_command);
//!
//! // Here's the interesting part. Open a pipe, copy its write end, and
//! // give both copies to the child.
//! let (mut reader, writer) = pipe().unwrap();
//! let writer_clone = writer.try_clone().unwrap();
//! child.stdout(writer);
//! child.stderr(writer_clone);
//!
//! // Now start the child running.
//! let mut handle = child.spawn().unwrap();
//!
//! // Very important when using pipes: This parent process is still
//! // holding its copies of the write ends, and we have to close them
//! // before we read, otherwise the read end will never report EOF. The
//! // Command object owns the writers now, and dropping it closes them.
//! drop(child);
//!
//! // Finally we can read all the output and clean up the child.
//! let mut output = String::new();
//! reader.read_to_string(&mut output).unwrap();
//! handle.wait().unwrap();
//! assert!(output.split_whitespace().eq(vec!["foo", "bar"]));
//! ```

use std::fs::File;
use std::io;
use std::process::Stdio;

/// The reading end of a pipe, returned by [`pipe`](fn.pipe.html).
///
/// `PipeReader` implements `Into<Stdio>`, so you can pass it as an argument to
/// `Command::stdin` to spawn a child process that reads from the pipe.
#[derive(Debug)]
pub struct PipeReader(File);

impl PipeReader {
    pub fn try_clone(&self) -> io::Result<PipeReader> {
        // Do *not* use File::try_clone here. It's buggy on windows. See
        // comments on windows.rs::dup().
        sys::dup(&self.0).map(PipeReader)
    }
}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<'a> io::Read for &'a PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut file_ref = &self.0;
        file_ref.read(buf)
    }
}

impl From<PipeReader> for Stdio {
    fn from(p: PipeReader) -> Stdio {
        p.0.into()
    }
}

/// The writing end of a pipe, returned by [`pipe`](fn.pipe.html).
///
/// `PipeWriter` implements `Into<Stdio>`, so you can pass it as an argument to
/// `Command::stdout` or `Command::stderr` to spawn a child process that writes
/// to the pipe.
#[derive(Debug)]
pub struct PipeWriter(File);

impl PipeWriter {
    pub fn try_clone(&self) -> io::Result<PipeWriter> {
        // Do *not* use File::try_clone here. It's buggy on windows. See
        // comments on windows.rs::dup().
        sys::dup(&self.0).map(PipeWriter)
    }
}

impl io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> io::Write for &'a PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file_ref = &self.0;
        file_ref.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file_ref = &self.0;
        file_ref.flush()
    }
}

impl From<PipeWriter> for Stdio {
    fn from(p: PipeWriter) -> Stdio {
        p.0.into()
    }
}

/// Open a new pipe and return a [`PipeReader`] and [`PipeWriter`] pair.
///
/// This corresponds to the `pipe2` library call on Posix and the
/// `CreatePipe` library call on Windows (though these implementation
/// details might change). Pipes are non-inheritable, so new child
/// processes won't receive a copy of them unless they're explicitly
/// passed as stdin/stdout/stderr.
///
/// [`PipeReader`]: struct.PipeReader.html
/// [`PipeWriter`]: struct.PipeWriter.html
pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    sys::pipe()
}

/// Get a duplicated copy of the current process's standard input, as a
/// [`PipeReader`].
///
/// Reading directly from this pipe isn't recommended, because it's not
/// synchronized with [`std::io::stdin`]. [`PipeReader`] implements
/// [`Into<Stdio>`], so it can be passed directly to [`Command::stdin`]. This is
/// equivalent to [`Stdio::inherit`], though, so it's usually not necessary
/// unless you need a collection of different pipes.
///
/// [`std::io::stdin`]: https://doc.rust-lang.org/std/io/fn.stdin.html
/// [`PipeReader`]: struct.PipeReader.html
/// [`Into<Stdio>`]: https://doc.rust-lang.org/std/process/struct.Stdio.html
/// [`Command::stdin`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.stdin
/// [`Stdio::inherit`]: https://doc.rust-lang.org/std/process/struct.Stdio.html#method.inherit
pub fn dup_stdin() -> io::Result<PipeReader> {
    sys::dup(&io::stdin()).map(PipeReader)
}

/// Get a duplicated copy of the current process's standard output, as a
/// [`PipeWriter`](struct.PipeWriter.html).
///
/// Writing directly to this pipe isn't recommended, because it's not
/// synchronized with [`std::io::stdout`]. [`PipeWriter`] implements
/// [`Into<Stdio>`], so it can be passed directly to [`Command::stdout`] or
/// [`Command::stderr`]. This can be useful if you want the child's stderr to go
/// to the parent's stdout.
///
/// [`std::io::stdout`]: https://doc.rust-lang.org/std/io/fn.stdout.html
/// [`PipeWriter`]: struct.PipeWriter.html
/// [`Into<Stdio>`]: https://doc.rust-lang.org/std/process/struct.Stdio.html
/// [`Command::stdout`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.stdout
/// [`Command::stderr`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.stderr
/// [`Stdio::inherit`]: https://doc.rust-lang.org/std/process/struct.Stdio.html#method.inherit
pub fn dup_stdout() -> io::Result<PipeWriter> {
    sys::dup(&io::stdout()).map(PipeWriter)
}

/// Get a duplicated copy of the current process's standard error, as a
/// [`PipeWriter`](struct.PipeWriter.html).
///
/// Writing directly to this pipe isn't recommended, because it's not
/// synchronized with [`std::io::stderr`]. [`PipeWriter`] implements
/// [`Into<Stdio>`], so it can be passed directly to [`Command::stdout`] or
/// [`Command::stderr`]. This can be useful if you want the child's stdout to go
/// to the parent's stderr.
///
/// [`std::io::stderr`]: https://doc.rust-lang.org/std/io/fn.stderr.html
/// [`PipeWriter`]: struct.PipeWriter.html
/// [`Into<Stdio>`]: https://doc.rust-lang.org/std/process/struct.Stdio.html
/// [`Command::stdout`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.stdout
/// [`Command::stderr`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.stderr
/// [`Stdio::inherit`]: https://doc.rust-lang.org/std/process/struct.Stdio.html#method.inherit
pub fn dup_stderr() -> io::Result<PipeWriter> {
    sys::dup(&io::stderr()).map(PipeWriter)
}

#[cfg(not(windows))]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;

#[cfg(test)]
mod tests {
    use std::env::consts::EXE_EXTENSION;
    use std::io::prelude::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Once;
    use std::thread;

    fn path_to_exe(name: &str) -> PathBuf {
        // This project defines some associated binaries for testing, and we shell out to them in
        // these tests. `cargo test` doesn't automatically build associated binaries, so this
        // function takes care of building them explicitly, with the right debug/release flavor.
        static CARGO_BUILD_ONCE: Once = Once::new();
        CARGO_BUILD_ONCE.call_once(|| {
            let mut build_command = Command::new("cargo");
            build_command.args(&["build", "--quiet"]);
            if !cfg!(debug_assertions) {
                build_command.arg("--release");
            }
            let build_status = build_command.status().unwrap();
            assert!(
                build_status.success(),
                "Cargo failed to build associated binaries."
            );
        });
        let flavor = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        Path::new("target")
            .join(flavor)
            .join(name)
            .with_extension(EXE_EXTENSION)
    }

    #[test]
    fn test_pipe_some_data() {
        let (mut reader, mut writer) = crate::pipe().unwrap();
        // A small write won't fill the pipe buffer, so it won't block this thread.
        writer.write_all(b"some stuff").unwrap();
        drop(writer);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "some stuff");
    }

    #[test]
    fn test_pipe_some_data_with_refs() {
        // As with `File`, there's a second set of impls for shared
        // refs. Test those.
        let (reader, writer) = crate::pipe().unwrap();
        let mut reader_ref = &reader;
        {
            let mut writer_ref = &writer;
            // A small write won't fill the pipe buffer, so it won't block this thread.
            writer_ref.write_all(b"some stuff").unwrap();
        }
        drop(writer);
        let mut out = String::new();
        reader_ref.read_to_string(&mut out).unwrap();
        assert_eq!(out, "some stuff");
    }

    #[test]
    fn test_pipe_no_data() {
        let (mut reader, writer) = crate::pipe().unwrap();
        drop(writer);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn test_pipe_a_megabyte_of_data_from_another_thread() {
        let data = vec![0xff; 1_000_000];
        let data_copy = data.clone();
        let (mut reader, mut writer) = crate::pipe().unwrap();
        let joiner = thread::spawn(move || {
            writer.write_all(&data_copy).unwrap();
            // This drop happens automatically, so writing it out here is mostly
            // just for clarity. For what it's worth, it also guards against
            // accidentally forgetting to drop if we switch to scoped threads or
            // something like that and change this to a non-moving closure. The
            // explicit drop forces `writer` to move.
            drop(writer);
        });
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        joiner.join().unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn test_pipes_are_not_inheritable() {
        // Create pipes for a child process.
        let (input_reader, mut input_writer) = crate::pipe().unwrap();
        let (mut output_reader, output_writer) = crate::pipe().unwrap();

        // Create a bunch of duplicated copies, which we'll close later. This
        // tests that duplication preserves non-inheritability.
        let ir_dup = input_reader.try_clone().unwrap();
        let iw_dup = input_writer.try_clone().unwrap();
        let or_dup = output_reader.try_clone().unwrap();
        let ow_dup = output_writer.try_clone().unwrap();

        // Spawn the child. Note that this temporary Command object takes
        // ownership of our copies of the child's stdin and stdout, and then
        // closes them immediately when it drops. That stops us from blocking
        // our own read below. We use our own simple implementation of cat for
        // compatibility with Windows.
        let mut child = Command::new(path_to_exe("cat"))
            .stdin(input_reader)
            .stdout(output_writer)
            .spawn()
            .unwrap();

        // Drop all the dups now that the child is spawned.
        drop(ir_dup);
        drop(iw_dup);
        drop(or_dup);
        drop(ow_dup);

        // Write to the child's stdin. This is a small write, so it shouldn't
        // block.
        input_writer.write_all(b"hello").unwrap();
        drop(input_writer);

        // Read from the child's stdout. If this child has accidentally
        // inherited the write end of its own stdin, then it will never exit,
        // and this read will block forever. That's what this test is all
        // about.
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).unwrap();
        child.wait().unwrap();

        // Confirm that we got the right bytes.
        assert_eq!(b"hello", &*output);
    }

    #[test]
    fn test_parent_handles() {
        // This test invokes the `swap` test program, which uses parent_stdout() and
        // parent_stderr() to swap the outputs for another child that it spawns.

        // Create pipes for a child process.
        let (reader, mut writer) = crate::pipe().unwrap();

        // Write input. This shouldn't block because it's small. Then close the write end, or else
        // the child will hang.
        writer.write_all(b"quack").unwrap();
        drop(writer);

        // Use `swap` to run `cat_both`. `cat_both will read "quack" from stdin
        // and write it to stdout and stderr with different tags. But because we
        // run it inside `swap`, the tags in the output should be backwards.
        let output = Command::new(path_to_exe("swap"))
            .arg(path_to_exe("cat_both"))
            .stdin(reader)
            .output()
            .unwrap();

        // Check for a clean exit.
        assert!(
            output.status.success(),
            "child process returned {:#?}",
            output
        );

        // Confirm that we got the right bytes.
        assert_eq!(b"stderr: quack", &*output.stdout);
        assert_eq!(b"stdout: quack", &*output.stderr);
    }

    #[test]
    fn test_parent_handles_dont_close() {
        // Open and close each parent pipe multiple times. If this closes the
        // original, subsequent opens should fail.
        let stdin = crate::dup_stdin().unwrap();
        drop(stdin);
        let stdin = crate::dup_stdin().unwrap();
        drop(stdin);

        let stdout = crate::dup_stdout().unwrap();
        drop(stdout);
        let stdout = crate::dup_stdout().unwrap();
        drop(stdout);

        let stderr = crate::dup_stderr().unwrap();
        drop(stderr);
        let stderr = crate::dup_stderr().unwrap();
        drop(stderr);
    }

    #[test]
    fn test_try_clone() {
        let (reader, writer) = crate::pipe().unwrap();
        let mut reader_clone = reader.try_clone().unwrap();
        let mut writer_clone = writer.try_clone().unwrap();
        // A small write won't fill the pipe buffer, so it won't block this thread.
        writer_clone.write_all(b"some stuff").unwrap();
        drop(writer);
        drop(writer_clone);
        let mut out = String::new();
        reader_clone.read_to_string(&mut out).unwrap();
        assert_eq!(out, "some stuff");
    }

    #[test]
    fn test_debug() {
        let (reader, writer) = crate::pipe().unwrap();
        format!("{:?} {:?}", reader, writer);
    }
}
