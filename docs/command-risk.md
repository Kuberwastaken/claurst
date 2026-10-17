# Command Risk Gate

Claurst screens every Bash tool command through a two-stage risk gate before it runs. The existing risk classifier (`bash_classifier`, `ps_classifier`) stays in place; the gate layers on top of it.

## Stage 1: blast-radius classifier

Stage 1 (`claurst-command-risk`, zero dependencies, pure standard library) classifies a command by what it would destroy and whether that can be undone, not by command name. A denylist of `rm -rf` misses `find -delete`, `shred`, `truncate`, `dd`, and `>file`; the classifier asks about the target instead. It wraps command unwrappers (`sudo`, `env`, `xargs`, `sh -c`) and splits compound lines, so `ls; rm -rf $HOME` cannot hide the second segment in the first.

Verdicts:

- `Safe` / `Low`: no action; the command continues through the normal permission flow. `Low` covers bounded destruction, such as a recursive delete inside the working directory.
- `Confirm`: the blast radius could not be determined statically (unresolved variables, globs, pipe-fed operands). The command escalates to a manual approval even in permission modes that would have auto-approved it, and the approval question carries the findings.
- `Catastrophic`: the command targets a path that must never be destroyed. It is refused outright and no approval mode, allow rule, or justification unlocks it. If the user genuinely wants the command, they must run it themselves outside Claurst.

The catastrophic tier is a small, absolute, path-based deny list compared after lexical normalization, independent of command parsing: `$HOME` itself, `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.docker` and their contents, plus the wholesale destruction of `~/.config`, `~/.claurst`, `~/.claude`, `~/.local`, `~/Documents`, `~/Desktop`, and the system roots (`/`, `/etc`, `/usr`, `/bin`, `/var`, `/Users`, ...). Deleting one stale file inside `~/.config` remains a normal approval; deleting `~/.config` does not.

Stage 1 is deliberately biased toward recall: a false positive costs one approval question, a false negative can cost a home directory.

## Stage 2: reflection gate

The ported gate logic (`claurst_command_risk::gate`) defines what happens after stage 1 flags a command: a `Confirm` verdict is answered with a reflection prompt that forces the model to compare the command against what the user actually asked for and re-issue it with a justification, instead of a bare retry. The reflection wiring into the approval loop is a known open item (see below); stage 1 is the part wired into the Bash tool today.

## Limitations

The classifier is defense in depth, not a sandbox. It is lexical: it never touches the filesystem, so symlinks and computed paths it cannot resolve escalate (`Confirm`) rather than being trusted.