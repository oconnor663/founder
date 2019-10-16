use anyhow::{anyhow, Context, Result};
use duct::cmd;
use once_cell::sync::OnceCell;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
// Unix-only for now.
use std::os::unix::ffi::OsStrExt;

const MAX_HISTORY_LINES: usize = 1000;

fn history_path() -> Result<&'static PathBuf> {
    static HISTORY_PATH: OnceCell<PathBuf> = OnceCell::new();
    HISTORY_PATH.get_or_try_init(|| {
        let user_data_dir = dirs::data_local_dir().ok_or_else(|| anyhow!("no data dir"))?;
        let founder_dir = user_data_dir.join("founder");
        fs::create_dir_all(&founder_dir).context("creating history dir")?;
        Ok(founder_dir.join("history"))
    })
}

fn filter_fd_output(
    seen_history: &HashSet<&[u8]>,
    fd_buf_reader: &mut io::BufReader<&duct::ReaderHandle>,
    // By value, so that it's closed implicitly:
    mut fzf_buf_writer: io::BufWriter<os_pipe::PipeWriter>,
) -> Result<()> {
    // Start the fd child process with a stdout reader. Dropping this reader or
    // reading to EOF will automatically await (and potentially kill) the child
    // process.
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = match fd_buf_reader.read_until('\n' as u8, &mut line) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    // Fzf has exited. This thread should quit gracefully.
                    return Ok(());
                } else {
                    return Err(e.into());
                }
            }
        };
        if n == 0 {
            // The output from fd is finished. This thread is done.
            fzf_buf_writer.flush()?;
            return Ok(());
        }
        // Check the line we just read against the lines from the history file,
        // and suppress any duplicates.
        assert_eq!(line[line.len() - 1], '\n' as u8);
        let stripped_line = &line[..line.len() - 1];
        if seen_history.contains(stripped_line) {
            continue;
        }
        fzf_buf_writer.write_all(&line)?;
    }
}

// These lines do not include the terminating newline.
fn history_lines_from_most_recent(history_bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bstr::ByteSlice::rsplit_str(&history_bytes[..], "\n").filter(|line| !line.is_empty())
}

fn compact_history_file(history_bytes: &[u8]) -> Result<()> {
    // Iterate over all the history lines, starting with the most recent, and
    // collect the first unique occurrence of each one into a vector.
    let mut lines_set = HashSet::new();
    let mut ordered_unique_lines = Vec::new();
    for line in history_lines_from_most_recent(history_bytes) {
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

fn main() -> Result<()> {
    // Start the fzf child process with a stdout reader and an explicit stdin
    // pipe. Dropping the reader will implicitly kill fzf, though that will
    // only happen if there's an unexpected error. Do this first to mimize the
    // delay before fzf appears. Reading to EOF will automatically await the
    // child process. This is unchecked() because it returns an error if the
    // user's filter doesn't match anything, and we'll want to report that
    // error cleanly rather than crashing.
    let (fzf_stdin_read, fzf_stdin_write) = os_pipe::pipe()?;
    let fzf_reader = cmd!("fzf")
        .stdin_file(fzf_stdin_read)
        .unchecked()
        .reader()
        .context("opening fzf (is fzf installed?)")?;
    let mut fzf_buf_writer = io::BufWriter::new(fzf_stdin_write);

    // Read all the bytes of the history file. If it doesn't exist, create an
    // empty vec instead.
    let history_bytes = match fs::read(history_path()?) {
        Ok(bytes) => bytes,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                Vec::new()
            } else {
                return Err(e).context("reading history");
            }
        }
    };

    // Iterate over history lines backwards from the end. That means that the
    // most recently appended lines come first. For each line, if the current
    // working directory is a prefix, strip that off. Then write each line to
    // fzf. These will display in the terminal immediately. Keep track of the
    // set of lines written so far, to suppress duplicates. (Note that this
    // relies on fd not putting ./ at the start of the paths it outputs.)
    let cwd = env::current_dir()?;
    let mut seen_history = HashSet::<&[u8]>::new();
    let mut history_lines: usize = 0;
    for line in history_lines_from_most_recent(&history_bytes) {
        let mut relative_line = Path::new(OsStr::from_bytes(line));
        if relative_line.starts_with(&cwd) {
            relative_line = relative_line.strip_prefix(&cwd).unwrap();
        }
        let relative_line_bytes = relative_line.as_os_str().as_bytes();
        if seen_history.contains(relative_line_bytes) {
            continue;
        }
        fzf_buf_writer.write(relative_line_bytes)?;
        fzf_buf_writer.write(b"\n")?;
        seen_history.insert(relative_line_bytes);
        history_lines += 1;
    }
    fzf_buf_writer.flush()?;

    // Start the fd child process with a stdout reader. Each line of output
    // from fd will become input to fzf, if it's not a duplicate of what was
    // already shown from history. This is unchecked() because we might kill
    // it.
    let fd_reader = cmd!("fd", "--type=f")
        .unchecked()
        .reader()
        .context("opening fd (is fd installed?)")?;
    let mut fd_buf_reader = io::BufReader::new(&fd_reader);

    let mut fzf_output = Vec::new();
    crossbeam_utils::thread::scope(|scope| -> Result<()> {
        // Start the background thread that reads the fd pipe and continues
        // writing to the fzf pipe.
        scope.spawn(|_| filter_fd_output(&seen_history, &mut fd_buf_reader, fzf_buf_writer));

        // If the history file is at capacity, start another background thread
        // to compact it. Leaving this scope will join this thread.
        if history_lines >= MAX_HISTORY_LINES {
            scope.spawn(|_| compact_history_file(&history_bytes));
        }

        // Read the selection from fzf. (Fzf might return an error, but we'll
        // check that below.)
        (&fzf_reader).read_to_end(&mut fzf_output)?;

        // Kill fd. We'll explicitly join that thread as we leave this scope.
        fd_reader.kill()?;

        Ok(())
    })
    .unwrap()?;

    // If Fzf returned an error, exit with that error.
    let fzf_status = fzf_reader.try_wait()?.expect("fzf exited").status;
    if !fzf_status.success() {
        std::process::exit(fzf_status.code().unwrap_or(1));
    }

    // Write the selection to stdout. Fzf appends a newline, which we strip.
    assert_eq!(fzf_output[fzf_output.len() - 1], '\n' as u8);
    let stripped_selection = &fzf_output[..fzf_output.len() - 1];
    io::stdout().write_all(stripped_selection)?;
    io::stdout().flush()?;

    // Canonicalize the selection and add that to the history file. Put the
    // newline back for this part.
    let mut canonical_path: OsString =
        fs::canonicalize(OsStr::from_bytes(stripped_selection))?.into();
    canonical_path.push("\n");
    let mut history_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(history_path()?)?;
    history_file.write_all(canonical_path.as_os_str().as_bytes())?;

    Ok(())
}
