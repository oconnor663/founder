#! /usr/bin/env python3

from os import path

history_path = path.expanduser("~/.local/share/founder/file_history")
output = []

with open(history_path) as f:
    for line in f:
        filepath = line.strip()
        if path.exists(filepath):
            output.append(filepath)

with open(history_path, "w") as f:
    for filepath in output:
        f.write(filepath + "\n")
