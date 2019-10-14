use duct::cmd;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::io::prelude::*;
use std::path::Path;
// Unix-only for now.
use std::os::unix::ffi::OsStrExt;

fn fd_filter_thread(
    seen_history: &HashSet<&[u8]>,
    mut fzf_buf_writer: io::BufWriter<os_pipe::PipeWriter>,
) -> io::Result<()> {
    // Start the fd child process with a stdout reader. Dropping this reader or
    // reading to EOF will automatically await (and potentially kill) the child
    // process.
    let fd_reader = cmd!("fd", "--type=f").reader()?;
    let mut fd_buf_reader = io::BufReader::new(fd_reader);
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
                    return Err(e);
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

fn main() -> io::Result<()> {
    // Start the fzf child process with a stdout reader and an explicit stdin
    // pipe. Do this first to mimize the delay before the finder appears.
    // Reading to EOF will automatically await the child process.
    let (fzf_stdin_read, fzf_stdin_write) = os_pipe::pipe()?;
    let mut fzf_reader = cmd!("fzf").stdin_file(fzf_stdin_read).reader()?;
    let mut fzf_buf_writer = io::BufWriter::new(fzf_stdin_write);

    // Create the history dir (~/.local/share/founder or equivalent).
    let user_data_dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?;
    let founder_dir = user_data_dir.join("founder");
    fs::create_dir_all(&founder_dir)?;

    // Read all the bytes of the history file. If it doesn't exist, create an
    // empty vec instead.
    let history_path = founder_dir.join("history");
    let history_bytes = match fs::read(&history_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                Vec::new()
            } else {
                return Err(e);
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
    let mut seen_history = HashSet::new();
    for line_bytes in bstr::ByteSlice::rsplit_str(&history_bytes[..], &b"\n"[..]) {
        if line_bytes.is_empty() {
            continue;
        }
        let mut relative_line = Path::new(OsStr::from_bytes(line_bytes));
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
    }
    fzf_buf_writer.flush()?;

    let selection = crossbeam_utils::thread::scope(|scope| {
        // Start the background thread that will manage the fd child process
        // and continue writing to the fzf pipe.
        let fd_thread_handle = scope.spawn(|_| fd_filter_thread(&seen_history, fzf_buf_writer));

        // Read the user's selection from the output of fzf. This automatically
        // awaits fzf after reading EOF.
        let mut selection = Vec::new();
        let read_result = fzf_reader.read_to_end(&mut selection);

        // Join the fd thread. If it didn't finish writing its output to the
        // fzf pipe, it should gracefully exit after a closed pipe error.
        // TODO: Kill it?
        let fd_result = fd_thread_handle.join().unwrap();

        read_result.and(fd_result).map(|_| selection)
    })
    .unwrap()?;

    // Write the selection to stdout. Fzf appends a newline, which we strip.
    assert_eq!(selection[selection.len() - 1], '\n' as u8);
    let stripped_selection = &selection[..selection.len() - 1];
    io::stdout().write_all(stripped_selection)?;

    // Canonicalize the selection and add that to the history file. Put the
    // newline back for this part.
    let mut canonical_path: OsString =
        fs::canonicalize(OsStr::from_bytes(stripped_selection))?.into();
    canonical_path.push("\n");
    let mut history_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&history_path)?;
    history_file.write_all(canonical_path.as_os_str().as_bytes())?;

    Ok(())
}
