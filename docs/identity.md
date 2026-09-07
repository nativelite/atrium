# Credential identity

atrium can launch an agent pane under a chosen credential identity, so a single grid of agents can run under different keys or federation profiles at once. Only the identity's name is ever shown; the secret behind it is never displayed, cached, logged, or persisted.

## Launching under an identity

Pass `--identity <name>` (short form `-I <name>`) when you launch an agent:

```bash
atrium --identity work claude      # a static key stored in your OS vault
atrium --identity wif:prod claude  # a Workload Identity Federation profile
```

The identity is resolved for that one child and injected into its environment. It never touches atrium's sibling panes, and atrium itself makes no network calls.

## Static key vs `wif:` profile

A `<name>` can name one of two kinds of credential:

- **Static key.** A plain key name (for example `work`) refers to a static key stored in your OS vault.
- **Workload Identity Federation.** A `wif:<name>` name (for example `wif:prod`) refers to a Workload Identity Federation profile.

The two read apart at a glance in the pane's tag: `·wif:prod` for a federation profile versus `·work` for a static key. That visible distinction surfaces the credential-precedence footgun, a leftover static key silently shadowing a WIF profile, so you can catch it.

## How resolution works

atrium resolves the target's environment through the [`akey`](https://github.com/nativelite/akey) crate:

- A key name becomes `ANTHROPIC_API_KEY`.
- A `wif:<name>` profile becomes the five documented federation variables.

The resolved environment is injected into that one child through the pty (`pty::Pty::spawn_with_env`). The credential is set for that agent alone.

## Lifetime and inheritance

The identity applies to the initial agent pane and is inherited by every split and new pane created from it. Identity is fixed for a pane's lifetime, because a running process's environment cannot change after it starts. To re-identify a pane you respawn it, which is an explicit, visible action.

## What is shown: names only

Only the *name* of an identity is ever rendered.

- A `·<name>` tag rides in the pane's top border (for example `2:claude ·work`) and appears next to the window's entry in the bar.
- The tag renders as **colored text**: the identity color is used as the name's foreground, matching the pane border, in both the border and the reverse-video bar. It is never a filled color chip.
- The text is always drawn. Color is a redundant accessibility channel, never the only signal, so the identity is legible without relying on color.

## The hard guarantee: names, never secrets

The resolved environment is re-fetched on every spawn, handed straight to the child, and dropped. It is:

- never cached on the pane,
- never logged,
- never persisted,
- never present in `ATRIUM_DEBUG` traces.

A screen-shared grid leaks no key and no token. It shows only the labels you typed.

## Resolve failure is visible

If an identity cannot be resolved (no such target, or the vault is locked), the reason is flashed in the bar and the agent spawns *without* the credential. You are told what happened rather than running unauthenticated by surprise. atrium never leaves an agent silently unauthenticated.

## Scope

Non-agent panes (shells, editors) and no-identity spawns are unaffected: they use the plain spawn path and carry no tag.

Identity is not limited to a single interactive launch. It also works with [fleets](fleets.md) and with [the control plane](control-plane.md) via `ctl spawn --identity`, so a whole fleet or a remotely spawned pane can carry the same credential guarantees.

See also the [atrium README](../README.md).
