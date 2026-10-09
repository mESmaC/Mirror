# 0000: North star

Status: confirmed (2026-10-09)
Ledger: G1-G8c, A1-A17, I1-I6, M0-M9

## What Mirror is

**Mirror is an agent harness that is also a complete, fast, independent operating system.** Humans and AI agents operate the same machine, separately or at the same time. They work through typed operations governed by capabilities and policy, and every agent action lands in an immutable audit log.

Mirror is its own system: its own kernel, shell, tools and inference engine. It is not a Linux fork or a Windows clone. Windows and Linux application compatibility comes later, as layers on top of Mirror.

## Principles

1. **Independent.** Mirror defines its own interfaces and tools. Compatibility layers translate onto Mirror, never the reverse.
2. **The OS is the harness.** Every resource is reached through a capability. Every action humans or agents take on the system is a typed operation. Humans and agents use the same operations.
3. **Flexibility with control.** Agent policy is configurable and tuned by experiment using audit logs. Only core invariants are compiled into the immutable core.
4. **Speed is measured, not claimed.** v1 has explicit performance gates measured against Linux and Windows in the same VM.
5. **Small specs, explicit decisions.** Nothing is built from an unconfirmed decision.

## Operating modes

| Mode | Description | v1 |
| --- | --- | --- |
| Human only | Normal use with no agent active. | Yes |
| Together | Human and agent at the same time, each with their own input seat (cursor, focus, keyboard), never fighting for control. | Yes |
| Agent alone (local) | An agent works unsupervised on a local machine or VM under policy. | Yes |
| Cloud agent desktops | Headless Mirror instances that give agents their own desktops remotely. | Later |

## v1: Cameron's daily driver, in a VM

v1 is done when Cameron uses Mirror daily inside VMware (VirtualBox also supported, QEMU for automated tests) with:

- persistent files (ext2, then ext3), including a mirrored (RAID-1-like) pool across two or more virtual disks
- network and internet access
- Mirror's typed, agent-native shell and native command-line tools
- the agent harness: principals, read-only and full-access presets, grants, human override, input provenance, the immutable audit log, and the Agent API with an MCP adapter
- the built-in assistant (an Agent API client) with an open/close panel, using cloud providers or Mirror's own CPU inference engine (Qwen3 MoE, sharded models, TurboQuant KV cache, CLM context objects with Suffix Cache Reuse)
- an OS semantic index and agent memory, built on TurboQuant vector compression and an embedding model
- the four performance gates passing

## Not in v1

Web browser, dev tools (git and compilers), PLE models, Windows app and Linux binary compatibility, screen/input fallback for agents, GPU, unified capacity tiering, inference-aware scheduling, physical hardware, cloud agent desktops, multiple human accounts, CI, license choice.

## Performance gates

Each is measured against Linux and Windows in the same VM configuration. Target numbers are set after the first baselines are measured (see O3).

1. Boot to desktop
2. Idle memory
3. Input latency (keypress or click to pixels)
4. Agent action latency (read state plus one typed operation, round trip)
