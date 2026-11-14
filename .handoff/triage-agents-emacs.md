# Codex / Claude / Emacs (≈230 s; notices +31 s)
Time sinks: sleep(32) claude spawn heartbeat, sleep(7) claude hooks, sleep(3) emacs messages, 1.2 s attach sleeps in codex_native, tramp test 47 s (unprofiled).
codex_native (57.6 s) mostly tests real Codex App Server behaviour (CONTRACT) or adapter binding races already unit-covered (adapter-codex main.rs ~1072/1111) → opt-in "Codex contract" run on Codex bumps, not default suite.

Workflows (default ≈35 s; +≈25 s opt-in real Codex):
- W1 Codex goblin handles its inbox (notifier fake): message while detached processed, attach/quit detaches, pause + manual inbox next/reply, resume, member message, stop/recreate same thread, adapter dies → stop; + CodexNotices steps ~8–10s
- W3 Codex goblin setup: credentials visible + rotation in owner/member, host config untouched, secret hidden ~3s
- W2 Work in real Codex (opt-in): run native, codex login status/exec, /quit detaches, reattach history, bind ~15s
- W4 Claude goblin handles its inbox: message wakes via monitor, reply, idle, attach/exit, recreate resumes conversation; probe goblin creds/mounts/secret; + ClaudeNotices ~5–6s
- E1 Emacs status buffer: activity, accept/deny, messages thread, resume chooser, close ~6–8s
- E2 terminal Emacs daily workspace + Ghostel run ~7s
- E3 start server from Emacs 1.5s
- E4 /goblins: TRAMP edit+eshell+editor approval+shell detach ~12–15s (edge cases → ERT with stubs; profile first)
DROP: nix-skill tests (covered checks.nix:640), claude_mod (run as Nix check), fake-specific asserts.
New infra: adapter-codex scripted fake App Server in Rust for open/approval/enforce/join/catch_up (main.rs:551-694) + run() refactor; shell logic (claude hook.sh report_variables/monitor_alive/nested guard, codex launcher.nix, mcp.sh) → Nix runCommand checks or port into Rust; manifest asserts in checks.nix (~1387 pattern); confirm daemon unit test for agent_state heartbeat expiry before dropping 32 s sleep.
