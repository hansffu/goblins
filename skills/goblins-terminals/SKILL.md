---
name: goblins-terminals
description: Run long-lived programs in named terminals of this Goblins sandbox with goblin terminal, and read, type into and wait on them. Use for dev servers, watchers, long builds, anything the human may want to watch or take over, and anything that needs input or a prompt answered; also when the human mentions their terminals, or an inbox notice says a terminal exited.
---

A terminal is a named program on a PTY that keeps running whether or not
anyone is attached. Use one instead of a plain command when the program runs
for a long time or forever (a dev server, a file watcher, a long build or test
run), when the human may want to watch it or take it over, or when it needs
input: a prompt, a REPL, a debugger, `ssh`, `sudo`. Use a plain command for
anything short that finishes by itself and whose output you simply need.

The human sees your terminals in their sidebar and can attach to them, read
what they show and type into them. They can create terminals in your sandbox
too. `goblin terminal list` shows all of them: its ORIGIN column says `agent`
for yours and `host` for theirs. You can read, type into and wait on any of
them, but you can close only yours; if the human's is in the way, ask them.
You cannot attach, and other goblins' terminals are not reachable at all.

## Commands

```
goblin terminal create [--name NAME] [--cwd DIR] [--env NAME=VALUE]... [--size ROWSxCOLS] [--notify] [-- ARGV...]
goblin terminal list
goblin terminal read TERMINAL [--source visible|recent] [--lines N]
goblin terminal send TERMINAL [--text TEXT] [--keys KEY...]
goblin terminal run TERMINAL COMMAND
goblin terminal wait TERMINAL (--match TEXT | --regex RE | --exit) [--timeout MS] [--lines N]
goblin terminal close TERMINAL
```

`create` prints the terminal's ID (`t1`, `t2`, …). TERMINAL is an ID or a name.
Without `--` and a program it starts the sandbox's own shell, with the
environment of your other commands; give a program and arguments after `--` to
run it directly, in which case the terminal ends when the program does. A goblin
has at most 16 terminals.

## How to use them well

- Name every terminal you create (`--name server`, `--name tests`): the human
  sees the name in their sidebar, and you can use it instead of an ID. Names
  are lowercase letters, digits, `-` and `_`, start with a letter, and are not
  of the form `t1`.
- Read before you type. `goblin terminal read TERMINAL` prints the screen as it
  is now; `--source recent --lines 200` adds the scrollback. Look at the prompt
  or the program's state before sending anything, since the human may have
  typed there too.
- To run commands in a shell, create a terminal without a program and use
  `goblin terminal run TERMINAL 'COMMAND'`, which types the line and presses
  enter. The shell and its output stay readable after the command ends. Run a
  program directly (`create -- gradle run`) only when you want the terminal to
  go away with it.
- Use `send` for anything that is not a command line: `--text` types text first
  (as a paste when the program asked for bracketed paste), then `--keys` presses
  keys. Keys are `enter`, `tab`, `esc`, `backspace`, `space`, `up`, `down`,
  `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `insert`, `delete`,
  `f1` to `f12`, `shift+tab`, `ctrl+LETTER`, `alt+CHARACTER`, and any single
  character that is not a control character (`g`, `A`, `:`, `/`), which is
  typed as that keystroke and never as a paste. For example, `send t1 --keys
  ctrl+c` interrupts a program, and `send t1 --text y --keys enter` answers a
  prompt. An entry that is not a key name and not one character, like `gg`,
  refuses the whole call.
- Drive full-screen programs with keys, and put text in with `--text`: navigate
  and run commands with keys, so each character is a keystroke the program acts
  on; insert with text. In Vim:

  ```
  goblin terminal send T --keys g g A                # first line, append at its end
  goblin terminal send T --text ' START' --keys esc  # type the text, leave insert mode
  goblin terminal send T --keys : w q enter          # save and quit
  ```

- Wait instead of polling or sleeping. `wait` returns when a line of the
  screen, or of the last `--lines` recent lines (default 100), contains
  `--match TEXT` or matches `--regex RE`, and output that is already there
  counts. Anchor regular expressions (`'^BUILD SUCCESSFUL$'`, `'^Listening on'`)
  so the echoed command line, which holds your pattern, does not match. `wait
  --exit` returns when the program ends and prints what it left on screen. The
  default timeout is 30 seconds (`--timeout MS`, at most an hour). It exits 0
  on a match or an exit and 1 on a timeout; on a timeout read the terminal
  and decide, do not just wait again.
- For something that will take long, ask to be told: `create --notify` puts one
  notice in your inbox when the program exits (not when you close the terminal
  or the goblin stops). It says whether it exited with a code or a signal and
  quotes the last lines of its screen. A notice is an operation notice like the
  ones described in [goblins-messaging](../goblins-messaging/SKILL.md): claim
  it with `goblin inbox next` and complete it with `goblin inbox complete`.
  End your turn when there is nothing else to do and the notice will wake you,
  instead of holding a command open.
- Do not leave terminals running that nothing needs: `goblin terminal close
  TERMINAL` ends the program and everything it started. A terminal also ends
  by itself when its program exits and when the goblin stops, and it is gone
  from the list then. Terminals do not come back when a goblin is recreated.

## Limits and errors

- `--cwd` and `--env` are as for any command in your sandbox; a configuration
  that cannot run commands cannot create terminals.
- `read` and `wait` see only what the program drew: a full-screen program such
  as `vim` is read as its screen. Output is bounded; a cut result says
  `truncated`.
- A terminal's screen can hold secrets the human typed. Do not copy what you
  read from their terminals anywhere you would not copy it from their screen.
