# Security

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/voilecx/vmjob/security/advisories/new),
not in a public issue. We answer within a week.

In scope: a guest that writes outside its output directory, reaches a network,
gets more than `/dev/kvm` from the runner, or makes the runner or the operator
act beyond a job's bounds; a `VmJob` that gets a pod more than its namespace's
Pod Security allows. [docs/security.md](docs/security.md) describes the model.

Only the latest release is supported.
