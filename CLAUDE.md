# vm_llm_notify

KDE tray app + tiny HTTP server. Claude Code hooks (often running inside a VM) POST their JSON
payload to it; the tray icon flashes while any session is waiting for the user.

Everything lives in `src/main.rs`. Keep it that way until it genuinely hurts.

## Build / test

```
cargo test        # unit tests: event handling, config precedence
cargo clippy      # keep clean
cargo run -- --listen 127.0.0.1:9876
curl --data '{"hook_event_name":"Stop","session_id":"x","cwd":"/tmp"}' localhost:9876
```

## Behaviour

- `Stop`, `StopFailure`, and `Notification` whose `notification_type` needs the user
  (`permission_prompt`, `idle_prompt`, `elicitation_dialog`, `elicitation_url_dialog`,
  `agent_needs_input`, or missing) → session marked waiting (icon flashes, status `NeedsAttention`).
- Other notification types (`auth_success`, `agent_completed`, …) → ignored.
- Any other event from that session → cleared. Left-click clears all; menu dismisses one.
- 200 with empty body for an understood event (Claude Code treats that as plain success),
  400 otherwise (a non-blocking hook error). Never return a body: a JSON body would be parsed as a
  hook decision. Body capped at 1 MiB.
- Menu "Copy address for agent" copies `http://<listen>` via Klipper D-Bus (`org.kde.klipper`).

## Hook setup (per https://code.claude.com/docs/en/hooks)

Use native `http` hooks, not curl. Set a short `timeout`: the default is 600 s, and these hooks run
synchronously. `PostToolUse` clears the flash once a permission prompt is approved.

```json
{
  "hooks": {
    "Stop":             [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}],
    "StopFailure":      [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}],
    "Notification":     [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}],
    "PostToolUse":      [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}],
    "SessionEnd":       [{"hooks": [{"type": "http", "url": "http://HOST:9876", "timeout": 2}]}]
  }
}
```

## Config

Precedence: CLI args > `settings.conf.json` > environment > default.

- File: found via `xdg` crate (`$XDG_CONFIG_HOME/vm_llm_notify/`, then `$XDG_CONFIG_DIRS`).
  `deny_unknown_fields`; a malformed file is a fatal error.
- Env: clap's `env` feature (`VM_LLM_NOTIFY_LISTEN`). Because clap would naturally rank env above
  the file, `load_config` checks `ArgMatches::value_source` and only lets `CommandLine` beat the file.
- Adding a setting: add it to `Cli` (with `env =`) and `FileConfig`, resolve it the same way in
  `load_config`, extend the `config_precedence` test.

## Non-obvious bits

- `ksni` uses the `blocking` API but still needs a runtime feature; we use `async-io` (lighter than
  tokio). `zbus` is a direct dep with the same features — don't let a second zbus build creep in.
- HTTP is `tiny_http`: blocking, one thread, no async runtime. Deliberate; don't swap for hyper/axum.
- Clipboard D-Bus call runs on its own thread so it doesn't block ksni's D-Bus dispatch.
- Syslog is RFC 5424 (user requirement) via a small `Syslog5424` `log::Log` adapter, since
  `syslog::init` only does RFC 3164. journald does not parse 5424 on `/dev/log`: the header lands
  in `MESSAGE` and `journalctl -t` won't match — use `journalctl _COMM=vm_llm_notify`.
- The HTTP endpoint is unauthenticated. Anyone who can reach the listen address can flash the icon.

## Commits

Claude has standing authority to commit in this repo without asking. Commit messages must have
**no trailers** — no `Co-Authored-By`, no attribution lines of any kind. Subject + body only.
