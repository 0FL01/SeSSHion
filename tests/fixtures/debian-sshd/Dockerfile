FROM debian:trixie-slim

# Install OpenSSH server, GNU tar, rsync, and sudo
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        openssh-server \
        openssh-client \
        tar \
        rsync \
        sudo && \
    rm -rf /var/lib/apt/lists/*

# Create test user with password and passwordless sudo
RUN useradd -m -s /bin/bash test && \
    echo "test:secret" | chpasswd && \
    usermod -aG sudo test && \
    printf 'test ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/test && \
    chmod 440 /etc/sudoers.d/test

# Generate SSH host keys
RUN ssh-keygen -A

# Configure sshd
RUN echo "PasswordAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "Port 2222" >> /etc/ssh/sshd_config && \
    echo "UsePAM no" >> /etc/ssh/sshd_config && \
    echo "PermitRootLogin no" >> /etc/ssh/sshd_config && \
    echo "ChallengeResponseAuthentication no" >> /etc/ssh/sshd_config && \
    echo "PubkeyAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "MaxAuthTries 10" >> /etc/ssh/sshd_config

# Set up SSH key authentication for test user
RUN mkdir -p /home/test/.ssh && \
    echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJntvVTUo53qNWwM84VBkVWi3gFo0z7aFPLpGIP9O1z5 ssh-mcp-test" > /home/test/.ssh/authorized_keys && \
    chown -R test:test /home/test/.ssh && \
    chmod 700 /home/test/.ssh && \
    chmod 600 /home/test/.ssh/authorized_keys

# Create runtime directory for sshd
RUN mkdir -p /run/sshd

# Set proper permissions
RUN chmod 755 /run/sshd && \
    chmod 644 /etc/ssh/sshd_config

# Expose SSH port
EXPOSE 2222

# Run sshd in foreground with stderr logging
ENTRYPOINT ["/usr/sbin/sshd", "-D", "-e"]
