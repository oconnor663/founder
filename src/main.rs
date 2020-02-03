use anyhow::{anyhow, bail, Context, Result};
use clap::{App, Arg, SubCommand};
use duct::cmd;
use once_cell::sync::OnceCell;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::process::ExitStatus;
// Unix-only for now.
use std::os::unix::ffi::OsStrExt;

const MAX_HISTORY_LINES: u64 = 1000;

fn history_dir() -> Result<&'static Path> {
    static HISTORY_DIR: OnceCell<PathBuf> = OnceCell::new();
    HISTORY_DIR
        .get_or_try_init(|| {
            let user_data_dir = dirs::data_local_dir().ok_or_else(|| anyhow!("no data dir"))?;
            let founder_dir = user_data_dir.join("founder");
            fs::create_dir_all(&founder_dir).context("failed to create history dir")?;
            Ok(founder_dir)
        })
        .map(|p| p.as_ref())
}

fn file_history_path() -> Result<PathBuf> {
    Ok(history_dir()?.join("file_history"))
}

fn query_history_path() -> Result<PathBuf> {
    Ok(history_dir()?.join("query_history"))
}

fn file_history_bytes() -> Result<&'static [u8]> {
    static FILE_HISTORY_BYTES: OnceCell<Vec<u8>> = OnceCell::new();
    FILE_HISTORY_BYTES
        .get_or_try_init(|| match fs::read(file_history_path()?) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    // If the file didn't exist, just make an empty Vec.
                    Ok(Vec::new())
                } else {
                    Err(e).context("failed to read history")
                }
            }
        })
        .map(|b| b.as_ref())
}

// These lines do not include the terminating newline.
fn history_lines_from_most_recent() -> Result<impl Iterator<Item = &'static [u8]>> {
    let bytes = file_history_bytes()?;
    Ok(bstr::ByteSlice::rsplit_str(bytes, "\n").filter(|line| !line.is_empty()))
}

fn home_dir() -> Result<&'static Path> {
    static HOME_DIR: OnceCell<PathBuf> = OnceCell::new();
    HOME_DIR
        .get_or_try_init(|| match dirs::home_dir() {
            Some(homedir) => Ok(homedir),
            None => bail!("home directory not configured"),
        })
        .map(|p| p.as_ref())
}

fn compact_history_file() -> Result<()> {
    // Iterate over all the history lines, starting with the most recent, and
    // collect the first unique occurrence of each one into a vector.
    let mut total_lines: u64 = 0;
    let mut lines_set = HashSet::new();
    let mut ordered_unique_lines = Vec::new();
    for line in history_lines_from_most_recent()? {
        total_lines += 1;
        if lines_set.insert(line) {
            ordered_unique_lines.push(line);
        }
    }
    // If the history file does not need to be truncated, short-circuit.
    if total_lines <= MAX_HISTORY_LINES {
        return Ok(());
    }
    // Retain only half the maximum number of lines. (Though pruning duplicates
    // above might already have brought us below that.) This means that we'll
    // go a long time between compactions, rather than compacting all the time
    // when the history file is full of unique entries.
    ordered_unique_lines.truncate((MAX_HISTORY_LINES / 2) as usize);
    // Write the remaining lines to a temporary file. Once the lines are
    // written, we'll swap it with the real history file. Note that this
    // temporary file must be on the same filesystem as the real one, so a
    // standard temp file in /tmp doesn't work here.
    let temp_file_path = file_history_path()?.with_extension("tmp");
    let temp_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true) // error if the file already exists
        .open(&temp_file_path)?;
    let mut temp_file_writer = io::BufWriter::new(temp_file);
    // Note that lines in the history file are oldest-to-newest, which is the
    // opposite of what's in our vector here, so we reverse it.
    for line in ordered_unique_lines.iter().rev() {
        temp_file_writer.write_all(line)?;
        temp_file_writer.write_all(b"\n")?;
    }
    temp_file_writer.flush()?;
    drop(temp_file_writer);
    // Swap the new history file into place.
    fs::rename(&temp_file_path, file_history_path()?)?;
    Ok(())
}

