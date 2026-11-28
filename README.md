<p align="center">
  <img src="assets/logo.png" width="128" height="128" alt="dumBot logo: a smiling robot over a .md tag">
</p>

<h1 align="center">dumBot</h1>

A native desktop app for running your own bots, like a local Grok bot, powered by
[opencode](https://opencode.ai) and a folder of markdown files. Built with
[Rune](https://github.com/anomalyco/rune),
like [gitsnack](https://github.com/vangheem/gitsnack).

A bot is a folder of markdown. You give it a name and a purpose. It writes its own
instructions, keeps notes, does the job (on your computer, or in your browser through the
browse MCP), and decides when it should run next. The next run starts from whatever it wrote
down.

## Install

On macOS or Linux (Ubuntu, Debian, Fedora, …):

```sh
curl -fsSL https://raw.githubusercontent.com/vangheem/dumbot/main/install.sh | sh
```

It downloads the latest release for your machine, checks its SHA-256 and installs `dumbot` to
`~/.local/bin`. Options (after `sh -s --`): `--server` installs it as a server (below),
`--cli` the command line only, `--version v0.3.0` a given release, `--dir DIR` somewhere
else. On macOS, Homebrew works too:

```sh
brew install vangheem/tap/dumbot
dumbot install-app        # optional: ~/Applications/dumbot.app (macOS)
```

To run your bots on another machine (a home server, a VPS), install it there as a server:

```sh
curl -fsSL https://raw.githubusercontent.com/vangheem/dumbot/main/install.sh | sh -s -- --server
```

It runs at boot (systemd or launchd) on port 7457; then connect to it from the app (see
[Bots on a server](#bots-on-a-server)).

### Requirements

- [Docker](https://www.docker.com/) (Docker Desktop on macOS). Every run is in a container.
- [`opencode`](https://opencode.ai) v2 where you run the app, set up with your providers:
  bots use its config and logins (a server gets them from the app).
- Strongly recommended: [Browse](https://opencode.ai/product/browse) from opencode, so bots
  can use your browser ([how dumBot uses it](#browser-automation-with-browse)).

### Updates

The app checks GitHub for new releases every few hours and offers **Update and restart**
(through Homebrew for Homebrew installs; otherwise it downloads the release, checks its
SHA-256 and replaces itself). A server updates itself: it installs each new release and
restarts (`DUMBOT_NO_AUTO_UPDATE=1` in its service turns that off). Prebuilt binaries are also
on the [releases page](https://github.com/vangheem/dumbot/releases), and `make install` builds
from source into `~/.local/bin`.

## Getting started

1. Run `dumbot` to open the app.
2. Set up [Browse](https://opencode.ai/product/browse) so bots can use your browser.
3. **New bot** (⌘N): give it a name and describe what it's for ("Every morning, check my
   Gmail, archive newsletters and summarize what needs a reply"). Its first run works out
   how to do the job, rewrites its own instructions and picks its schedule.
4. Give it what it needs: secrets as **Environment…** variables, documents with **Upload
   files…**, tools under **Container…**.

## Features

- A list of bots with their next wake-up or running status. **New bot** (⌘N) creates one and
  starts its first run, where it works out how to do its job and picks a schedule.
- The bot's markdown files, rendered, refreshed as runs change them.
- Each run's model, token usage and cost, and its transcript: tool calls, permission requests to allow or reject, a composer to
  message the run, and **Stop**.
- A **Schedule** tab listing each wake-up (a one-off, an interval, each weekly rule) with its
  next time and note, to remove any of them or add one: once on a day and time, weekly on
  chosen days, or repeating every so many minutes, hours or days.
- **Run now** (with the composer text as a message, if any).
- **Upload files…** / **Upload folder…** (or drop them on the bot) copy files into the bot's
  `files/`, wherever it runs, for it to use and refer to; the Files tab lists them.
- A default model (bottom of the sidebar) and a model per bot. Changing either also switches
  the bot's latest run, so a run that's working or that you message next uses it.

## Bots on a server

The app can manage bots that run on another machine (a home server, a VPS), which runs them on
schedule while your computer is off or the app is closed.

```sh
# On the server (Linux with systemd, or macOS), with Docker:
dumbot install-server          # runs `dumbot server` now and at boot (port 7457)
dumbot server approve <code>   # pairs an app (the app shows the code)
dumbot server clients          # paired apps; `dumbot server revoke <fingerprint>` unpairs
```

In the app, **Server…** (bottom of the sidebar) connects to it (or `dumbot connect <host>`).

- **Pairing**: each machine has its own key and self-signed certificate. The first time an app
  connects, the server doesn't know it; the app shows a code made from both keys, which you
  approve on the server with `dumbot server approve <code>`. From then on all traffic is TLS
  between those two keys only: the app pins the server's, the server only answers approved
  apps. Someone in the middle would change a key, so the codes wouldn't match.
- **Moving bots**: once paired, the app offers to move this computer's bots there (their
  folders, schedules, settings and own variables; not their run history).
- **Your setup**: bots on a server use your computer's opencode config (models, agents,
  skills, MCP servers), logins, opencode version, GitHub CLI token, git identity and time
  zone, which the app sends when it connects and every 10 minutes. The server keeps the latest
  (owner-only) for runs while the app is closed.
- **Browse** is carried back to your computer: the server listens on your relay's port on its
  loopback and the app tunnels each connection (over the same TLS) to your browse relay, so
  bots on the server use your browser, and hand tabs back to you, while the app is open.

## Browser automation with browse

Most useful bots work on websites: your email, a fantasy league, a store, an admin
dashboard. Many of those sites have no API, or need you logged in.
[**Browse**](https://opencode.ai/product/browse) from opencode is what makes those bots
possible. It's an MCP server and a Chromium extension that let agents use your real browser
(Chrome, Edge, Brave, Arc and others), with your tabs, logins and extensions. Agents open tabs
in the background, in collapsed groups out of your way, and read pages, click, type and take
screenshots. When it's your turn (a login, 2FA or a payment), they hand the tab back to you
and continue once you click Continue.

dumBot builds on it:

- Every bot's instructions tell it to use the browse tools for anything on a website, and to
  write down what it learns about a site (URLs, page layout, what to click) in its
  `AGENTS.md`, so later runs go straight to the right page.
- Subagents each open their own tabs, so a bot can research several pages at once.
- Bots can turn browser routines into skills and scripts that later runs reuse.

To set it up, install the
[browse extension](https://chromewebstore.google.com/detail/mfnicocicmmlkpjnaffgihfjhdgjkdjg)
from the Chrome Web Store; it walks you through the rest, including adding browse to opencode.
See [opencode.ai/product/browse](https://opencode.ai/product/browse) for details and
[github.com/anomalyco/browse](https://github.com/anomalyco/browse) for the source. Bots
without browse still work, but only with what the shell and simple web fetches can reach.

## Instructions, environment variables and scripts

- **Instructions for all bots** (sidebar) is markdown added to every bot's system prompt, and
  its subagents'. Use it for what every bot should know: "I have Gotify at …; the token is in
  `$GOTIFY_TOKEN`", your time zone, house rules. It's kept in
  `~/.local/share/dumbot/instructions.md`.
- **Environment variables**, global (sidebar) or per bot (**Environment…**), are for secrets
  and settings. They live in `~/.local/share/dumbot/env.json` (owner-only), outside every
  bot's folder; each bot's container sees only its own. Each bot folder's opencode config
  sets its `shell` to the container's `dumbot-shell` (running `dumbot shell <slug>`), so every
  shell command the bot or its subagents run gets them. Bots are told only the names. The wrapper
  replaces any value in command output with `[$NAME]`, and permission rules block reading
  `env.json` and dumping the environment. A bot could still put a value somewhere outside
  command output (say, write it into a file and open that with the read tool); its
  instructions forbid that, but it isn't enforced.
- **Scripts**: bots keep reusable scripts in `scripts/`, which they can run without asking.
  Scripts get the environment variables and show up in the app.

## MCP servers for bots

Besides the MCP servers in your opencode config (turned on per bot under **Container…**),
bots can have MCP servers of their own, in opencode's `mcp` format: for every bot (**MCP
servers for bots** in the sidebar) or one bot (**Container…**). They're kept in
`~/.local/share/dumbot/mcp.json` (owner-only) where the bots run, and only bots get them.

Bots can also install MCP servers for themselves: `dumbot mcp <bot> add <name> --url …` or
`… add <name> -- npx -y <package>`, kept in the bot's `mcp.json` and on from its next run
(its container restarts opencode with them). Values can use `{env:NAME}` for the bot's
environment variables, filled in when dumBot writes the bot's opencode config, which bots
aren't allowed to read. The image has Node.js (npx) and uv (uvx); what a bot installs
itself (`npm install -g`, `pip install --user`, `uv tool install`) goes to its home, which
outlives its container.

## How bots work

Each bot lives in `~/.local/share/dumbot/bots/<slug>/`:

- `AGENTS.md`: who the bot is and how it works. Runs use the bot's folder as their working
  directory, so opencode loads it into every session. The bot rewrites it as it learns.
- `memory/*.md`: everything else it keeps (notes, logs, to-dos, things already handled).
- `files/`: files you uploaded for it to use (documents, data, references).
- `bot.json`: dumBot's record of the bot's model, schedule and runs.
- `.opencode/`: what dumBot writes before every run (from `src/prompts/`):
  - `agents/dumbot.md`: the agent every run uses. Its system prompt is the bot's operating
    loop: journal and update memory after each step, edit `AGENTS.md` the moment it learns
    something, fan independent work out to several subagents at once, review itself, then
    schedule the next run. It may edit its folder and run `dumbot wake` without asking.
  - `agents/dumbot-worker.md`: a subagent for parallel tasks, which writes its results under
    `memory/work/`.
  - `skills/dumbot-evolve/SKILL.md`: how to restructure `AGENTS.md` and write its own skills
    and subagents.

  The bot's own skills and subagents live beside them, and show up in the app.

The folder is a git repository, because opencode only loads a folder's `AGENTS.md` and
`.opencode/` agents in a project.

Every run is a new opencode session. Its first prompt says why the bot woke up and passes on
any note its last run left. Bots schedule themselves with:

```sh
dumbot wake <bot> in 30m                     # one-off: in 2h, in 1d, at 09:00, at 2026-12-01 09:00
dumbot wake <bot> every 6h --note "check the PR queue first"
dumbot wake <bot> on wed,thu at 19:00        # weekly rules add up: mon..sun, weekdays,
dumbot wake <bot> on sat,sun at 10:00,18:00  #   weekends, daily; several times per rule
dumbot wake <bot> off                        # clears everything
dumbot list                                  # every wake-up and its note
dumbot list
dumbot run <bot> [message]                   # start a run now, from a script or cron
```

A bot wakes at the earliest of its one-off, interval and weekly times. A `--note` belongs to
the rule or wake-up it was given with, and the run that wakes gets the notes of what fired. `dumbot wake` writes
`bot.json`; the running app re-reads bots every few seconds and starts the ones that are due.
A bot whose last run is still working (or waiting on its subagents) waits for it to finish.
On this computer, bots only wake while the app is running; times missed while it was closed
run once when it next starts. A server wakes its bots whether or not the app is open.

## Containers

Every run happens in the bot's own Docker container (`dumbot-<slug>`), running
`opencode serve`. Docker is required.

- **Image**: `dumbot-runner:<hash>`, built on your computer the first time a bot runs, so it
  matches your architecture. It has opencode (your host's version), git, gh, browse and
  common tools, plus a Linux `dumbot` command line: downloaded from the release matching the
  app's version, or built from the checkout for development builds. The hash covers the
  Dockerfile (`src/docker/Dockerfile`), dumBot's version (and source, in development), and
  the host's opencode and browse versions, so a change to any of them builds a new image.
- **Mounts**, at the same paths as on the host: the bot's folder, your opencode config
  (models, auth, agents, skills), a file with the bot's environment variables, a persistent
  home of its own (`~/.local/share/dumbot/containers/<slug>/home`, where its opencode sessions
  live), and any folders you add.
- **Pausing**: containers aren't stopped between runs. One with no working run that isn't the
  selected bot is paused (`docker pause`) after a minute, and resumed in an instant for the
  next run or when you select the bot. A container whose image or settings changed is
  recreated on its next run.
- **Opt-in per bot** (**Container…**): tools set up from your computer's own (GitHub CLI,
  logged in with your `gh auth token`; Browse, through the relay on your computer), MCP
  servers from your opencode config (ones that run a local command are flagged: they run
  inside the container), and more folders to mount.
- **Starting** shows each step (checking Docker, building the image, resuming the container,
  waiting for opencode, …) with its time, live in the run panel and the sidebar.
- **Issues**: the sidebar shows problems running bots (Docker missing, a failed image build
  with its log, a container that stopped, a tool that isn't logged in on the host, …).
  `dumbot doctor` prints the same.

## Development

```sh
make run
make test
make lint
cargo test -- --ignored        # also hits the local opencode service and GitHub
DUMBOT_DATA_DIR=/tmp/c cargo test remote_bot -- --ignored   # a bot on a paired server
cargo build --no-default-features   # the command line only, as built into the image
cargo run -- --snap out.png --scene demo --size 1440x900 --theme dark
```

- `src/app.rs`: app state, the scheduler loop and all views.
- `src/bots.rs`: bot folders, `bot.json`, the `dumbot` agent and starting runs.
- `src/schedule.rs`: parsing and describing wake-up times.
- `src/cli.rs`: `dumbot wake`, `run` and `list`.
- `src/container.rs`: the image, bots' containers, pausing, and `dumbot doctor`.
- `src/opencode.rs`: client for opencode's HTTP API (in each bot's container).
- `src/api.rs`: every request the app makes, handled in-process or by a server.
- `src/remote.rs`, `src/server.rs`, `src/tunnel.rs`: pairing and TLS, `dumbot server` and
  its service, and carrying browse back to the app.
- `src/context.rs`: what a server's bots take from the app's computer.
- `src/mcp.rs`: bot-only MCP servers.
- `src/store.rs`: app settings (`~/.local/share/dumbot/state.json`).
- `src/update.rs`, `src/appbundle.rs`: self-update and the macOS app bundle.

## Releasing

Run the **Release new version** workflow (`gh workflow run bump-version.yml -f bump=patch`).
It bumps the version, tags `vX.Y.Z`, and the release workflow builds macOS and Linux
binaries, publishes the GitHub release, and updates the formula in
[vangheem/homebrew-tap](https://github.com/vangheem/homebrew-tap). Every push to `main` also
refreshes a rolling `latest` prerelease. It needs the `RELEASE_DEPLOY_KEY` and
`HOMEBREW_TAP_DEPLOY_KEY` secrets, as gitsnack does.

## License

MIT
