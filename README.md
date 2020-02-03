# founder

A CLI fuzzy finder tool, built around `fzf` and `fd`. It remembers files
you've selected in the past, and past selections are included as
candidates in future searches even if they're not below the current
directory. Pressing `<Ctrl-T>` switches between the default "combined"
mode (history plus local non-hidden files) and the "local" mode (all
local files, including hidden ones).

This is a work in progress, and it's not ready for anyone else to use
yet.

## Vim integration

Here's what I do:

```vim
function OpenFounder()
  let l:filepath = system("founder --tmux --no-newline")
  if v:shell_error == 0
    execute "e ".fnameescape(l:filepath)
  endif
endfunction
nnoremap <C-t> :call OpenFounder()<CR>
autocmd BufEnter * call system("founder add " . fnameescape(@%))
```
