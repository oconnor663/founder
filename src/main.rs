use anyhow::{anyhow, Context, Result};
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

const MAX_HISTORY_LINES: usize = 1000;

fn history_path() -> Result<&'static Path> {
    static HISTORY_PATH: OnceCell<PathBuf> = OnceCell::new();
    HISTORY_PATH
        .get_or_try_init(|| {
            let user_data_dir = dirs::data_local_dir().ok_or_else(|| anyhow!("no data dir"))?;
            let founder_dir = user_data_dir.join("founder");
            fs::create_dir_all(&founder_dir).context("failed to create history dir")?;
            Ok(founder_dir.join("history"))
        })
        .map(|p| p.as_ref())
}

fn history_bytes() -> Result<&'static [u8]> {
    static HISTORY_BYTES: OnceCell<Vec<u8>> = OnceCell::new();
    HISTORY_BYTES
        .get_or_try_init(|| match fs::read(history_path()?) {
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
    let bytes = history_bytes()?;
    Ok(bstr::ByteSlice::rsplit_str(bytes, "\n").filter(|line| !line.is_empty()))
}

fn home_dir() -> io::Result<&'static Path> {
    static HOME_DIR: OnceCell<PathBuf> = OnceCell::new();
    HOME_DIR
        .get_or_try_init(|| match dirs::home_dir() {
            Some(homedir) => Ok(homedir),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no home directory configured",
            )),
        })
        .map(|p| p.as_ref())
}

