#!/bin/bash
# Custom SSHD config for verbose logging
echo 'LogLevel VERBOSE' >> /config/sshd/sshd_config
echo 'SyslogFacility AUTH' >> /config/sshd/sshd_config
echo 'PrintLastLog yes' >> /config/sshd/sshd_config
echo "[custom-init] SSH verbose logging enabled"
