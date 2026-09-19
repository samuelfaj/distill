# Your First Prompt

Remote-Code is a conversation with an agent that can read your code, run
commands, and edit files — right here in your terminal.

Type what you want and press `Enter`. Grok streams its work into the
**scrollback** above the prompt: responses, shell commands, file edits.

## Keep typing while Grok works

While a turn is running, `Enter` **queues** your next message instead of
interrupting. Change your mind? Press `Enter` on the empty prompt to stop
the current turn and send the queued message right away.

## You are always in control

- **`Ctrl+C`** — cancel a running turn (with a draft, the first press only clears it). `Esc` does not cancel; it reminds you to use `Ctrl+C`.
- **`Esc Esc`** while idle — clear the prompt; with an empty prompt, open
  the rewind picker instead. Cleared something by accident? `Ctrl+Z` undoes.
- **`Ctrl+Q`** — quit (`Ctrl+D` in VS Code-family terminals), press twice.

The **shortcuts bar** at the bottom always shows the keys relevant to what
you're doing right now — when in doubt, look down.

*Go deeper: `/docs Getting Started`*
