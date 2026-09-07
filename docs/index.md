# atrium documentation

**atrium** is a multi-agent terminal: tmux for coding agents. It runs your CLIs in
switchable, splittable panes, reads each agent's real status so you can see who is
blocked, and lets one human drive a whole bench of agents through a couple of
coordinators, every one a visible, killable pane.

New here? Start with the [README](../README.md) for the overview, install, and
quickstart. This guide is the deeper breakdown, one topic per page.

## Getting started

- [Install](../README.md#install)
- [Quickstart](../README.md#quickstart): the commands and the `Ctrl+A` keys

## Concepts

- [Windows and panes](../README.md#concepts): passthrough vs tiled, splitting, zoom
- [Agent-aware status](agent-status.md): how atrium reads working / waiting / idle, and what it can and cannot see
- [Credential identity](identity.md): running an agent under a vault key or a WIF profile, names-only, never secrets

## Running many agents

- [Fleets and mass-spawn](fleets.md): a grid of identical agents, or a saved roster of named, in-role agents, with the full `atrium.fleet.json` spec

## Coordinating agents

- [The control plane (ctl)](control-plane.md): spawn, send, observe, and kill agent panes; the board and the bus
- [Trust and the security model](trust-and-security.md): the `--trust` ladder, the ceiling, and the seven guarantees that make it safe

## Reference

- [Keys, subcommands, environment variables, and config](../README.md#reference)

## Internals

- [Architecture](architecture.md): the zero-dependency stack, passthrough-first rendering, and the host-level subtleties
- [Orphaned panes and reaping](reaping.md): how atrium tears down process groups and never strands a pty

---

Every layer here is built on the nativelite stack (`pty`, `rawterm`, `ansi`,
`vterm`, `agsess`) with zero third-party dependencies. atrium ships under the MIT
license.
