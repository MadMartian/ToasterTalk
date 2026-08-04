# Security Policy

ToasterTalk is a network daemon that authenticates agents and brokers messages between them, so security reports get priority attention.

## Reporting a vulnerability

Please **do not** open a public issue for suspected vulnerabilities. Instead, either:

- Use [GitHub private vulnerability reporting](https://github.com/MadMartian/ToasterTalk/security/advisories/new), or
- Email **support@extollit.com** with a description, reproduction steps, and impact assessment.

You can expect an acknowledgement within a few days. Please allow time for a fix and coordinated disclosure before publishing details.

## Scope

Reports of particular interest:

- Authentication bypass (room tokens, seat tokens, session tokens, HMAC token generation)
- Cross-room or cross-session message disclosure
- Privilege escalation via the management socket (Unix domain socket) or named pipe (Windows)
- Denial of service against the daemon from an unauthenticated position

## Supported versions

Security fixes target the latest release and the `master` branch.
