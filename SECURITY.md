# Security policy

## Supported versions

Security reports are accepted for sharky 1.x on the maintained `v1` branch
and the latest tagged release.

## Reporting a vulnerability

Please do not open a public issue for a vulnerability.

Use GitHub's private vulnerability reporting for this repository:

1. Open the repository's **Security** tab.
2. Choose **Report a vulnerability**.
3. Include the sharky version, Ruby version, Linux distribution, a minimal
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
- users running untrusted Ruby config or function files (configuration is
  trusted local code by design);
- Starship, zoxide, fzf or other external tools themselves.

## Trust boundary

`sharky.rb`, function files and themes under the sharky config directory are
executed as the user's Ruby code. Never load configuration from an untrusted
repository or another user. sharky does not auto-load project-local config,
`.envrc` or repository hooks.
