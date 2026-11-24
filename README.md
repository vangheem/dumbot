<p align="center">
  <img src="assets/logo.png" width="128" height="128" alt="dumBot logo: a smiling robot over a .md tag">
</p>

<h1 align="center">dumBot</h1>

A native desktop app for running your own bots, like a local Grok bot, powered by
[opencode](https://opencode.ai) and a folder of markdown files. Built with
[Rune](https://github.com/anomalyco/rune) (through [a fork](https://github.com/vangheem/rune)),
like [gitsnack](https://github.com/vangheem/gitsnack).

A bot is a folder of markdown. You give it a name and a purpose. It writes its own
instructions, keeps notes, does the job (on your computer, or in your browser through the
browse MCP), and decides when it should run next. The next run starts from whatever it wrote
down.

## How bots work

Each bot lives in `~/.local/share/dumbot/bots/<slug>/`:

- `AGENTS.md`: who the bot is and how it works. Runs use the bot's folder as their working
  directory, so opencode loads it into every session. The bot rewrites it as it learns.
- `memory/*.md`: everything else it keeps (notes, logs, to-dos, things already handled).
- `bot.json`: dumBot's record of the bot's model, schedule and runs.
- `.opencode/agents/dumbot.md`: the `dumbot` opencode agent every run uses, rewritten by
  dumBot before each run. Its system prompt is the bot's operating loop: read memory, do the
  work, keep `AGENTS.md` current as it learns, review itself and add a changelog line, then
  schedule the next run. Its permissions let the bot edit its own folder and run
  `dumbot wake` without asking.

The folder is a git repository, because opencode only loads a folder's `AGENTS.md` and
`.opencode/` agents in a project.

Every run is a new opencode session. Its first prompt says why the bot woke up and passes on
any note its last run left. Bots schedule themselves with:

```sh
dumbot wake <bot> in 30m                     # or: in 2h, in 1d, at 09:00, at 2026-12-01 09:00
dumbot wake <bot> every 6h --note "check the PR queue first"
dumbot wake <bot> off
dumbot list
dumbot run <bot> [message]                   # start a run now, from a script or cron
```

`dumbot wake` writes `bot.json`. The running app re-reads bots every few seconds and starts
the ones that are due. A bot whose last run is still working waits for it to finish. Bots
only wake while the app is running; a wake-up missed while it was closed runs when it next
starts.

## Features

- A list of bots with their next wake-up or running status. **New bot** (⌘N) creates one and
  starts its first run, where it works out how to do its job and picks a schedule.
- The bot's markdown files, rendered, refreshed as runs change them.
- Each run's model, token usage and cost, and its transcript: tool calls, permission requests to allow or reject, a composer to
  message the run, **Stop**, and **Open in terminal** (attaches the opencode TUI).
- **Run now** (with the composer text as a message, if any), **Schedule…** by hand, and
  **Open folder** to edit files yourself.
- A default model (bottom of the sidebar) and a model per bot. Changing either also switches
  the bot's latest run, so a run that's working or that you message next uses it.

## Requirements

- [`opencode`](https://opencode.ai) v2. dumBot uses opencode's shared background service
  (the one the TUI uses) and starts it when it isn't running.
- For browser automation, the browse MCP server configured in opencode.

## Install

```sh
brew install vangheem/tap/dumbot
dumbot install-app        # optional: ~/Applications/dumbot.app (macOS)
```

Prebuilt binaries are on the [releases page](https://github.com/vangheem/dumbot/releases),
and `make install` builds from source into `~/.local/bin`. dumBot checks for new releases
and offers **Update and restart** for Homebrew installs.

## Releasing

Run the **Release new version** workflow (`gh workflow run bump-version.yml -f bump=patch`).
It bumps the version, tags `vX.Y.Z`, and the release workflow builds macOS and Linux
binaries, publishes the GitHub release, and updates the formula in
[vangheem/homebrew-tap](https://github.com/vangheem/homebrew-tap). Every push to `main` also
refreshes a rolling `latest` prerelease. It needs the `RELEASE_DEPLOY_KEY` and
`HOMEBREW_TAP_DEPLOY_KEY` secrets, as gitsnack does.

## Development

```sh
make run
make test
make lint
cargo test -- --ignored        # also hits the local opencode service and GitHub
cargo run -- --snap out.png --scene demo --size 1440x900 --theme dark
```

- `src/main.rs`: app state, the scheduler loop and all views.
- `src/bots.rs`: bot folders, `bot.json`, the `dumbot` agent and starting runs.
- `src/schedule.rs`: parsing and describing wake-up times.
- `src/cli.rs`: `dumbot wake`, `run` and `list`.
- `src/opencode.rs`: client for opencode's local HTTP API.
- `src/store.rs`: app settings (`~/.local/share/dumbot/state.json`).
- `src/update.rs`, `src/appbundle.rs`: self-update and the macOS app bundle.

## License

MIT
