# Security policy

## Reporting a vulnerability

Please report security problems privately, using GitHub's "Report a vulnerability" button on the repository's Security tab (private vulnerability reporting), rather than a public issue. Include what you found, how to reproduce it, and the version or commit.

davlog is early software maintained by one person, so there is no fixed response time, but reports are read and taken seriously.

## What is in scope

davlog serves a git working tree over WebDAV to one authenticated user. Reports about path handling, access to `.git`, authentication, request handling that can exhaust the server, and data loss are all welcome. The README's "Security and hardening" section describes what the server does and does not defend against.
