FROM debian:trixie-slim

# Install OpenSSH server and GNU tar
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        openssh-server \
        tar && \
    rm -rf /var/lib/apt/lists/*

# Create test user with password
RUN useradd -m -s /bin/bash test && \
    echo "test:secret" | chpasswd

# Generate SSH host keys
RUN ssh-keygen -A

# Configure sshd
RUN echo "PasswordAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "Port 2222" >> /etc/ssh/sshd_config && \
    echo "UsePAM no" >> /etc/ssh/sshd_config && \
    echo "PermitRootLogin no" >> /etc/ssh/sshd_config && \
    echo "ChallengeResponseAuthentication no" >> /etc/ssh/sshd_config && \
    echo "PubkeyAuthentication no" >> /etc/ssh/sshd_config

# Create runtime directory for sshd
RUN mkdir -p /run/sshd

# Set proper permissions
RUN chmod 755 /run/sshd && \
    chmod 644 /etc/ssh/sshd_config

# Expose SSH port
EXPOSE 2222

# Run sshd in foreground with stderr logging
ENTRYPOINT ["/usr/sbin/sshd", "-D", "-e"]
