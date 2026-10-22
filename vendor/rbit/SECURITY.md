# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in rbit, please report it responsibly.

### How to Report

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, please email the maintainer directly with:

1. A description of the vulnerability
2. Steps to reproduce the issue
3. Potential impact assessment
4. Any suggested fixes (optional)

### What to Expect

- Acknowledgment of your report within 48 hours
- Regular updates on the progress of addressing the issue
- Credit in the security advisory (unless you prefer anonymity)

### Scope

Security issues we're particularly interested in:

- Path traversal vulnerabilities in the storage module
- Denial of service through malformed protocol messages
- Memory safety issues
- Cryptographic weaknesses in hash handling
- Network protocol vulnerabilities

### Out of Scope

- Vulnerabilities in dependencies (report these to the respective projects)
- Issues requiring physical access to the system
- Social engineering attacks

## Security Considerations

rbit is a BitTorrent library that handles untrusted network data. Users should be aware of:

- **File paths**: The storage module validates paths against directory traversal attacks
- **Network input**: Protocol parsers are designed to handle malformed input safely
- **Resource limits**: Consider using appropriate timeouts and limits when integrating

## Disclosure Policy

We follow coordinated disclosure:

1. Reporter notifies maintainer privately
2. Maintainer confirms and works on a fix
3. Fix is released with a security advisory
4. Public disclosure after users have time to update
