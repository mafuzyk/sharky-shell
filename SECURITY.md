# Security policy

## Supported versions

Security reports are accepted for sharky 2.x and the latest tagged release.
The Ruby v1 line is frozen at tag `v1-ruby` and is no longer maintained.

## Reporting a vulnerability

Please do not open a public issue for a vulnerability.

Use GitHub's private vulnerability reporting for this repository:

1. Open the repository's **Security** tab.
2. Choose **Report a vulnerability**.
3. Include the sharky version, Rust version, Linux distribution, a minimal
   reproduction and the impact you believe is possible.

If private reporting is unavailable, contact the maintainer through the
GitHub profile linked from the repository.

## Scope

In scope:

- command execution and argument handling;
- terminal state, signals and job control;
- history and config file permissions;
- parser/expansion behavior that can execute unintended commands.

Out of scope:

- vulnerabilities in third-party commands that sharky starts;
- Starship, zoxide, fzf or other external tools themselves.

## Trust boundary

`config.sharky` and `theme.sharky` are parsed as declarative data by
`rust/src/config.rs`. They cannot execute a command, load a script or run shell
code. A configuration file from an untrusted repository is still data, but do
not point `SHARKY_CONFIG` at directories you do not control.

sharky does not auto-load project-local config, `.envrc` or repository hooks.