fn add_path_to_history(path: &[u8]) -> Result<()> {
    let path_osstr = OsStr::from_bytes(path);
    // Note that we don't use std::fs::canonicalize here. That fails for files
    // that don't exist. (A common example is "vim foo.txt". That file doesn't
    // exist until you save it, but we want to add it to history immediately.)
    // It's also better not to resolve symbolic links, but to allow different
    // paths to the same file to exist separately in history.
    let mut absolute_path = path_abs::PathAbs::new(path_osstr)?
        .as_path()
        .as_os_str()
        .to_owned();
    // The path does not have an extra newline at the end, so we add one.
    absolute_path.push("\n");
    let mut history_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(file_history_path()?)?;
    history_file.write_all(absolute_path.as_bytes())?;
    Ok(())
}

// Substitute ~/ for the home directory.
fn write_path_to_fzf(
    path_bytes: &[u8],
    fzf_buf_writer: &mut io::BufWriter<os_pipe::PipeWriter>,
) -> Result<()> {
    let path = Path::new(OsStr::from_bytes(path_bytes));
    let mut separator_buf = [0; 4];
    let separator = MAIN_SEPARATOR.encode_utf8(&mut separator_buf);
    if path.starts_with(home_dir()?) {
        // If the path is underneath the home directory, substitute in a ~/.
        let rest = path.strip_prefix(home_dir()?).unwrap();
        fzf_buf_writer.write_all(b"~")?;
        fzf_buf_writer.write_all(separator.as_bytes())?;
        fzf_buf_writer.write_all(rest.as_os_str().as_bytes())?;
    } else if path.starts_with("~") {
        // If the first entire component of the path is a literal ~, prepend a
        // dot-slash. That prevents us from getting confused when we read
        // leading ~ back out from FZF.
        fzf_buf_writer.write_all(b".")?;
        fzf_buf_writer.write_all(separator.as_bytes())?;
        fzf_buf_writer.write_all(path_bytes)?;
    } else {
        // Otherwise just write the path without any changes.
        fzf_buf_writer.write_all(path_bytes)?;
    }
    fzf_buf_writer.write_all(b"\n")?;
    Ok(())
}

// Expands ~/
fn expand_selection(selection: &[u8]) -> Result<Vec<u8>> {
    let path = Path::new(OsStr::from_bytes(selection));
    let mut expanded;
    if path.starts_with("~") {
        // If the first entire component is ~, then we need to expand that to
        // the home directory.
        let rest = path.strip_prefix("~").unwrap();
        let mut separator_buf = [0; 4];
        let separator = MAIN_SEPARATOR.encode_utf8(&mut separator_buf);
        expanded = home_dir()?.as_os_str().as_bytes().to_vec();
        expanded.extend_from_slice(separator.as_bytes());
        expanded.extend_from_slice(rest.as_os_str().as_bytes());
    } else {
        expanded = selection.to_vec();
    }
    Ok(expanded)
}

