# ACP Lifecycle Hardening Design

## Goal

Make daemon-hosted ACP sessions safe across concurrent open, kill, restart, and
daemon upgrade operations. Preserve workspace-profile identity and launch
configuration across restarts, support profile paths containing `~` or spaces,
clean up rejected handoff resources, and emit each waiting notification once.

## Lifecycle architecture

`smeltd` will own an `AcpRegistry` that is the only API for ACP session
registration and lifecycle operations. It will replace direct access to the
ACP session map for open, attach, relaunch, kill, list, subscription snapshots,
handoff collection, and handoff restoration.

Each session ID maps to one slot with a lifecycle mutex. Registry insertion uses
the map entry API so concurrent opens atomically select the same slot. The slot
serializes launch, relaunch, attach, and kill decisions for that ID. A session
is published before launch begins, so observers see one stable identity, while
the lifecycle state distinguishes starting, running, ended, and stopping
sessions.

The registry owns a shared spawn/upgrade gate. The code path that actually
creates an ACP child process takes a read permit immediately before spawning.
Daemon upgrade takes the write permit before collecting either ACP or terminal
handoff state and holds it through `exec`, or until rollback completes. This
defines one snapshot boundary: no ACP or terminal child can appear after
handoff collection starts.

## Handoff ownership

Handoff restoration validates inherited descriptors, PID, snapshot, and ACP
session ID before publishing a restored session. Every rejection after resource
ownership transfers to the new daemon goes through one cleanup helper that:

1. Closes both inherited descriptors.
2. Terminates the inherited ACP process group when the PID is valid.
3. Reaps the child when possible without blocking daemon startup indefinitely.

Successful restoration transfers descriptor ownership to the resumed ACP
connection exactly once. Upgrade collection includes only sessions with stable
stdio metadata while holding the upgrade gate. Sessions that cannot be handed
off receive an explicit shutdown rather than becoming unreachable.

## Structured launch configuration

`smelt-core` will define a serializable `AcpLaunchSpec` containing:

- The executable command and arguments.
- Explicit environment overrides.

The structure travels through GUI persistence, the GUI-to-daemon protocol,
`AcpRegistry`, ACP connection startup, and daemon handoff. Workspace profiles
put their config directory in the environment map rather than interpolating it
into a whitespace-tokenized command.

Existing user commands beginning with `VAR=value` remain supported for backward
compatibility. Structured environment values take precedence for profile-owned
settings. Tilde expansion happens once when a path is consumed, not when it is
displayed or persisted.

## Profile restart behavior

`AcpView` and `AcpSaved` retain an optional profile ID alongside the launch
spec. Restart resolves launch configuration in this order:

1. If the profile ID still exists, regenerate the spec from its current
   settings.
2. If the profile was removed, reuse the session's persisted launch spec.
3. For a non-profile session, use the selected agent's current global command.

History listing reads the workspace directory from the structured environment,
expands `~`, and passes the normalized directory to the agent-specific history
reader. Paths containing spaces remain a single environment value.

## Notifications

The daemon state subscription is the sole producer of waiting notifications for
daemon-hosted ACP sessions. Applying an ACP snapshot updates view state only and
does not enqueue approval or elicitation notifications. Phase-transition
deduplication remains centralized in the daemon state consumer.

## Error handling

Lifecycle conflicts and launch failures return explicit failure state to the
client instead of silently replacing a registry entry. Lock poisoning and
resource cleanup failures follow existing daemon logging conventions. Cleanup
does not report success-shaped state when descriptors or processes may remain
owned.

## Tests

Regression coverage will include:

- Concurrent opens of the same ID create one slot and one child launch.
- Open versus kill has deterministic ownership and leaves no orphan process.
- ACP spawn and daemon upgrade cannot cross the handoff snapshot boundary.
- A live session survives a complete upgrade and resumes under the same ID.
- Invalid handoff snapshots close descriptors and terminate inherited agents.
- Profile sessions restart with the same profile, including after GUI restore.
- Removed profiles fall back to the persisted launch spec.
- `~` and spaces in workspace paths work for launch and history discovery.
- Approval and elicitation transitions enqueue one notification each.

Targeted tests for `smelt-core`, `smeltd`, `smelt-acp-view`, and `smelt` will run
before workspace-level `cargo check`.
