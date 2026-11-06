# Security Policy

homing-agent installs and runs code on devices, so security reports are taken seriously.

## Reporting a vulnerability

Please **do not** open a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/trevortomlin/homing-agent/security/advisories/new)
(the "Security" tab, then "Report a vulnerability"), or email **trevortomlin2@outlook.com**.

Please include a description, the affected version or commit, and a reproducer if possible
(for example, the commands and files needed to trigger the issue). You can expect an
acknowledgement within 7 days. Once a fix is available, we will coordinate disclosure with you
and credit you, unless you prefer otherwise.

## Scope

In scope: anything that lets a package be installed or run without a valid signature from a
trusted key, a downgrade below the anti-rollback floor, writes outside the agent's data
directory, unauthorized use of the socket API (for example a non-admin triggering an install,
or an app impersonating another), and failures of crash or power-loss recovery that leave a
device without a working committed version. The known limitations listed in the README (for
example running as root) are already known.

## Supported versions

homing-agent is pre-alpha and has not been audited. Until 1.0, only the latest commit on
`main` (and later the latest release) receives fixes.