// Inner, because we want to catch any BrokenPipe errors that this returns.
// This takes a ReaderHandle for fd from the caller, because the caller might
// kill it from another thread.
fn input_thread_inner(
    fd_reader: &duct::ReaderHandle,
    fzf_stdin_writer: os_pipe::PipeWriter,
    mode: &Mode,
) -> Result<()> {
    // Note that &ReaderHandle implements Read.
    let mut fd_buf_reader = io::BufReader::new(fd_reader);
    let mut fzf_buf_writer = io::BufWriter::new(fzf_stdin_writer);

    // Write all the history lines to fzf first, and collect them in a set so
    // that we can filter out duplicates from older history lines and from fd.
    // When we're not in "everything mode", skip over history entries that
    // aren't under the current working directory. Note that we do include
    // hidden files from history, regardless of whether we're asking fd to
    // search for them.
    let cwd = env::current_dir()?;
    let mut seen_history = HashSet::<&[u8]>::new();
    for line in history_lines_from_most_recent()? {
        let mut relative_line = Path::new(OsStr::from_bytes(line));
        if relative_line.starts_with(&cwd) {
            relative_line = relative_line.strip_prefix(&cwd).unwrap();
        } else if !mode.global_history {
            continue;
        }
        let relative_line_bytes = relative_line.as_os_str().as_bytes();
        if seen_history.contains(relative_line_bytes) {
            continue;
        }
        write_path_to_fzf(relative_line_bytes, &mut fzf_buf_writer)?;
        seen_history.insert(relative_line_bytes);
    }
    fzf_buf_writer.flush()?;

    // Now write lines from fd to fzf, filtering out duplicates as noted above.
    let mut line = Vec::new();
    loop {
        line.clear();
        // Read a line from fd. This will implicitly wait on the fd child
        // process if the read encounters EOF, though if fd was killed then the
        // killing thread may have awaited it already.
        let n = fd_buf_reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            // The output from fd is finished. This thread is done.
            fzf_buf_writer.flush()?;
            return Ok(());
        }
        // Check the line we just read against the lines from the history file,
        // and suppress any duplicates.
        assert_eq!(line[line.len() - 1], b'\n');
        let stripped_line = &line[..line.len() - 1];
        if seen_history.contains(stripped_line) {
            continue;
        }
        write_path_to_fzf(stripped_line, &mut fzf_buf_writer)?;
    }
}

// Catches BrokenPipe errors. This takes a ReaderHandle for fd from the caller,
// because the caller might kill it from another thread.
fn input_thread(
    fd_reader: &duct::ReaderHandle,
    fzf_stdin_writer: os_pipe::PipeWriter,
    mode: &Mode,
) -> Result<()> {
    // Ignore BrokenPipe errors from input_thread_inner(). We do that here, at
    // a relatively high level, because we do want these errors to
    // short-circuit the entire input thread.
    match input_thread_inner(fd_reader, fzf_stdin_writer, mode) {
        Ok(()) => Ok(()),
        Err(e) => {
            let maybe_io: Option<&io::Error> = e.root_cause().downcast_ref();
            if let Some(io_error) = maybe_io {
                if io_error.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
            }
            Err(e)
        }
    }
}

fn fzf_command(config: &Config, mode: &Mode, query: &OsStr) -> Result<duct::Expression> {
    let exe = if config.tmux { "fzf-tmux" } else { "fzf" };
    Ok(cmd!(
        exe,
        "--prompt",
        format!("{}> ", mode.mode_name),
        "--expect=ctrl-t",
        "--print-query",
        "--query",
        query,
        "--history",
        query_history_path()?,
        "--history-size=100"
    ))
}

fn run_finder_once(config: &Config, mode: &Mode, query: &OsStr) -> Result<(ExitStatus, Vec<u8>)> {
    // Open the stdin pipe for FZF. The input thread will receive the write
    // end.
    let (fzf_stdin_reader, fzf_stdin_writer) = os_pipe::pipe()?;

    // Start the fd child process with a stdout reader. Each line of output
    // from fd will become input to fzf, if it's not a duplicate of what was
    // already shown from history. In "everything mode", tell fd to include
    // hidden files. The fd command is unchecked() because we will kill it if
    // it's still running when the user makes a selection. That's also why we
    // start it here, instead of just letting the input thread do it.
    let mut fd_args = vec!["--type=f"];
    if mode.fd_hidden_files {
        fd_args.push("--hidden");
    }
    let fd_reader = cmd("fd", &fd_args)
        .unchecked()
        .reader()
        .context("failed to start fd (is it installed?)")?;

    // Start the input thread, then await output from fzf.
    crossbeam_utils::thread::scope(|scope| {
        // Start the background thread that reads the fd pipe and continues
        // writing to the fzf pipe.
        let input_thread = scope.spawn(|_| input_thread(&fd_reader, fzf_stdin_writer, mode));

        // Run FZF and capture its output. This is unchecked() because it
        // returns an error code if the user's filter doesn't match anything,
        // and we'll want to exit with the same code in that case without
        // printing a failure message.
        let fzf_output = fzf_command(config, mode, query)?
            .stdin_file(fzf_stdin_reader)
            .stdout_capture()
            .unchecked()
            .run()
            .context("failed to start fzf (is is installed?)")?;

        // Kill fd if it's still running, and return an error if the fd thread
        // encountered one. This implicitly waits on the fd child process. Note
        // that because of this potential kill signal, fd is unchecked(), and
        // exiting with a non-zero status is not considered an error. Errors
        // here are either a rare OS failure (out of memory?) or a bug.
        fd_reader.kill()?;
        input_thread.join().unwrap()?;

        Ok((fzf_output.status, fzf_output.stdout))
    })
    .expect("panic in threading scope")
}