// Returns an io::Result, so that broken pipe errors can get caught and
// suppressed by the caller.
fn filter_fd_output_ioresult(
    seen_history: &HashSet<&[u8]>,
    fd_buf_reader: &mut io::BufReader<&duct::ReaderHandle>,
    // By value, so that it's closed implicitly:
    mut fzf_buf_writer: io::BufWriter<os_pipe::PipeWriter>,
) -> io::Result<()> {
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

fn filter_fd_output(
    seen_history: &HashSet<&[u8]>,
    fd_buf_reader: &mut io::BufReader<&duct::ReaderHandle>,
    fzf_buf_writer: io::BufWriter<os_pipe::PipeWriter>,
) -> Result<()> {
    match filter_fd_output_ioresult(seen_history, fd_buf_reader, fzf_buf_writer) {
        Ok(()) => Ok(()),
        Err(e) => {
            if e.kind() == io::ErrorKind::BrokenPipe {
                // Suppress broken pipe errors.
                Ok(())
            } else {
                Err(e).context("failed to filter fd output")
            }
        }
    }
}

fn compact_history_file() -> Result<()> {
    // Iterate over all the history lines, starting with the most recent, and
    // collect the first unique occurrence of each one into a vector.
    let mut lines_set = HashSet::new();
    let mut ordered_unique_lines = Vec::new();
    for line in history_lines_from_most_recent()? {
        if lines_set.insert(line) {
            ordered_unique_lines.push(line);
        }
    }
    // Retain only half the maximum number of lines. (Though pruning duplicates
    // above might already have brought us below that.) This means that we'll
    // go a long time between compactions, rather than compacting all the time
    // when the history file is full of unique entries.
    ordered_unique_lines.truncate(MAX_HISTORY_LINES / 2);
    // Write the remaining lines to a temporary file. Once the lines are
    // written, we'll swap it with the real history file. Note that this
    // temporary file must be on the same filesystem as the real one, so a
    // standard temp file in /tmp doesn't work here.
    let temp_file_path = history_path()?.with_extension("tmp");
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
    fs::rename(&temp_file_path, history_path()?)?;
    Ok(())
}

fn add_selection_to_history(selection: &[u8]) -> Result<()> {
    let selection_osstr = OsStr::from_bytes(selection);
    let mut canonical_path: OsString = fs::canonicalize(selection_osstr)
        .with_context(|| format!("failed to canonicalize {:?}", selection_osstr))?
        .into();
    // The selection does not have an extra newline at the end, so we add one.
    canonical_path.push("\n");
    let mut history_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(history_path()?)?;
    history_file.write_all(canonical_path.as_bytes())?;
    Ok(())
}

// Substitute ~/ for the home directory.
fn write_path_to_fzf(
    path_bytes: &[u8],
    fzf_buf_writer: &mut io::BufWriter<os_pipe::PipeWriter>,
) -> io::Result<()> {
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

// Includes global history, ignores hidden local files.
fn default_finder() -> Result<(ExitStatus, Vec<u8>)> {
    // Start the fzf child process with a stdout reader and an explicit stdin
    // pipe. Dropping the reader will implicitly kill fzf, though that will
    // only happen if there's an unexpected error. Do this first to mimize the
    // delay before fzf appears. Reading to EOF will automatically await the
    // fzf child process. This is unchecked() because it returns an error code
    // if the user's filter doesn't match anything, and we'll want to exit with
    // the same code in that case without printing a failure message.
    let (fzf_stdin_read, fzf_stdin_write) = os_pipe::pipe()?;
    let fzf_reader = cmd!(
        "fzf-tmux",
        "--prompt=combined> ",
        "--expect=ctrl-t",
        "--print-query"
    )
    .stdin_file(fzf_stdin_read)
    .unchecked()
    .reader()
    .context("failed to start fzf (is is installed?)")?;
    let mut fzf_buf_writer = io::BufWriter::new(fzf_stdin_write);

    // Iterate over history lines backwards from the end. That means that the
    // most recently appended lines come first. For each line, if the current
    // working directory is a prefix, strip that off. Then write each line to
    // fzf. These will display in the terminal immediately. Keep track of the
    // set of lines written so far, to suppress duplicates. (Note that this
    // relies on fd not putting ./ at the start of the paths it outputs.)
    let cwd = env::current_dir()?;
    let mut seen_history = HashSet::<&[u8]>::new();
    let mut history_lines: usize = 0;
    #[allow(clippy::explicit_counter_loop)]
    for line in history_lines_from_most_recent()? {
        history_lines += 1;
        let mut relative_line = Path::new(OsStr::from_bytes(line));
        if relative_line.starts_with(&cwd) {
            relative_line = relative_line.strip_prefix(&cwd).unwrap();
        }
        let relative_line_bytes = relative_line.as_os_str().as_bytes();
        if seen_history.contains(relative_line_bytes) {
            continue;
        }
        write_path_to_fzf(relative_line_bytes, &mut fzf_buf_writer)?;
        seen_history.insert(relative_line_bytes);
    }
    fzf_buf_writer.flush()?;

    // Start the fd child process with a stdout reader. Each line of output
    // from fd will become input to fzf, if it's not a duplicate of what was
    // already shown from history. This is unchecked() because we might kill it
    // if it's still running when the user makes a selection.
    let fd_reader = cmd!("fd", "--type=f")
        .unchecked()
        .reader()
        .context("failed to start fd (is it installed?)")?;
    let mut fd_buf_reader = io::BufReader::new(&fd_reader);

    let mut fzf_output = Vec::new();
    crossbeam_utils::thread::scope(|scope| -> Result<()> {
        // Start the background thread that reads the fd pipe and continues
        // writing to the fzf pipe.
        let fd_thread =
            scope.spawn(|_| filter_fd_output(&seen_history, &mut fd_buf_reader, fzf_buf_writer));

        // If the history file is at capacity, start another background thread
        // to compact it. We've already finished reading from the history file
        // by this point, so rewriting it won't cause any problems.
        let compact_thread = if history_lines >= MAX_HISTORY_LINES {
            Some(scope.spawn(|_| compact_history_file()))
        } else {
            None
        };

        // Read the selection from fzf. This implicitly waits on the fzf child
        // process, but the child returning a non-zero status is unchecked()
        // here. We'll check that explicitly below.
        (&fzf_reader).read_to_end(&mut fzf_output)?;

        // Kill fd if it's still running, and return an error if the fd thread
        // encountered one. This implicitly waits on the fd child process. Note
        // that because of this potential kill signal, fd is unchecked(), and
        // exiting with a non-zero status is not considered an error. Errors
        // here are either a rare OS failure (out of memory?) or a bug.
        fd_reader.kill()?;
        fd_thread.join().unwrap()?;

        // Return an error if the history compaction thread encountered one.
        if let Some(thread) = compact_thread {
            thread.join().unwrap()?;
        }

        Ok(())
    })
    .unwrap()?;

    let fzf_status = fzf_reader.try_wait()?.expect("fzf exited").status;
    Ok((fzf_status, fzf_output))
}

fn run_finder() -> Result<()> {
    let (status, output) = default_finder()?;

    // If Fzf exited with an error code, we exit with that same code. For
    // example, we get an error code if the user's filter didn't match
    // anything.
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    // The first line of output is the query string, the second is the
    // selection key (enter or ctrl-t), and the third line is the selection
    // (possibly empty with an accompanying error status). Note that these
    // split components will not include trailing newlines.
    let mut parts = bstr::ByteSlice::split_str(&output[..], "\n");
    let query = parts.next().expect("no query line");
    let key = parts.next().expect("no key line");
    let selection = expand_selection(parts.next().expect("no selection line"))?;

    // Check the key before the status. The user may have a query that matches
    // nothing, in which case Ctrl-T will lead to a non-zero status, which we
    // ignore.
    match key {
        b"" => (), // empty means newline here, fall through
        b"ctrl-t" => panic!("CTRL-T woo"),
        _ => panic!(
            "unexpected selector key: {:?}",
            String::from_utf8_lossy(key)
        ),
    }

    // Canonicalize the selection and add that to the history file. This can
    // fail if the selection no longer exists.
    add_selection_to_history(&selection)?;

    // Write the selection to stdout.
    io::stdout().write_all(&selection)?;
    io::stdout().flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<OsString> = env::args_os().collect();
    if args.len() > 1 {
        assert_eq!(args.len(), 3, "unexpected number of args");
        assert_eq!(&args[1], "--add", "unknown arg");
        add_selection_to_history(args[2].as_bytes())
    } else {
        run_finder()
    }
}
