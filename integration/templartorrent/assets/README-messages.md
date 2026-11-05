# Editing the program's messages

Every piece of text TemplarTorrent shows (window and dialog titles, buttons,
labels, column headers, status lines, errors and the command-line help) comes
from one file: `integration/templartorrent/assets/messages.json`.

## The file

It is a flat JSON object with one message per line:

```json
{
  "add.title": "Add download",
  "toolbar.add": "Add",
  "units.kb_per_s": "{value} kB/s"
}
```

The part before the colon is the key the program asks for; the part after it
is the text you see. Change only the text. Keys are grouped by screen
(`add.`, `limits.`, `peers.`, `settings.`, `status.` and so on) and kept in
sorted order, one per line, so you can find and edit them line by line.

Text is a JSON string: write `\"` for a double quote, `\\` for a backslash and
`\n` for a line break. Every line except the last ends with a comma.

## Changing text without rebuilding

The file is built into the program, so it always has every message. To use
your own wording without rebuilding, copy the file, edit the copy, and either

- put it next to the executable as `messages.json` (beside
  `templartorrent.exe` on Windows, or the `templartorrent` binary on Linux), or
- set the environment variable `TEMPLARTORRENT_MESSAGES` to the copy's path.

The variable wins over a file next to the executable. A copy only needs the
keys you want to change; every key it leaves out keeps the built-in text. A
copy that is not valid JSON is ignored as a whole, and the reason is printed
once on standard error. The file is read once at start-up, so restart the
program after editing it.

To change the built-in text itself, edit `assets/messages.json` and rebuild.

## Placeholders

Words in braces, such as `{value}`, `{name}` or `{detail}`, are filled in by
the program: a rate, a file name, an error from a lower layer. You can move a
placeholder anywhere in the sentence, repeat it or leave it out, but keep its
name exactly as written. A placeholder the program does not fill, for example
a misspelt name, is shown as written, so a typo is visible on screen.

To show a literal brace, write it twice: `{{` shows `{` and `}}` shows `}`.

## Unknown keys

If the program asks for a key the file does not have, it shows the key name
itself (for example `toolbar.add`) instead of nothing, so a missing message is
easy to spot. Keys you add that the program never asks for have no effect.
