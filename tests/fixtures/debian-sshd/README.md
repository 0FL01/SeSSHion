# Debian SSHD Docker Image

Lightweight SSH server container for E2E testing based on `debian:trixie-slim`.

## Features

- **Base**: `debian:trixie-slim` (minimal Debian image)
- **Authentication**: Password-based (user: `test`, password: `secret`)
- **Port**: 2222 (non-standard to avoid conflicts)
- **Security**: No PAM, no root login, no SSH keys
- **Includes**: GNU tar for file transfer tests

## Build

```bash
docker build -t debian-sshd:latest .
```

## Run

```bash
docker run -d -p 2222:2222 --name sshd-test debian-sshd:latest
```

## Connect

```bash
ssh -p 2222 test@localhost
# Password: secret
```

## Cleanup

```bash
docker stop sshd-test
docker rm sshd-test
```

## Configuration

The container runs `sshd` with the following settings:
- `PasswordAuthentication yes`
- `Port 2222`
- `UsePAM no`
- `PermitRootLogin no`
- `PubkeyAuthentication no`

Host keys are generated at build time. The container runs with a non-root user internally but allows SSH login via the `test` user.
