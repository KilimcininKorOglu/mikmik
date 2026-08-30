# MikMik Keybindings Reference

This document covers the keyboard shortcuts in MikMik, how to customize them, vim mode, and the special input behaviours.

The source of truth is `crates/core/src/keybindings.rs` for the bindings themselves and `crates/tui/src/app.rs` for the keys the dialogs handle on their own.

---

## Table of Contents

1. [Default Keybindings](#default-keybindings)
   - [Global Context](#global-context)
   - [Chat Context](#chat-context)
2. [Keys Handled Outside the Binding System](#keys-handled-outside-the-binding-system)
   - [Permission Dialogs](#permission-dialogs)
   - [Clipboard](#clipboard)
3. [Keybinding Contexts](#keybinding-contexts)
4. [Customizing Keybindings](#customizing-keybindings)
   - [The /keybindings command](#the-keybindings-command)
   - [The keybindings.json format](#the-keybindingsjson-format)
   - [Chords](#chords)
   - [Schema Versioning and Smart Merge](#schema-versioning-and-smart-merge)
5. [Non-Rebindable Keys](#non-rebindable-keys)
6. [Vim Mode](#vim-mode)
7. [Special Input Behaviors](#special-input-behaviors)
   - [Newline](#newline)
   - [ESC During Streaming](#esc-during-streaming)
   - [Pasting](#pasting)
   - [@file Injection with Typeahead](#file-injection-with-typeahead)
8. [Cyrillic Keyboard Layouts](#cyrillic-keyboard-layouts)

---

## Default Keybindings

### Global Context

These bindings resolve in every context.

| Key            | Action         | Description                                               |
|----------------|----------------|-----------------------------------------------------------|
| `Ctrl+L`       | redraw         | Redraw the terminal screen                                |
| `Ctrl+R`       | historySearch  | Open interactive history search                           |
| `Ctrl+B`       | createBranch   | Open the session branch screen                            |
| `Ctrl+Shift+L` | toggleTimeline | Show the execution timeline panel, focus it, then hide it |
| `Alt+H`        | openHelp       | Toggle the help panel                                     |

`Ctrl+Shift+L` needs `timelineEnabled` (see
[Configuration](configuration.md#interface)); while the setting is off it says
so and does nothing. Once the panel has focus, `Up` and `Down` move its cursor,
`Right` expands the selected row, `Left` collapses it and `Esc` returns to the
prompt. `Enter` always submits the prompt, never the panel.

`Ctrl+L` is bound twice: `redraw` in Global and `clearLine` in Chat. The Chat
binding wins while the prompt has focus.

### Chat Context

These bindings resolve when focus is in the chat input field.

| Key                                     | Action              | Description                                                                  |
|-----------------------------------------|---------------------|------------------------------------------------------------------------------|
| `Enter`                                 | submit              | Submit the current message to the model                                      |
| `Shift+Enter`                           | newline             | Insert a literal newline without submitting                                  |
| `Ctrl+J`                                | newline             | Newline (fallback for terminals without the kitty keyboard protocol)         |
| `Alt+Enter`                             | newline             | Newline (second fallback)                                                    |
| `Up` / `Ctrl+O`                         | historyPrev         | Navigate to the previous message in input history                            |
| `Down` / `Ctrl+I`                       | historyNext         | Navigate to the next message in input history                                |
| `Tab`                                   | indent              | Accept a completion if one is open; on an empty prompt, cycle the agent mode  |
| `Shift+Tab`                             | reverseIndent       | Remove one level of indentation; on an empty prompt, cycle the permission mode |
| `Page Up`                               | scrollUp            | Scroll the conversation view up one page                                     |
| `Page Down`                             | scrollDown          | Scroll the conversation view down one page                                   |
| `Home` / `Cmd+Left` / `Ctrl+A`          | goLineStart         | Move cursor to beginning of line                                             |
| `End` / `Cmd+Right` / `Ctrl+E`          | goLineEnd           | Move cursor to end of line                                                   |
| `Ctrl+Left` / `Alt+B`                   | moveWordBackward    | Move one word left                                                           |
| `Ctrl+Right` / `Alt+F`                  | moveWordForward     | Move one word right                                                          |
| `Alt+Left`                              | previousMessage     | Jump to previous user/assistant message                                      |
| `Alt+Right`                             | nextMessage         | Jump to next user/assistant message                                          |
| `Ctrl+Shift+A`                          | openModelPicker     | Open the interactive model picker                                            |
| `Ctrl+K`                                | openCommandPalette  | Open the slash command palette                                               |
| `Ctrl+U`                                | killToStart         | Delete from cursor to beginning of line                                      |
| `Ctrl+W` / `Alt+Backspace` / `Ctrl+Backspace` | killWord      | Delete the word before the cursor                                            |
| `Alt+D` / `Ctrl+Delete` / `Alt+Delete`  | deleteWord          | Delete the word after the cursor                                             |
| `Ctrl+H`                                | deleteCharBefore    | Delete character before cursor                                               |
| `Ctrl+L`                                | clearLine           | Clear current input line                                                     |
| `Ctrl+Y`                                | yank                | Paste the last killed text (kill ring)                                       |
| `Alt+Y`                                 | yankPop             | Cycle backwards through the kill ring                                        |
| `Alt+E`                                 | expandPaste         | Expand a `[Pasted text #N ...]` placeholder back into the full body          |
| `Ctrl+F`                                | findInMessage       | Open inline search within the current conversation                           |
| `Ctrl+Shift+F`                          | globalSearch        | Open global codebase search                                                  |
| `F3` / `Ctrl+]`                         | findNext            | Jump to next search match                                                    |
| `Shift+F3` / `Ctrl+[`                   | findPrev            | Jump to previous search match                                                |
| `Ctrl+G`                                | goToLine            | Jump to a specific line                                                      |
| `Ctrl+.`                                | jumpToNextError     | Jump to next error / issue                                                   |
| `Ctrl+Shift+.`                          | jumpToPreviousError | Jump to previous error / issue                                               |

`Tab` and `Shift+Tab` each do a second job when the prompt is empty and no
turn is running. `Tab` cycles the agent mode between `build` and `plan`, which
decides which tools are offered. `Shift+Tab` cycles the permission mode through
`default` → `acceptEdits` → `bypassPermissions` → `default`, which decides what
a tool may do without asking; from `plan` it returns to `default`. Switching
into `bypassPermissions` raises the warning described in
[Configuration](configuration.md#bypasspermissions).

> `Ctrl+A` previously opened the model picker; it now moves the cursor to the line start (matching Emacs/readline). The model picker is now `Ctrl+Shift+A`. Old `keybindings.json` files are auto-migrated.

---

## Keys Handled Outside the Binding System

Overlays and dialogs read their keys directly and return before the resolver
runs. `default_bindings()` also carries entries for the `Confirmation`, `Help`,
`HistorySearch`, `Transcript`, `MessageSelector`, `ThemePicker`, `Task`,
`DiffDialog`, `Select`, `Plugin` and `Attachments` contexts, but those entries
never fire, because the dialog blocks consume the keystroke first. Rebinding
them has no effect.

### Permission Dialogs

A permission dialog builds its own option list, and each option carries a key
character:

| Key       | Option                                                       |
|-----------|--------------------------------------------------------------|
| `y`       | Yes, allow once                                              |
| `Y`       | Yes, allow this session                                      |
| `p`       | Yes, always allow (persistent)                               |
| `n`       | No, deny                                                     |
| `P`       | Allow commands matching `<prefix>*` (Bash dialogs only)      |
| `1` … `9` | Select the option at that position without confirming it     |
| `Up` / `Down` | Move the selection                                       |
| `Enter`   | Accept the highlighted option                                |
| `Escape`  | Cancel the prompt and deny the action                        |

A digit only moves the selection. A letter key both selects and closes the
dialog.

### Clipboard

| Key                | Behaviour                                                                |
|--------------------|--------------------------------------------------------------------------|
| `Ctrl+C`           | Copy the selection if there is one; otherwise cancel the stream; otherwise start the exit sequence |
| `Ctrl+V` / `Cmd+V` | Paste an image, or text, from the system clipboard                       |
| `Shift+Insert`     | Paste the primary selection                                              |

---

## Keybinding Contexts

MikMik carries a context system so the same key can have different effects
depending on where focus is. `KeyContext` has eighteen variants; the resolver
matches a binding when its context equals the current one or is `Global`.

| Context           | Resolves defaults | Description                                       |
|-------------------|-------------------|---------------------------------------------------|
| `Global`          | yes               | Always active regardless of focus                 |
| `Chat`            | yes               | Active when the chat input field has focus        |
| `Confirmation`    | no                | Permission dialog, import dialog, rewind flow     |
| `Settings`        | no                | Settings screen                                   |
| `ThemePicker`     | no                | Theme screen                                      |
| `Help`            | no                | Help overlay                                      |
| `HistorySearch`   | no                | History search overlay                            |
| `DiffDialog`      | no                | Diff viewer                                       |
| `Select`          | no                | Agents menu, MCP view, stats dialog               |
| `Transcript`      | no                | Transcript pane                                   |
| `MessageSelector` | no                | Message selector overlay                          |
| `Task`            | no                | Task list                                         |
| `Plugin`          | no                | Plugin picker                                     |
| `Attachments`     | no                | Attachment list                                   |
| `Autocomplete`    | no                | Declared, carries no defaults                     |
| `Tabs`            | no                | Declared, carries no defaults                     |
| `Footer`          | no                | Declared, carries no defaults                     |
| `ModelPicker`     | no                | Declared, carries no defaults                     |

Context names in `keybindings.json` are PascalCase: `"Global"`, `"Chat"`. A
name the parser does not recognise falls back to `Global`.

---

## Customizing Keybindings

### The /keybindings command

```
/keybindings
```

The command creates `~/.config/mikmik/keybindings.json` from a template if the
file does not exist, then opens it in your system editor. It is not an
interactive editor; you edit the JSON by hand.

### The keybindings.json format

```json
{
  "schema_version": 1,
  "bindings": [
    {
      "context": "Chat",
      "action": "submit",
      "chord": "ctrl+enter"
    },
    {
      "context": "Global",
      "action": "historySearch",
      "chord": "ctrl+p"
    }
  ]
}
```

Each binding object has:

| Field     | Type           | Description                                    |
|-----------|----------------|------------------------------------------------|
| `context` | string \| null | Keybinding context (see table above)           |
| `action`  | string \| null | Action identifier (or `null` to unbind)        |
| `chord`   | string         | One or more keystrokes, separated by spaces    |

Setting `"action": null` for a chord explicitly **unbinds** the default. The
resolver reports the chord as claimed and does nothing, so the key stops firing
its default action instead of falling through.

User bindings are appended after the defaults and the last match wins, so a user
binding always beats the default for the same chord and context.

Key notation uses lowercase letters, with modifier prefixes separated by `+`:

| Prefix   | Modifier key | Accepted spellings                       |
|----------|--------------|------------------------------------------|
| `ctrl+`  | Control      | `ctrl`, `control`                        |
| `alt+`   | Alt / Option | `alt`, `opt`, `option`                   |
| `shift+` | Shift        | `shift`                                  |
| `meta+`  | Super / Cmd  | `meta`, `cmd`, `command`, `super`, `win` |

Special key names: `enter` (`return`), `escape` (`esc`), `tab`, `backspace`
(`bs`), `delete` (`del`), `space`, `up`, `down`, `left`, `right`, `home`, `end`,
`pageup` (`pgup`), `pagedown` (`pgdn`, `pgdown`), and function keys such as
`f3`.

The file is read at startup. Restart MikMik after editing it.

### Chords

A chord is a multi-key sequence written as one string with the keystrokes
separated by spaces:

```json
{
  "context": "Chat",
  "action": "openModelPicker",
  "chord": "ctrl+x ctrl+m"
}
```

The first keystroke acts as the leader. After it, the resolver holds the
sequence and reports `Pending`. If the next keystroke continues a known chord,
the chord fires; if it matches nothing, the pending sequence is dropped and the
keystroke is treated as unmatched.

There is **no timeout**. A pending chord waits until the next keystroke arrives
or the caller cancels it. There is no depth limit either; a chord may carry as
many keystrokes as you write.

### Schema Versioning and Smart Merge

`keybindings.json` carries a top-level `schema_version` field (currently `1`).
When the file is older than the bundled `KEYBINDINGS_SCHEMA_VERSION`, MikMik
runs a smart merge on load:

1. Two known stale defaults are dropped: `ctrl+a → openModelPicker` (moved to `ctrl+shift+a`) and `tab → togglePreview` in the Chat context (now `indent`).
2. Every remaining binding in your file is recorded as a customization, keyed by its chord.
3. The current defaults are walked. Where your file names the same chord, your action wins; otherwise the default is written.
4. Customizations whose chord is not in the defaults are appended with `context` set to `null`, which resolves to `Global`.
5. The merged file is written back with the new `schema_version`.

A warning is logged whenever a migration occurs.

The merge keys on the chord alone, not on the chord and context together. A
chord that carries different actions in two contexts collapses to one action.

---

## Non-Rebindable Keys

The following keys have fixed behaviour and cannot be rebound:

| Key      | Fixed behaviour                                                           |
|----------|---------------------------------------------------------------------------|
| `Ctrl+C` | Copy the selection, cancel the stream, or start the exit sequence         |
| `Ctrl+D` | Exit MikMik when input is empty                                           |
| `Ctrl+M` | Identical to `Enter` at the terminal level (terminals emit `CR` for both) |

`Ctrl+C` and `Ctrl+D` are deliberately absent from `default_bindings()`; the TUI
handles them directly so it can implement the two-press confirmation.

If any of the three appear as a `chord` in `keybindings.json`, MikMik:

1. Logs a warning (`Cannot rebind protected key '<chord>' in keybindings.json`).
2. **Filters the binding out** of the loaded set before resolving any keystrokes.

The filter compares the whole chord string, so a chord that merely *starts* with
a protected key (for example `"ctrl+c x"`) is not filtered.

---

## Vim Mode

Vim mode replaces the default line editor with a modal input field.

### Enabling Vim Mode

```
/vim          # toggle
/vim on
/vim off
```

The setting is persisted to `~/.config/mikmik/ui-settings.json` as
`editor_mode`. **Restart the REPL for the change to take effect.**

`/config set vim` is not a valid key; `/config` only takes `theme`,
`output-style`, `model` and `permission-mode`.

### Modes

| Mode         | Indicator            | Entered with                                |
|--------------|----------------------|---------------------------------------------|
| Insert       | `-- INSERT --`       | `i`, `a`, `I`, `A`, `o`, `O`, `c`, `s`, `S` |
| Normal       | `-- NORMAL --`       | `Escape`                                    |
| Visual       | `-- VISUAL --`       | `v`                                         |
| Visual line  | `-- VISUAL LINE --`  | `V`                                         |
| Visual block | `-- VISUAL BLOCK --` | `Ctrl+V`                                    |
| Command      | `-- COMMAND --`      | `:`                                         |
| Search       | `-- SEARCH --`       | `/`                                         |

The indicator is drawn in the status line. `INSERT` is dim; the other modes are
bold and coloured. A custom status line that renders `vim.mode` itself can
suppress the built-in indicator with `hideVimModeIndicator`, so the mode is not
shown twice.

### Normal Mode

**Motions.** `h`, `l`, `0`, `^`, `$`, `w`, `b`, `e`, `W`, `B`, `E`, `G`, `gg`,
`f<char>`, `F<char>`, `t<char>`, `T<char>`, `;`, `,`.

**Operators.** `d`, `c`, `y` take a motion (`dw`, `c$`, `yb`). `gu` and `gU`
convert case. `dd` and `yy` (or `Y`) act on the whole line.

**Edits.** `x`, `X`, `D`, `C`, `s`, `S`, `r<char>`, `~`, `>`, `<`, `J`, `p`,
`P`, `o`, `O`.

**Counts.** A leading number repeats the command: `3w`, `2dd`.

**Other.** `u` undoes the last change (the undo stack holds 100 snapshots).
`n` and `N` repeat the last `/` search forward and backward. `"`, `m`, `'`, `q`
and `@` start register, mark and macro commands.

There is **no redo**. `Ctrl+R` is not bound in vim mode. `j` and `k` are not
bound either; use `Up` and `Down` for input history.

### Visual Modes

| Key       | Action                                        |
|-----------|-----------------------------------------------|
| `y`       | Yank the selection                            |
| `d` / `x` | Delete the selection                          |
| `c`       | Delete the selection and enter insert mode    |
| motions   | Extend the selection                          |
| `Escape`  | Return to normal mode                         |

In visual line mode the three operators act on whole lines. The prompt is a
single text buffer, so visual block behaves like character-wise visual.

---

## Special Input Behaviors

### Newline

`Shift+Enter` inserts a literal newline without submitting. `Ctrl+J` and
`Alt+Enter` do the same, as fallbacks for terminals that do not support the
kitty keyboard protocol: without the protocol, `Shift+Enter` arrives as a raw
`0x0A` byte, which crossterm reports as `Ctrl+J`.

Plain `Enter` always submits, regardless of how many lines the input already
holds. In vim insert mode `Enter` also submits.

### ESC During Streaming

`Escape` while the model is streaming cancels the turn. `Ctrl+C` with no
selection does exactly the same thing: both clear the streamed text, the
streamed thinking and the tool blocks, mark the turn cancelled in the timeline,
and complete the turn snapshot. Neither one preserves the partial response in
the view.

### Pasting

`Ctrl+V` (or `Cmd+V`) reads the system clipboard. An image on the clipboard is
attached to the message; text is inserted into the prompt. `Shift+Insert` pastes
the primary selection and works in terminals that never send `Ctrl+V` through.

In vim normal, visual and visual-block modes `Ctrl+V` is not a paste: it enters
visual block mode.

A paste that arrives as a flood of key events, which is what happens when a
terminal has no bracketed paste, is collected as one paste rather than typed key
by key, so a trailing newline cannot submit the message before you have read it.

When nothing answers, MikMik says so and what to do about it. Over SSH the usual
fix is to set `mouseCapture` to `false` (see
[Configuration](configuration.md#interface)) so the terminal's own paste works
again.

### @file Injection with Typeahead

Type `@` followed by a path to attach a file's contents to your message. As you
type after the `@`, MikMik opens a typeahead completion overlay scanning the
current working directory.

```
explain @src/main.rs and compare to @tests/integration.rs
```

When you press `Enter`, MikMik:

1. Splits the message on whitespace and takes every word that starts with `@`.
2. Strips trailing ASCII punctuation from the token, but never a trailing `/`.
3. Resolves the path: `~/` expands to your home directory, a leading `/` is absolute, anything else is relative to the working directory.
4. Reads each file and **prepends** the contents to the message as a separate text block, one `<file path="...">…</file>` block per file.

The `@` token stays in the message text. The contents are added in front of it,
not substituted in place of it.

The `@` reference works with:

- Plain absolute paths: `@/etc/hosts`
- Paths relative to cwd: `@src/main.rs`
- Home-relative paths: `@~/.bashrc`
- Trailing punctuation is stripped: `@src/main.rs.` is treated as `@src/main.rs`

An `@` inside a word (for example an email address, `me@example.com`) never
matches, because the scan only looks at words that begin with `@`. A path that
does not exist is skipped silently.

**Limits and warnings.** If a referenced path is too large, binary, or a
directory, MikMik opens a confirmation dialog before sending:

| Issue                   | Behaviour                                                   |
|-------------------------|-------------------------------------------------------------|
| File exceeds size limit | Dialog offers "Allow anyway" or "Abort"                     |
| Binary file             | Dialog warns; same choice                                   |
| Path is a directory     | Dialog warns; "Allow anyway" drops the directory ref         |
| Path does not exist     | Skipped silently, no dialog                                 |

Directories take precedence: when a directory ref and an oversized file ref are
both present, the dialog lists only the directories. "Allow anyway" re-submits
with the size limit set to 0, so every remaining file is injected whatever its
size.

Files that pass all checks are injected silently, with no dialog.

**Configuration.** Two settings in `~/.config/mikmik/settings.json`:

| Setting                | Default | Description                                                    |
|------------------------|---------|----------------------------------------------------------------|
| `fileInjectionEnabled` | `true`  | Master switch; set to `false` to disable @-injection entirely  |
| `fileInjectionMaxSize` | `100`   | Per-file size limit in KB; `0` disables the check (accept all) |

Both live under `config` and are also accepted as legacy top-level keys. They
can be edited in the in-app settings screen.

**Typeahead navigation.** While the completion overlay is open:

| Key             | Action                                                    |
|-----------------|-----------------------------------------------------------|
| `Up` / `Down`   | Move selection                                            |
| `Tab` / `Enter` | Insert the highlighted completion and append a space      |
| `Escape`        | Dismiss the overlay (keep typed text)                     |

The same overlay serves slash commands and history recall. `Enter` on a slash
command completes **and runs** it; `Enter` on a file path or a history entry
completes it and keeps the prompt open.

---

## Cyrillic Keyboard Layouts

Terminal key events for `Ctrl+<key>` combinations report the character the
active layout produces, not the Latin letter printed on a QWERTY keycap. On a
Russian or Ukrainian JCUKEN layout, `Ctrl+С` arrives as the Cyrillic `с`, so a
lookup for `ctrl+c` fails.

MikMik maps the reported character back to the Latin letter at the same physical
QWERTY position before the keybinding lookup. The mapping covers the Russian and
Ukrainian JCUKEN letter rows, which share the same physical-key layout, plus the
Ukrainian-specific `і`, `ї` and `є`.

A character outside the mapping is passed through lowercased and unchanged. This
is safe: an unmapped character simply matches no binding. **Other non-Latin
layouts (Greek, Arabic, Hebrew, CJK) are not mapped.**

### Shift Normalization

A separate mapping handles the kitty keyboard protocol, which sends the
unshifted character plus a Shift flag (`Shift+1` arrives as `1` + `SHIFT`
instead of `!`). MikMik applies a US QWERTY shift map to recover the shifted
character, and only when the protocol is active. On AZERTY, QWERTZ and other
layouts this map is wrong, so MikMik relies on the terminal to send the
correctly shifted character, which modern terminals do.
