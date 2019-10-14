use std::fs;
use std::io;
use std::io::prelude::*;
use std::process::{Command, Stdio};

fn main() -> io::Result<()> {
    // Create the history dir (~/.local/share/founder or equivalent).
    let user_data_dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data dir"))?;
    let founder_dir = user_data_dir.join("founder");
    fs::create_dir_all(&founder_dir)?;

    // Read all the bytes of the history file.
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

    // Start the fzf child process with stdin and stdout pipes.
    let mut fzf_handle = Command::new("fzf")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut fzf_stdin = fzf_handle.stdin.take().unwrap();
    let mut fzf_stdout = fzf_handle.stdout.take().unwrap();

    // Iterate over history lines backwards from the end. That means that the
    // most recently appended lines come first. Write each line to fzf. These
    // will display in the terminal immediately.
    let mut buf_writer = io::BufWriter::new(&mut fzf_stdin);
    for line in bstr::ByteSlice::rsplit_str(&history_bytes[..], &b"\n"[..]) {
        if line.is_empty() {
            continue;
        }
        buf_writer.write(line)?;
        buf_writer.write(b"\n")?;
    }
    buf_writer.flush()?;
    drop(buf_writer);

    // Start the fd child process writing to the same pipe.
    let mut fd_handle = Command::new("fd")
        .arg("--type=f")
        .stdout(fzf_stdin)
        .spawn()?;

    // Read the user's selection from the output of fzf.
    let mut selection = Vec::new();
    fzf_stdout.read_to_end(&mut selection)?;

    // Write the selection to stdout. Fzf appends a newline, which we strip.
    assert_eq!(selection[selection.len() - 1], '\n' as u8);
    let stripped_selection = &selection[..selection.len() - 1];
    io::stdout().write_all(stripped_selection)?;

    // Add the selection to the history file. Include the \n for this part.
    let mut history_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&history_path)?;
    history_file.write_all(&selection)?;

    // Wait on fzf, then kill fd and wait on that. Fd generally exits promptly,
    // but there's no reason to assume.
    let status = fzf_handle.wait()?;
    fd_handle.kill()?;
    fd_handle.wait()?;

    std::process::exit(status.code().unwrap_or(1));
}