struct Mode {
    global_history: bool,
    fd_hidden_files: bool,
    mode_name: &'static str,
}

fn run_finder_loop(config: &Config) -> Result<()> {
    const NUM_MODES: usize = 2;
    let mut mode_number: usize = 0;
    let mut previous_query = OsString::new();
    loop {
        let mode = match mode_number {
            0 => Mode {
                global_history: false,
                fd_hidden_files: false,
                mode_name: "local",
            },
            1 => Mode {
                global_history: true,
                fd_hidden_files: true,
                mode_name: "everything",
            },
            _ => unreachable!("invalid mode"),
        };

        let (fzf_status, fzf_output) = run_finder_once(config, &mode, &previous_query)?;

        // The first line of output is the query string, the second is the
        // selection key (enter or ctrl-t), and the third line is the selection
        // (possibly empty with an accompanying error status). Note that these
        // split components will not include trailing newlines.
        let mut parts = bstr::ByteSlice::split_str(&fzf_output[..], "\n");
        let used_query = OsStr::from_bytes(parts.next().expect("no query line"));
        let key = parts.next().expect("no key line");
        let selection = expand_selection(parts.next().expect("no selection line"))?;

        // Check the key before the status. The user may have a query that
        // matches nothing, in which case Ctrl-T will lead to a non-zero
        // status, which we ignore.
        match key {
            b"" => {
                // This is the newline case, which means the user has made a
                // selection. Record that selection to history, write it to
                // stdout, and exit.

                // If Fzf exited with an error code, we exit with that same
                // code. For example, we get an error code if the user's filter
                // didn't match anything.
                if !fzf_status.success() {
                    std::process::exit(fzf_status.code().unwrap_or(1));
                }

                // Absolutify the selection and add that to the history file.
                add_path_to_history(&selection)?;

                // Write the selection to stdout. Add a newline to be
                // compatible with FZF, unless --no-newline is specified.
                io::stdout().write_all(&selection)?;
                if !config.no_newline {
                    io::stdout().write_all(b"\n")?;
                }
                io::stdout().flush()?;
                return Ok(());
            }
            b"ctrl-t" => {
                // The user pressed Ctrl-T. We change modes, preserving the
                // query string, and repeat this loop.
                mode_number = (mode_number + 1) % NUM_MODES;
                previous_query.clear();
                previous_query.push(used_query);
                continue;
            }
            _ => panic!(
                "unexpected selector key: {:?}",
                String::from_utf8_lossy(key)
            ),
        }
    }
}

fn clap_parse_argv() -> clap::ArgMatches<'static> {
    App::new("founder")
        .arg(Arg::with_name("no-newline").long("no-newline"))
        .arg(Arg::with_name("tmux").long("tmux"))
        .subcommand(
            SubCommand::with_name("add").arg(Arg::with_name("path").index(1).required(true)),
        )
        .get_matches()
}

struct Config {
    no_newline: bool,
    tmux: bool,
}

fn main() -> Result<()> {
    let compactor_thread = std::thread::spawn(compact_history_file);
    let matches = clap_parse_argv();
    let command_result = if let Some(add_matches) = matches.subcommand_matches("add") {
        let path = add_matches.value_of_os("path").unwrap().as_bytes();
        add_path_to_history(path)
    } else {
        let config = Config {
            no_newline: matches.is_present("no-newline"),
            tmux: matches.is_present("tmux"),
        };
        run_finder_loop(&config)
    };
    let compactor_result = compactor_thread.join().expect("compactor panic");
    command_result.and(compactor_result)
}
