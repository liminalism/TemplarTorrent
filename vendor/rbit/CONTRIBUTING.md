# Contributing to rbit

Thank you for your interest in contributing to rbit! This document provides guidelines and information for contributors.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/rbit.git`
3. Create a branch for your changes: `git checkout -b my-feature`

## Development Setup

Ensure you have Rust 1.85 or later installed. Then:

```bash
cargo build              # Build the library
cargo test               # Run all tests
cargo clippy             # Run linter
cargo fmt                # Format code
```

## Making Changes

### Code Style

- Run `cargo fmt` before committing
- Ensure `cargo clippy` passes without warnings
- Follow existing code patterns and naming conventions

### Testing

- Add tests for new functionality
- Ensure all existing tests pass: `cargo test`
- Include doc tests for public API examples

### Documentation

- Document all public APIs with rustdoc comments
- Include examples in documentation where helpful
- Update CHANGELOG.md for notable changes

## Pull Request Process

1. Update documentation if you're changing public APIs
2. Add an entry to CHANGELOG.md under `[Unreleased]`
3. Ensure all tests pass and clippy is clean
4. Submit a pull request with a clear description of your changes

### Commit Messages

- Use clear, descriptive commit messages
- Start with a verb (Add, Fix, Update, Remove, etc.)
- Reference issues when applicable: `Fix #123`

## Reporting Issues

When reporting bugs, please include:

- Rust version (`rustc --version`)
- Operating system
- Steps to reproduce
- Expected vs actual behavior

## BEP Implementations

When implementing new BEP specifications:

- Reference the official BEP document
- Add the BEP number to the module documentation
- Include comprehensive tests for protocol compliance
- Update the BEP support table in README.md

## License

By contributing to rbit, you agree that your contributions will be licensed under the MIT OR Apache-2.0 license.
