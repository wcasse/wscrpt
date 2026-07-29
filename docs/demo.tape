# README demo — render with: vhs docs/demo.tape   (needs charmbracelet/vhs)
# Re-record on each release so the GIF never drifts from the shipped UI.
# Assumes a release build exists: cargo build --release

Output docs/assets/demo.gif

Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 12
Set TypingSpeed 70ms

# Skip the one-time first-run overlay so Esc sequences reach the editor.
Hide
Type "export WSCRPT_SKIP_FIRST_RUN_HELP=1 PATH=$PWD/target/release:$PATH"
Enter
Type "clear"
Enter
Show

# Open the workspace. First frame does not wait for indexing.
Type "wscrpt ."
Enter
Sleep 2.5s

# Quick Open: fuzzy-find a file.
Escape
Type "o"
Sleep 800ms
Type "render"
Sleep 1.2s
Enter
Sleep 2s

# Project sidebar.
Escape
Type "wS"
Sleep 2s

# Project-wide search.
Escape
Type "ws"
Sleep 800ms
Type "passthrough"
Sleep 1.2s
Enter
Sleep 2.5s

# Command palette, then close it.
Escape
Type " "
Sleep 2s
Ctrl+G
Sleep 1s

# Quit cleanly (terminal modes restore on exit).
Escape
Type "q"
Sleep 1.5s
