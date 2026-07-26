# Debian SSHD Docker Image

Lightweight SSH server container for E2E testing based on `debian:trixie-slim`.

## Features

- **Base**: `debian:trixie-slim` (minimal Debian image)
- **Authentication**: Both password and key authentication supported
  - Password: user `test`, password `secret`
  - Key: ED25519 public key pre-configured
- **Port**: 2222 (non-standard to avoid conflicts)
- **Security**: No PAM, no root login
- **Includes**: GNU tar and rsync for file transfer tests

## Build

```bash
docker build -t debian-sshd:latest .
```

## Run

```bash
docker run -d -p 2222:2222 --name sshd-test debian-sshd:latest
```

## Connect

### Password Authentication

```bash
ssh -p 2222 test@localhost
# Password: secret
```

### Key Authentication

A pre-configured ED25519 key is set up in the container. Use the following private key to connect:

```
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCZ7b1U1KOd6jVsDPOFQZFVot4BaNM+2hTy6RiD/Ttc+QAAAJD4/zqo+P86
qAAAAAtzc2gtZWQyNTUxOQAAACCZ7b1U1KOd6jVsDPOFQZFVot4BaNM+2hTy6RiD/Ttc+Q
AAAEDCxgrF63olxn5oZkm+x+wntKjbSB9nWO+mazmilqLU5pntvVTUo53qNWwM84VBkVWi
3gFo0z7aFPLpGIP9O1z5AAAADHNzaC1tY3AtdGVzdAE=
-----END OPENSSH PRIVATE KEY-----
```

Or use the corresponding public key:
```
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJntvVTUo53qNWwM84VBkVWi3gFo0z7aFPLpGIP9O1z5 ssh-mcp-test
```

Save the private key to a file (e.g., `~/.ssh/ssh-mcp-test`) and connect:
```bash
chmod 600 ~/.ssh/ssh-mcp-test
ssh -p 2222 -i ~/.ssh/ssh-mcp-test test@localhost
```

## Cleanup

```bash
docker stop sshd-test
docker rm sshd-test
```

## Configuration

The container runs `sshd` with the following settings:
- `PasswordAuthentication yes`
- `PubkeyAuthentication yes`
- `Port 2222`
- `UsePAM no`
- `PermitRootLogin no`

Host keys are generated at build time. The container runs with a non-root user internally but allows SSH login via the `test` user.
