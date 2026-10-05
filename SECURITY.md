# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/fermah-xyz/fermah-pay-stellar/security/advisories/new)
for this repository. Do not open a public issue for a vulnerability.

Include what is affected (the contract, the gateway, the worker, the
observer, the tools or the deployment files), how to reproduce it, and what
an attacker gains. We acknowledge reports within three working days and
keep you informed until a fix is released.

## Scope

- The Soroban contracts in `contracts/`.
- The gateway, settlement worker and chain observer in `crates/`, and the
  tools in `crates/cli`.
- The deployment files in `deploy/`, to the extent that following the
  [self-hosting guides](docs/self-hosting) as written would be unsafe.

The contracts and services are deployed on Stellar testnet only; no
mainnet deployment exists yet. Testnet funds have no value, but a way to move
another party's testnet credit, or to make the system record something the
chain contradicts, is in scope.

What each party can and cannot do is described in the
[threat model](docs/security/threat-model.md).
